//! Synchronous AT keyboard controller, set-1/set-2 keyboard and three-button
//! PS/2 mouse. Device queues wait behind inhibited links; controller replies
//! remain pollable, while keyboard inhibition also suppresses IRQ1. Frontend
//! events provide key repeats and mouse samples, so no serial-bit/typematic
//! timer is needed. Set 3 and vendor/wheel commands are not claimed.
//!
//! Study inputs: tiny386 i8042.c first, then v86 ps2.js; IBM's Mouse technical
//! reference resolves status/scaling/remote/RESEND omissions. These define
//! observed contracts, not implementation templates. v86 differs: inhibition
//! holds no queued bytes or keyboard input, scan-set writes are ignored, mouse
//! status returns a packet, and remote mode, RESEND and 2:1 scaling are absent.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::devices::DeviceEffect;

const STATUS_OBF: u8 = 0x01;
const STATUS_IBF: u8 = 0x02;
const STATUS_SELFTEST: u8 = 0x04;
const STATUS_CMD: u8 = 0x08;
const STATUS_UNLOCKED: u8 = 0x10;
const STATUS_MOUSE_OBF: u8 = 0x20;

const MODE_KBD_INT: u8 = 0x01;
const MODE_MOUSE_INT: u8 = 0x02;
const MODE_SYS: u8 = 0x04;
const MODE_DISABLE_KBD: u8 = 0x10;
const MODE_DISABLE_MOUSE: u8 = 0x20;
const MODE_TRANSLATE: u8 = 0x40;

const ACK: u8 = 0xfa;
const RESEND: u8 = 0xfe;
const QUEUE_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ps2Key {
    Enter,
    Space,
    Backspace,
    Tab,
    Escape,
    LeftShift,
    RightShift,
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    Num0,
    Num1,
    Num2,
    Num3,
    Num4,
    Num5,
    Num6,
    Num7,
    Num8,
    Num9,
    Minus,
    Equal,
    LeftBracket,
    RightBracket,
    Backslash,
    Semicolon,
    Apostrophe,
    Grave,
    Comma,
    Dot,
    Slash,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Ps2ButtonState {
    pub left: bool,
    pub right: bool,
    pub middle: bool,
}

#[derive(Clone, Debug)]
pub struct I8042 {
    status_base: u8,
    mode: u8,
    write_cmd: u8,
    output: Option<OutputByte>,
    last_output: u8,
    translator: Set2Translator,
    keyboard: Ps2Keyboard,
    mouse: Ps2Mouse,
    effects: Vec<DeviceEffect>,
    irq1_level: bool,
    irq12_level: bool,
    a20: bool,
    reset_requests: u64,
}

impl I8042 {
    pub fn new() -> Self {
        let mut dev = Self {
            status_base: STATUS_SELFTEST | STATUS_CMD | STATUS_UNLOCKED,
            mode: MODE_KBD_INT | MODE_MOUSE_INT | MODE_SYS | MODE_TRANSLATE,
            write_cmd: 0,
            output: None,
            last_output: 0,
            translator: Set2Translator::new(),
            keyboard: Ps2Keyboard::new(),
            mouse: Ps2Mouse::new(),
            effects: Vec::new(),
            irq1_level: false,
            irq12_level: false,
            a20: true,
            reset_requests: 0,
        };
        dev.refresh_output();
        dev
    }

    pub fn pio_read(&mut self, port: u16) -> Option<u8> {
        match port {
            0x60 => Some(self.read_data()),
            0x64 => Some(self.status()),
            _ => None,
        }
    }

    pub fn pio_write(&mut self, port: u16, value: u8) -> bool {
        match port {
            0x60 => {
                self.write_data(value);
                true
            }
            0x64 => {
                self.write_command(value);
                true
            }
            _ => false,
        }
    }

    pub fn irq_level(&self, line: u8) -> bool {
        match line {
            1 => self.irq1_level,
            12 => self.irq12_level,
            _ => false,
        }
    }

    pub fn reset_requests(&self) -> u64 {
        self.reset_requests
    }

    pub fn keyboard_dropped(&self) -> u64 {
        self.keyboard.dropped
    }

    pub fn mouse_dropped(&self) -> u64 {
        self.mouse.dropped
    }

    /// Bulk text frontends wait for the previous character to leave the device,
    /// rather than overfilling its bounded queue. Physical key events still use
    /// inject_key directly. A disabled keyboard or command exchange must finish
    /// before a new character can be admitted; mouse traffic need not drain.
    pub fn keyboard_ready_for_text(&self) -> bool {
        self.keyboard.scanning
            && self.mode & MODE_DISABLE_KBD == 0
            && self.write_cmd == 0
            && self.keyboard.expecting_param.is_none()
            && self.keyboard.queue.is_empty()
            && !self.output.is_some_and(|b| b.source == Source::Keyboard)
    }

    pub fn inject_scancode_set2(&mut self, bytes: &[u8]) {
        // Frontends always supply set 2. Scanning belongs to the keyboard;
        // link inhibition and IRQ masking belong to the controller below.
        if !self.keyboard.scanning {
            return;
        }
        let mut encoded = Vec::new();
        for &byte in bytes {
            if self.keyboard.scan_set == 1 {
                encoded.extend(self.keyboard.set1_encoder.translate(byte));
            } else {
                encoded.push(byte);
            }
        }
        if self.mode & MODE_TRANSLATE != 0 {
            let mut translated = Vec::new();
            for byte in encoded {
                translated.extend(self.translator.translate(byte));
            }
            self.keyboard.queue_bytes(&translated);
        } else {
            self.keyboard.queue_bytes(&encoded);
        }
        // One frontend key event is enqueued whole, including E0/F0 prefixes.
        self.recompute_irqs();
    }

    pub fn inject_key(&mut self, key: Ps2Key, down: bool) {
        let seq = key.set2_sequence(down);
        self.inject_scancode_set2(&seq);
    }

    pub fn inject_ascii(&mut self, ch: u8) {
        let Some((key, shift)) = ascii_to_key(ch) else {
            return;
        };
        if shift {
            self.inject_key(Ps2Key::LeftShift, true);
        }
        self.inject_key(key, true);
        self.inject_key(key, false);
        if shift {
            self.inject_key(Ps2Key::LeftShift, false);
        }
    }

    pub fn inject_mouse(&mut self, dx: i16, dy_screen: i16, buttons: Ps2ButtonState) {
        self.mouse.inject(dx, dy_screen.saturating_neg(), buttons);
        self.refresh_output();
        self.recompute_irqs();
    }

    pub fn drain_effects(&mut self, sink: &mut impl FnMut(DeviceEffect)) {
        for effect in self.effects.drain(..) {
            sink(effect);
        }
    }

    fn status(&mut self) -> u8 {
        self.refresh_output();
        let mut status = self.status_base & !(STATUS_OBF | STATUS_MOUSE_OBF | STATUS_IBF);
        if let Some(output) = self.output {
            status |= STATUS_OBF;
            if output.source == Source::Aux {
                status |= STATUS_MOUSE_OBF;
            }
        }
        status
    }

    fn read_data(&mut self) -> u8 {
        self.refresh_output();
        if let Some(byte) = self.output.take() {
            self.last_output = byte.value;
        }
        // Reading port 60 clears OBF and lowers its IRQ before another queued
        // byte fills the output buffer. Keeping the line high across bytes
        // loses the next request at the edge-triggered 8259, e.g. after an E0
        // keyboard prefix or the first byte of a mouse packet (v86 ps2.js
        // port60_read also lowers the IRQ first).
        self.drive_irqs(false, false);
        self.recompute_irqs();
        self.last_output
    }

    fn write_data(&mut self, value: u8) {
        self.status_base &= !STATUS_CMD;
        let cmd = core::mem::take(&mut self.write_cmd);
        match cmd {
            0x00 => {
                // A host device command reopens the inhibited link, as in
                // QEMU and Bochs; its reply must be able to reach OBF. v86
                // ps2.js leaves the inhibit bits set.
                self.mode &= !MODE_DISABLE_KBD;
                self.keyboard_command(value);
            }
            0x60 => {
                if (self.mode ^ value) & MODE_TRANSLATE != 0 {
                    self.translator = Set2Translator::new();
                }
                self.mode = value;
                self.status_base = (self.status_base & !STATUS_SELFTEST) | (value & MODE_SYS);
                self.recompute_irqs();
            }
            0xd1 => {
                self.a20 = (value & 0x02) != 0;
                self.effects.push(DeviceEffect::A20(self.a20));
                if (value & 0x01) == 0 {
                    self.effects.push(DeviceEffect::ResetRequest);
                    self.reset_requests = self.reset_requests.wrapping_add(1);
                }
            }
            0xd2 => self.controller_reply(value, Source::Keyboard),
            0xd3 => self.controller_reply(value, Source::Aux),
            0xd4 => {
                self.mode &= !MODE_DISABLE_MOUSE;
                self.mouse_command(value);
            }
            _ => {}
        }
        self.refresh_output();
        self.recompute_irqs();
    }

    fn write_command(&mut self, value: u8) {
        self.status_base |= STATUS_CMD;
        self.write_cmd = 0; // A new controller command supersedes its old parameter.
        match value {
            0x20 => self.controller_reply(self.mode, Source::Keyboard),
            0x60 | 0xd1 | 0xd2 | 0xd3 | 0xd4 => self.write_cmd = value,
            0xa7 => {
                self.mode |= MODE_DISABLE_MOUSE;
                self.recompute_irqs();
            }
            0xa8 => {
                self.mode &= !MODE_DISABLE_MOUSE;
                self.recompute_irqs();
            }
            0xa9 => self.controller_reply(0x00, Source::Keyboard),
            0xaa => {
                self.status_base |= STATUS_SELFTEST;
                self.mode |= MODE_SYS;
                self.controller_reply(0x55, Source::Keyboard);
            }
            0xab => self.controller_reply(0x00, Source::Keyboard),
            0xad => {
                self.mode |= MODE_DISABLE_KBD;
                self.recompute_irqs();
            }
            0xae => {
                self.mode &= !MODE_DISABLE_KBD;
                self.recompute_irqs();
            }
            0xc0 => self.controller_reply(0x00, Source::Keyboard),
            0xd0 => {
                let mut out = 0x01 | if self.a20 { 0x02 } else { 0 };
                if self.output.is_some() {
                    out |= 0x10;
                }
                if self
                    .output
                    .map(|b| b.source == Source::Aux)
                    .unwrap_or(false)
                {
                    out |= 0x20;
                }
                self.controller_reply(out, Source::Keyboard);
            }
            0xdd => {
                self.a20 = false;
                self.effects.push(DeviceEffect::A20(false));
            }
            0xdf => {
                self.a20 = true;
                self.effects.push(DeviceEffect::A20(true));
            }
            0xfe => {
                self.effects.push(DeviceEffect::ResetRequest);
                self.reset_requests = self.reset_requests.wrapping_add(1);
            }
            0xff => {}
            _ => {}
        }
        self.refresh_output();
        self.recompute_irqs();
    }

    fn keyboard_command(&mut self, value: u8) {
        if let Some(param) = self.keyboard.expecting_param.take() {
            match param {
                KeyboardParam::ScanSet => match value {
                    0 => {
                        let set = self.keyboard.scan_set;
                        let reply = if self.mode & MODE_TRANSLATE != 0 {
                            translate_set2_byte(set).unwrap_or(set)
                        } else {
                            set
                        };
                        self.keyboard.queue_bytes(&[ACK, reply]);
                    }
                    1 | 2 => {
                        self.keyboard.scan_set = value;
                        self.keyboard.set1_encoder = Set2Translator::new();
                        self.translator = Set2Translator::new();
                        self.queue_keyboard_raw(ACK);
                    }
                    _ => self.queue_keyboard_raw(RESEND),
                },
                // LEDs have no host indicator, and repeat events come from the
                // frontend. Accept their parameters without another scheduler.
                KeyboardParam::Leds | KeyboardParam::Typematic => self.queue_keyboard_raw(ACK),
            }
            return;
        }
        match value {
            0xed => {
                self.queue_keyboard_raw(ACK);
                self.keyboard.expecting_param = Some(KeyboardParam::Leds);
            }
            0xee => self.queue_keyboard_raw(0xee),
            0xf0 => {
                self.queue_keyboard_raw(ACK);
                self.keyboard.expecting_param = Some(KeyboardParam::ScanSet);
            }
            0xf2 => {
                let id = if self.mode & MODE_TRANSLATE != 0 {
                    0x41
                } else {
                    0x83
                };
                self.keyboard.queue_bytes(&[ACK, 0xab, id]);
            }
            0xf3 => {
                self.queue_keyboard_raw(ACK);
                self.keyboard.expecting_param = Some(KeyboardParam::Typematic);
            }
            0xf4 => {
                self.keyboard.scanning = true;
                self.queue_keyboard_raw(ACK);
            }
            0xf5 | 0xf6 | 0xff => {
                self.keyboard.reset_defaults();
                self.translator = Set2Translator::new();
                self.keyboard.scanning = value != 0xf5;
                if value == 0xff {
                    self.keyboard.queue_bytes(&[ACK, 0xaa]);
                } else {
                    self.queue_keyboard_raw(ACK);
                }
            }
            0xfe => self.queue_keyboard_raw(self.keyboard.last_byte),
            0x00 => self.queue_keyboard_raw(ACK), // Historical keyboard probe.
            _ => self.queue_keyboard_raw(RESEND),
        }
    }

    fn mouse_command(&mut self, value: u8) {
        if self.mouse.wrapped && !matches!(value, 0xec | 0xff) {
            self.queue_aux_raw(value);
            return;
        }
        if let Some(param) = self.mouse.expecting_param.take() {
            let valid = match param {
                MouseParam::Resolution => value <= 3,
                MouseParam::SampleRate => matches!(value, 10 | 20 | 40 | 60 | 80 | 100 | 200),
            };
            if valid {
                match param {
                    MouseParam::Resolution => self.mouse.resolution = value,
                    MouseParam::SampleRate => self.mouse.sample_rate = value,
                }
            }
            self.queue_aux_raw(if valid { ACK } else { RESEND });
            return;
        }
        // IBM mouse commands clear accumulated counts, except RESEND; READ
        // DATA first reports them. Stream timing remains frontend-driven.
        let delta = (self.mouse.pending_dx, self.mouse.pending_dy);
        if value != 0xfe {
            self.mouse.pending_dx = 0;
            self.mouse.pending_dy = 0;
        }
        match value {
            0xe6 => {
                self.mouse.scaling_2_to_1 = false;
                self.queue_aux_raw(ACK);
            }
            0xe7 => {
                self.mouse.scaling_2_to_1 = true;
                self.queue_aux_raw(ACK);
            }
            0xe8 => {
                self.queue_aux_raw(ACK);
                self.mouse.expecting_param = Some(MouseParam::Resolution);
            }
            0xe9 => {
                self.mouse.reply(&[
                    ACK,
                    self.mouse.status_byte(),
                    self.mouse.resolution,
                    self.mouse.sample_rate,
                ]);
            }
            0xea => {
                self.mouse.remote = false;
                self.queue_aux_raw(ACK);
            }
            0xeb => {
                let [first, dx, dy] = self.mouse.packet(delta.0, delta.1);
                self.mouse.reply(&[ACK, first, dx, dy]);
            }
            0xec => {
                self.mouse.wrapped = false;
                self.queue_aux_raw(ACK);
            }
            0xee => {
                self.mouse.wrapped = true;
                self.mouse.enabled = false;
                self.queue_aux_raw(ACK);
            }
            0xf0 => {
                self.mouse.remote = true;
                self.queue_aux_raw(ACK);
            }
            0xf2 => {
                self.mouse.reply(&[ACK, 0x00]); // Standard three-button mouse only.
            }
            0xf3 => {
                self.queue_aux_raw(ACK);
                self.mouse.expecting_param = Some(MouseParam::SampleRate);
            }
            0xf4 => {
                self.mouse.enabled = true;
                self.queue_aux_raw(ACK);
            }
            0xf5 => {
                self.mouse.enabled = false;
                self.queue_aux_raw(ACK);
            }
            0xf6 => {
                self.mouse.reset_defaults();
                self.queue_aux_raw(ACK);
            }
            0xff => {
                self.mouse.reset_defaults();
                self.mouse.reply(&[ACK, 0xaa, 0x00]);
            }
            0xfe => {
                let packet = self.mouse.last_packet;
                let len = self.mouse.last_packet_len;
                self.mouse.queue_packet(&packet[..len]);
            }
            _ => self.queue_aux_raw(RESEND),
        }
    }

    fn queue_keyboard_raw(&mut self, value: u8) {
        self.keyboard.queue_bytes(&[value]);
    }

    fn queue_aux_raw(&mut self, value: u8) {
        self.mouse.reply(&[value]);
    }

    fn controller_reply(&mut self, value: u8, source: Source) {
        // Controller-generated data bypasses the inhibited device links.
        // The host must consume OBF before requesting another controller
        // reply. Preserve an unread byte if it violates that handshake.
        if self.output.is_none() {
            self.output = Some(OutputByte { value, source });
        }
    }

    fn refresh_output(&mut self) {
        if self.output.is_some() {
            return;
        }
        if self.mode & MODE_DISABLE_KBD == 0
            && let Some(value) = self.keyboard.pop()
        {
            self.output = Some(OutputByte {
                value,
                source: Source::Keyboard,
            });
            return;
        }
        if self.mode & MODE_DISABLE_MOUSE == 0
            && let Some(value) = self.mouse.pop()
        {
            self.output = Some(OutputByte {
                value,
                source: Source::Aux,
            });
        }
    }

    fn recompute_irqs(&mut self) {
        self.refresh_output();
        let mut irq1 = false;
        let mut irq12 = false;
        if let Some(output) = self.output {
            match output.source {
                Source::Keyboard => {
                    // tiny386/QEMU's synchronous compatibility rule: keep OBF
                    // pollable while inhibited without latching a stale IRQ1.
                    // Bochs instead delays IRQ assertion, so polling can clear
                    // its pending request before delivery.
                    irq1 = self.mode & (MODE_KBD_INT | MODE_DISABLE_KBD) == MODE_KBD_INT;
                }
                Source::Aux => {
                    irq12 = (self.mode & MODE_MOUSE_INT) != 0;
                }
            }
        }
        self.drive_irqs(irq1, irq12);
    }

    fn drive_irqs(&mut self, irq1: bool, irq12: bool) {
        if irq1 != self.irq1_level {
            self.irq1_level = irq1;
            self.effects.push(DeviceEffect::IrqLevel {
                line: 1,
                level: irq1,
            });
        }
        if irq12 != self.irq12_level {
            self.irq12_level = irq12;
            self.effects.push(DeviceEffect::IrqLevel {
                line: 12,
                level: irq12,
            });
        }
    }
}

impl Default for I8042 {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OutputByte {
    value: u8,
    source: Source,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Source {
    Keyboard,
    Aux,
}

// Keep each key event or mouse packet whole under backpressure. Byte-at-a-time
// dropping can turn the next packet's header into the previous one's payload.
fn enqueue_input(queue: &mut VecDeque<u8>, dropped: &mut u64, bytes: &[u8]) -> bool {
    if bytes.len() > QUEUE_CAPACITY - queue.len() {
        *dropped = dropped.wrapping_add(bytes.len() as u64);
        return false;
    }
    queue.extend(bytes.iter().copied());
    true
}

#[derive(Clone, Copy, Debug)]
enum KeyboardParam {
    Leds,
    Typematic,
    ScanSet,
}

#[derive(Clone, Debug)]
struct Ps2Keyboard {
    queue: VecDeque<u8>,
    dropped: u64,
    scanning: bool,
    expecting_param: Option<KeyboardParam>,
    scan_set: u8,
    set1_encoder: Set2Translator,
    last_byte: u8,
}

impl Ps2Keyboard {
    fn new() -> Self {
        Self {
            queue: VecDeque::with_capacity(QUEUE_CAPACITY),
            dropped: 0,
            scanning: true,
            expecting_param: None,
            scan_set: 2,
            set1_encoder: Set2Translator::new(),
            last_byte: RESEND,
        }
    }

    fn reset_defaults(&mut self) {
        self.queue.clear();
        self.expecting_param = None;
        self.scan_set = 2;
        self.set1_encoder = Set2Translator::new();
    }

    fn queue_bytes(&mut self, bytes: &[u8]) {
        enqueue_input(&mut self.queue, &mut self.dropped, bytes);
    }

    fn pop(&mut self) -> Option<u8> {
        let byte = self.queue.pop_front()?;
        if byte != RESEND {
            self.last_byte = byte;
        }
        Some(byte)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MouseParam {
    Resolution,
    SampleRate,
}

#[derive(Clone, Debug)]
struct MousePacket {
    bytes: [u8; 3],
    len: usize,
}

#[derive(Clone, Debug)]
struct Ps2Mouse {
    // Retain packet boundaries for overflow and RESEND. IRQ/OBF still consume
    // one byte at a time; queued future motion is not a transmitted packet.
    queue: VecDeque<MousePacket>,
    queued_bytes: usize,
    packet_pos: usize,
    dropped: u64,
    enabled: bool,
    remote: bool,
    wrapped: bool,
    sample_rate: u8,
    resolution: u8,
    scaling_2_to_1: bool,
    buttons: Ps2ButtonState,
    expecting_param: Option<MouseParam>,
    pending_dx: i32,
    pending_dy: i32,
    last_packet: [u8; 3],
    last_packet_len: usize,
}

impl Ps2Mouse {
    fn new() -> Self {
        Self {
            queue: VecDeque::with_capacity(QUEUE_CAPACITY / 3),
            queued_bytes: 0,
            packet_pos: 0,
            dropped: 0,
            enabled: false,
            remote: false,
            wrapped: false,
            sample_rate: 100,
            resolution: 2,
            scaling_2_to_1: false,
            buttons: Ps2ButtonState::default(),
            expecting_param: None,
            pending_dx: 0,
            pending_dy: 0,
            last_packet: [RESEND, 0, 0],
            last_packet_len: 1,
        }
    }

    fn reset_defaults(&mut self) {
        self.enabled = false;
        self.remote = false;
        self.wrapped = false;
        self.sample_rate = 100;
        self.resolution = 2;
        self.scaling_2_to_1 = false;
        // Commands cannot release the user's physically held buttons.
        self.expecting_param = None;
        self.pending_dx = 0;
        self.pending_dy = 0;
        self.queue.clear();
        self.queued_bytes = 0;
        self.packet_pos = 0;
    }

    fn reply(&mut self, bytes: &[u8]) {
        if !self.reserve_bytes(bytes.len()) {
            return;
        }
        // ACK is a separate packet from the following ID/status/data.
        if bytes.len() > 1 && bytes[0] == ACK {
            self.push_packet(&[ACK]);
            self.push_packet(&bytes[1..]);
        } else {
            self.push_packet(bytes);
        }
    }

    fn reserve_bytes(&mut self, len: usize) -> bool {
        if len > QUEUE_CAPACITY - self.queued_bytes {
            self.dropped = self.dropped.wrapping_add(len as u64);
            return false;
        }
        true
    }

    fn push_packet(&mut self, bytes: &[u8]) {
        let mut packet = MousePacket {
            bytes: [0; 3],
            len: bytes.len(),
        };
        packet.bytes[..bytes.len()].copy_from_slice(bytes);
        self.queued_bytes += bytes.len();
        self.queue.push_back(packet);
    }

    fn queue_packet(&mut self, bytes: &[u8]) {
        if self.reserve_bytes(bytes.len()) {
            self.push_packet(bytes);
        }
    }

    fn pop(&mut self) -> Option<u8> {
        let packet = self.queue.front()?;
        if self.packet_pos == 0 && !(packet.len == 1 && packet.bytes[0] == RESEND) {
            self.last_packet = packet.bytes;
            self.last_packet_len = packet.len;
        }
        let byte = packet.bytes[self.packet_pos];
        self.packet_pos += 1;
        self.queued_bytes -= 1;
        if self.packet_pos == packet.len {
            self.queue.pop_front();
            self.packet_pos = 0;
        }
        Some(byte)
    }

    fn status_byte(&self) -> u8 {
        (if self.remote { 0x40 } else { 0 })
            | (if self.enabled { 0x20 } else { 0 })
            | (if self.scaling_2_to_1 { 0x10 } else { 0 })
            // E9 status uses L/M/R in bits 2/1/0, unlike a movement packet.
            | (u8::from(self.buttons.left) << 2)
            | (u8::from(self.buttons.middle) << 1)
            | u8::from(self.buttons.right)
    }

    fn inject(&mut self, dx: i16, dy_ps2: i16, buttons: Ps2ButtonState) {
        let buttons_changed = self.buttons != buttons;
        self.buttons = buttons;
        if self.wrapped {
            return;
        }
        if self.remote {
            self.pending_dx = self.pending_dx.saturating_add(i32::from(dx));
            self.pending_dy = self.pending_dy.saturating_add(i32::from(dy_ps2));
            return;
        }
        if !self.enabled || (dx == 0 && dy_ps2 == 0 && !buttons_changed) {
            return;
        }
        let mut rem_x = i32::from(dx);
        let mut rem_y = i32::from(dy_ps2);
        // Preserve large frontend deltas using ordinary complete packets.
        // Scaled chunks must still fit the signed nine-bit wire counts.
        let limit = if self.scaling_2_to_1 { 127 } else { 255 };
        loop {
            let step_x = rem_x.clamp(-limit, limit);
            let step_y = rem_y.clamp(-limit, limit);
            let (wire_x, wire_y) = if self.scaling_2_to_1 {
                (scale_mouse_count(step_x), scale_mouse_count(step_y))
            } else {
                (step_x, step_y)
            };
            let packet = self.packet(wire_x, wire_y);
            self.queue_packet(&packet);
            rem_x -= step_x;
            rem_y -= step_y;
            if rem_x == 0 && rem_y == 0 {
                break;
            }
        }
    }

    fn packet(&self, dx: i32, dy: i32) -> [u8; 3] {
        let first = 0x08
            | button_bits(self.buttons)
            | (if !(-256..=255).contains(&dx) { 0x40 } else { 0 })
            | (if !(-256..=255).contains(&dy) { 0x80 } else { 0 })
            | (if dx < 0 { 0x10 } else { 0 })
            | (if dy < 0 { 0x20 } else { 0 });
        [first, dx.clamp(-256, 255) as u8, dy.clamp(-256, 255) as u8]
    }
}

fn scale_mouse_count(count: i32) -> i32 {
    // IBM's 2:1 curve is nonlinear for small movements. Only automatic stream
    // reports use it; READ DATA returns unscaled counters even in stream mode.
    let scaled = match count.abs() {
        0 => 0,
        1 | 2 => 1,
        3 => 3,
        4 => 6,
        5 => 9,
        n => n * 2,
    };
    scaled * count.signum()
}

fn button_bits(buttons: Ps2ButtonState) -> u8 {
    u8::from(buttons.left) | (u8::from(buttons.right) << 1) | (u8::from(buttons.middle) << 2)
}

#[derive(Clone, Copy, Debug)]
struct Set2Translator {
    break_next: bool,
    e0_next: bool,
}

impl Set2Translator {
    const fn new() -> Self {
        Self {
            break_next: false,
            e0_next: false,
        }
    }

    fn translate(&mut self, byte: u8) -> Vec<u8> {
        if byte == 0xf0 {
            self.break_next = true;
            return Vec::new();
        }
        if byte == 0xe0 {
            self.e0_next = true;
            return Vec::new();
        }
        let translated = translate_set2_byte(byte).unwrap_or(byte);
        let break_next = self.break_next;
        let e0_next = self.e0_next;
        self.break_next = false;
        self.e0_next = false;
        let mut out = Vec::new();
        if e0_next {
            out.push(0xe0);
        }
        out.push(if break_next {
            translated | 0x80
        } else {
            translated
        });
        out
    }
}

fn translate_set2_byte(byte: u8) -> Option<u8> {
    // E0 prefixes do not select a different translation table. The old separate
    // table omitted extended Home and Print Screen despite frontend support.
    match byte {
        0x01 => Some(0x43),
        0x02 => Some(0x41),
        0x03 => Some(0x3f),
        0x04 => Some(0x3d),
        0x05 => Some(0x3b),
        0x06 => Some(0x3c),
        0x07 => Some(0x58),
        0x09 => Some(0x44),
        0x0a => Some(0x42),
        0x0b => Some(0x40),
        0x0c => Some(0x3e),
        0x0d => Some(0x0f),
        0x0e => Some(0x29),
        0x11 => Some(0x38),
        0x12 => Some(0x2a),
        0x14 => Some(0x1d),
        0x15 => Some(0x10),
        0x16 => Some(0x02),
        0x1a => Some(0x2c),
        0x1b => Some(0x1f),
        0x1c => Some(0x1e),
        0x1d => Some(0x11),
        0x1e => Some(0x03),
        0x1f => Some(0x5b),
        0x21 => Some(0x2e),
        0x22 => Some(0x2d),
        0x23 => Some(0x20),
        0x24 => Some(0x12),
        0x25 => Some(0x05),
        0x26 => Some(0x04),
        0x27 => Some(0x5c),
        0x29 => Some(0x39),
        0x2a => Some(0x2f),
        0x2b => Some(0x21),
        0x2c => Some(0x14),
        0x2d => Some(0x13),
        0x2e => Some(0x06),
        0x2f => Some(0x5d),
        0x31 => Some(0x31),
        0x32 => Some(0x30),
        0x33 => Some(0x23),
        0x34 => Some(0x22),
        0x35 => Some(0x15),
        0x36 => Some(0x07),
        0x3a => Some(0x32),
        0x3b => Some(0x24),
        0x3c => Some(0x16),
        0x3d => Some(0x08),
        0x3e => Some(0x09),
        0x41 => Some(0x33),
        0x42 => Some(0x25),
        0x43 => Some(0x17),
        0x44 => Some(0x18),
        0x45 => Some(0x0b),
        0x46 => Some(0x0a),
        0x49 => Some(0x34),
        0x4a => Some(0x35),
        0x4b => Some(0x26),
        0x4c => Some(0x27),
        0x4d => Some(0x19),
        0x4e => Some(0x0c),
        0x52 => Some(0x28),
        0x54 => Some(0x1a),
        0x55 => Some(0x0d),
        0x58 => Some(0x3a),
        0x59 => Some(0x36),
        0x5a => Some(0x1c),
        0x5b => Some(0x1b),
        0x5d => Some(0x2b),
        0x66 => Some(0x0e),
        0x69 => Some(0x4f),
        0x6b => Some(0x4b),
        0x6c => Some(0x47),
        0x70 => Some(0x52),
        0x71 => Some(0x53),
        0x72 => Some(0x50),
        0x73 => Some(0x4c),
        0x74 => Some(0x4d),
        0x75 => Some(0x48),
        0x76 => Some(0x01),
        0x77 => Some(0x45),
        0x79 => Some(0x4e),
        0x7a => Some(0x51),
        0x7b => Some(0x4a),
        0x7c => Some(0x37),
        0x7d => Some(0x49),
        0x7e => Some(0x46),
        0x83 => Some(0x41),
        _ => None,
    }
}

impl Ps2Key {
    fn set2_sequence(self, down: bool) -> Vec<u8> {
        match (self, down) {
            (Self::Enter, true) => alloc::vec![0x5a],
            (Self::Enter, false) => alloc::vec![0xf0, 0x5a],
            (Self::Space, true) => alloc::vec![0x29],
            (Self::Space, false) => alloc::vec![0xf0, 0x29],
            (Self::Backspace, true) => alloc::vec![0x66],
            (Self::Backspace, false) => alloc::vec![0xf0, 0x66],
            (Self::Tab, true) => alloc::vec![0x0d],
            (Self::Tab, false) => alloc::vec![0xf0, 0x0d],
            (Self::Escape, true) => alloc::vec![0x76],
            (Self::Escape, false) => alloc::vec![0xf0, 0x76],
            (Self::LeftShift, true) => alloc::vec![0x12],
            (Self::LeftShift, false) => alloc::vec![0xf0, 0x12],
            (Self::RightShift, true) => alloc::vec![0x59],
            (Self::RightShift, false) => alloc::vec![0xf0, 0x59],
            (key, true) => alloc::vec![key.set2_make()],
            (key, false) => {
                let make = key.set2_make();
                if make == 0 {
                    Vec::new()
                } else {
                    alloc::vec![0xf0, make]
                }
            }
        }
    }

    fn set2_make(self) -> u8 {
        match self {
            Self::A => 0x1c,
            Self::B => 0x32,
            Self::C => 0x21,
            Self::D => 0x23,
            Self::E => 0x24,
            Self::F => 0x2b,
            Self::G => 0x34,
            Self::H => 0x33,
            Self::I => 0x43,
            Self::J => 0x3b,
            Self::K => 0x42,
            Self::L => 0x4b,
            Self::M => 0x3a,
            Self::N => 0x31,
            Self::O => 0x44,
            Self::P => 0x4d,
            Self::Q => 0x15,
            Self::R => 0x2d,
            Self::S => 0x1b,
            Self::T => 0x2c,
            Self::U => 0x3c,
            Self::V => 0x2a,
            Self::W => 0x1d,
            Self::X => 0x22,
            Self::Y => 0x35,
            Self::Z => 0x1a,
            Self::Num0 => 0x45,
            Self::Num1 => 0x16,
            Self::Num2 => 0x1e,
            Self::Num3 => 0x26,
            Self::Num4 => 0x25,
            Self::Num5 => 0x2e,
            Self::Num6 => 0x36,
            Self::Num7 => 0x3d,
            Self::Num8 => 0x3e,
            Self::Num9 => 0x46,
            Self::Minus => 0x4e,
            Self::Equal => 0x55,
            Self::LeftBracket => 0x54,
            Self::RightBracket => 0x5b,
            Self::Backslash => 0x5d,
            Self::Semicolon => 0x4c,
            Self::Apostrophe => 0x52,
            Self::Grave => 0x0e,
            Self::Comma => 0x41,
            Self::Dot => 0x49,
            Self::Slash => 0x4a,
            Self::Enter
            | Self::Space
            | Self::Backspace
            | Self::Tab
            | Self::Escape
            | Self::LeftShift
            | Self::RightShift => 0,
        }
    }
}

fn ascii_to_key(ch: u8) -> Option<(Ps2Key, bool)> {
    let key = match ch {
        b'\n' | b'\r' => (Ps2Key::Enter, false),
        b' ' => (Ps2Key::Space, false),
        b'\t' => (Ps2Key::Tab, false),
        b'a'..=b'z' => (letter_key(ch), false),
        b'A'..=b'Z' => (letter_key(ch.to_ascii_lowercase()), true),
        b'0' => (Ps2Key::Num0, false),
        b'1' => (Ps2Key::Num1, false),
        b'2' => (Ps2Key::Num2, false),
        b'3' => (Ps2Key::Num3, false),
        b'4' => (Ps2Key::Num4, false),
        b'5' => (Ps2Key::Num5, false),
        b'6' => (Ps2Key::Num6, false),
        b'7' => (Ps2Key::Num7, false),
        b'8' => (Ps2Key::Num8, false),
        b'9' => (Ps2Key::Num9, false),
        b'!' => (Ps2Key::Num1, true),
        b'@' => (Ps2Key::Num2, true),
        b'#' => (Ps2Key::Num3, true),
        b'$' => (Ps2Key::Num4, true),
        b'%' => (Ps2Key::Num5, true),
        b'^' => (Ps2Key::Num6, true),
        b'&' => (Ps2Key::Num7, true),
        b'*' => (Ps2Key::Num8, true),
        b'(' => (Ps2Key::Num9, true),
        b')' => (Ps2Key::Num0, true),
        b'-' => (Ps2Key::Minus, false),
        b'_' => (Ps2Key::Minus, true),
        b'=' => (Ps2Key::Equal, false),
        b'+' => (Ps2Key::Equal, true),
        b'[' => (Ps2Key::LeftBracket, false),
        b'{' => (Ps2Key::LeftBracket, true),
        b']' => (Ps2Key::RightBracket, false),
        b'}' => (Ps2Key::RightBracket, true),
        b'\\' => (Ps2Key::Backslash, false),
        b'|' => (Ps2Key::Backslash, true),
        b';' => (Ps2Key::Semicolon, false),
        b':' => (Ps2Key::Semicolon, true),
        b'\'' => (Ps2Key::Apostrophe, false),
        b'"' => (Ps2Key::Apostrophe, true),
        b'`' => (Ps2Key::Grave, false),
        b'~' => (Ps2Key::Grave, true),
        b',' => (Ps2Key::Comma, false),
        b'<' => (Ps2Key::Comma, true),
        b'.' => (Ps2Key::Dot, false),
        b'>' => (Ps2Key::Dot, true),
        b'/' => (Ps2Key::Slash, false),
        b'?' => (Ps2Key::Slash, true),
        _ => return None,
    };
    Some(key)
}

fn letter_key(ch: u8) -> Ps2Key {
    match ch {
        b'a' => Ps2Key::A,
        b'b' => Ps2Key::B,
        b'c' => Ps2Key::C,
        b'd' => Ps2Key::D,
        b'e' => Ps2Key::E,
        b'f' => Ps2Key::F,
        b'g' => Ps2Key::G,
        b'h' => Ps2Key::H,
        b'i' => Ps2Key::I,
        b'j' => Ps2Key::J,
        b'k' => Ps2Key::K,
        b'l' => Ps2Key::L,
        b'm' => Ps2Key::M,
        b'n' => Ps2Key::N,
        b'o' => Ps2Key::O,
        b'p' => Ps2Key::P,
        b'q' => Ps2Key::Q,
        b'r' => Ps2Key::R,
        b's' => Ps2Key::S,
        b't' => Ps2Key::T,
        b'u' => Ps2Key::U,
        b'v' => Ps2Key::V,
        b'w' => Ps2Key::W,
        b'x' => Ps2Key::X,
        b'y' => Ps2Key::Y,
        _ => Ps2Key::Z,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(kbc: &mut I8042) -> Vec<u8> {
        let mut out = Vec::new();
        while (kbc.pio_read(0x64).unwrap() & STATUS_OBF) != 0 {
            out.push(kbc.pio_read(0x60).unwrap());
        }
        out
    }

    fn mouse_command(kbc: &mut I8042, command: u8) -> Vec<u8> {
        kbc.pio_write(0x64, 0xd4);
        kbc.pio_write(0x60, command);
        read_all(kbc)
    }

    #[test]
    fn bulk_text_waits_for_keyboard_reads_and_command_exchanges() {
        let mut kbc = I8042::new();
        assert!(kbc.keyboard_ready_for_text());
        kbc.inject_ascii(b'A');
        assert!(!kbc.keyboard_ready_for_text());
        for byte in [0x2a, 0x1e, 0x9e] {
            assert_eq!(kbc.pio_read(0x60), Some(byte));
            assert!(!kbc.keyboard_ready_for_text());
        }
        assert_eq!(kbc.pio_read(0x60), Some(0xaa));
        assert!(kbc.keyboard_ready_for_text());

        kbc.pio_write(0x64, 0xad); // Clock inhibition with an empty queue.
        assert!(!kbc.keyboard_ready_for_text());
        kbc.pio_write(0x64, 0xae);
        kbc.pio_write(0x60, 0xf5); // Disabled scanning is independent of the link.
        assert_eq!(read_all(&mut kbc), [ACK]);
        assert!(!kbc.keyboard_ready_for_text());
        kbc.pio_write(0x60, 0xf4);
        assert!(!kbc.keyboard_ready_for_text()); // ACK still unread.
        assert_eq!(read_all(&mut kbc), [ACK]);
        assert!(kbc.keyboard_ready_for_text());

        kbc.pio_write(0x60, 0xed); // Wait through LED parameter exchange.
        assert_eq!(read_all(&mut kbc), [ACK]);
        assert!(!kbc.keyboard_ready_for_text());
        kbc.pio_write(0x60, 0);
        assert_eq!(read_all(&mut kbc), [ACK]);
        kbc.pio_write(0x64, 0x60); // Controller parameter exchange too.
        assert!(!kbc.keyboard_ready_for_text());
        kbc.pio_write(0x60, MODE_TRANSLATE); // IRQ masking alone is not busy.
        assert!(kbc.keyboard_ready_for_text());
        kbc.pio_write(0x64, 0xd3);
        kbc.pio_write(0x60, 0x55); // AUX output doesn't block a character.
        assert!(kbc.keyboard_ready_for_text());
        kbc.inject_ascii(b'b');
        assert_eq!(read_all(&mut kbc), [0x55, 0x30, 0xb0]);
        assert!(kbc.keyboard_ready_for_text());
    }

    #[test]
    fn controller_inhibits_device_bytes_but_keeps_its_output_latch() {
        let mut kbc = I8042::new();
        assert_eq!(mouse_command(&mut kbc, 0xf4), [ACK]);
        kbc.pio_write(0x64, 0x60);
        kbc.pio_write(0x60, 3); // Raw scan codes, system flag clear.
        assert_eq!(
            kbc.pio_read(0x64).unwrap() & (STATUS_CMD | STATUS_SELFTEST),
            0
        );
        kbc.pio_write(0x64, 0xad);
        kbc.pio_write(0x64, 0xa7);
        kbc.inject_key(Ps2Key::A, true);
        kbc.inject_mouse(5, 0, Ps2ButtonState::default());
        assert_eq!(kbc.pio_read(0x64).unwrap() & STATUS_OBF, 0);
        kbc.pio_write(0x64, 0x20); // Controller replies bypass inhibited links.
        assert!(!kbc.irq_level(1)); // The reply remains pollable without an IRQ.
        assert_eq!(read_all(&mut kbc), [0x33]);
        assert_eq!(kbc.pio_read(0x60), Some(0x33)); // Empty read retains last byte.
        assert!(!kbc.irq_level(1));
        kbc.pio_write(0x64, 0xae);
        assert_eq!(read_all(&mut kbc), [0x1c]);
        kbc.pio_write(0x64, 0xa8);
        assert_eq!(read_all(&mut kbc), [8, 5, 0]);

        kbc.pio_write(0x64, 0xd4);
        kbc.pio_write(0x64, 0x20); // A new command cancels pending mouse routing.
        assert_eq!(read_all(&mut kbc), [3]);
        kbc.pio_write(0x60, 0xee);
        assert_eq!(read_all(&mut kbc), [0xee]);
        kbc.pio_write(0x64, 0xaa);
        assert_eq!(read_all(&mut kbc), [0x55]);
        assert_ne!(kbc.pio_read(0x64).unwrap() & STATUS_SELFTEST, 0);
        kbc.pio_write(0x64, 0x20);
        assert_eq!(read_all(&mut kbc), [7]);
    }

    #[test]
    fn keyboard_scanning_scan_set_and_reset_change_the_actual_byte_stream() {
        let mut kbc = I8042::new();
        kbc.pio_write(0x60, 0xf5);
        assert_eq!(read_all(&mut kbc), [ACK]);
        kbc.inject_key(Ps2Key::A, true);
        assert!(read_all(&mut kbc).is_empty());
        kbc.pio_write(0x60, 0xf2); // Commands still respond while scanning is off.
        assert_eq!(read_all(&mut kbc), [ACK, 0xab, 0x41]);
        kbc.pio_write(0x60, 0xf4);
        assert_eq!(read_all(&mut kbc), [ACK]);

        kbc.pio_write(0x64, 0x60);
        kbc.pio_write(0x60, 3); // Disable controller translation.
        for set in [1, 2] {
            kbc.pio_write(0x60, 0xf0);
            kbc.pio_write(0x60, set);
            assert_eq!(read_all(&mut kbc), [ACK, ACK]);
            kbc.pio_write(0x60, 0xf0);
            kbc.pio_write(0x60, 0);
            assert_eq!(read_all(&mut kbc), [ACK, ACK, set]);
            kbc.inject_scancode_set2(&[0x1c, 0xf0, 0x1c, 0xe0, 0x6c]);
            let expected: &[u8] = if set == 1 {
                &[0x1e, 0x9e, 0xe0, 0x47]
            } else {
                &[0x1c, 0xf0, 0x1c, 0xe0, 0x6c]
            };
            assert_eq!(read_all(&mut kbc), expected);
        }
        kbc.pio_write(0x60, 0xf0);
        kbc.pio_write(0x60, 3); // Set 3 is not implemented, never claim it is active.
        assert_eq!(read_all(&mut kbc), [ACK, 0xfe]);
        kbc.pio_write(0x60, 0xf0);
        kbc.pio_write(0x60, 1);
        read_all(&mut kbc);
        kbc.pio_write(0x60, 0xff);
        assert_eq!(read_all(&mut kbc), [ACK, 0xaa]);
        kbc.pio_write(0x60, 0xf0);
        kbc.pio_write(0x60, 0);
        assert_eq!(read_all(&mut kbc), [ACK, ACK, 2]);
        kbc.pio_write(0x60, 0x05);
        assert_eq!(read_all(&mut kbc), [0xfe]);
        kbc.pio_write(0x60, 0xee);
        assert_eq!(read_all(&mut kbc), [0xee]);
        kbc.pio_write(0x60, 0xfe);
        assert_eq!(read_all(&mut kbc), [0xee]); // RESEND is not an ACK.
    }

    #[test]
    fn mouse_remote_wrap_status_and_resend_follow_commands() {
        let mut kbc = I8042::new();
        let left = Ps2ButtonState {
            left: true,
            ..Ps2ButtonState::default()
        };
        assert_eq!(mouse_command(&mut kbc, 0xf0), [ACK]);
        kbc.inject_mouse(5, 3, left); // Remote polling works without stream enable.
        assert!(read_all(&mut kbc).is_empty());
        assert_eq!(mouse_command(&mut kbc, 0xeb), [ACK, 0x29, 5, 0xfd]);
        assert_eq!(mouse_command(&mut kbc, 0xfe), [0x29, 5, 0xfd]);
        assert_eq!(mouse_command(&mut kbc, 0xeb), [ACK, 9, 0, 0]);
        assert_eq!(mouse_command(&mut kbc, 0xe9), [ACK, 0x44, 2, 100]);
        assert_eq!(mouse_command(&mut kbc, 0xee), [ACK]);
        assert_eq!(mouse_command(&mut kbc, 0xe9), [0xe9]); // Echo, not status/ACK.
        kbc.inject_mouse(30, 40, left);
        assert!(read_all(&mut kbc).is_empty());
        assert_eq!(mouse_command(&mut kbc, 0xec), [ACK]);
        assert_eq!(mouse_command(&mut kbc, 0xeb), [ACK, 9, 0, 0]);
        assert_eq!(mouse_command(&mut kbc, 0xe8), [ACK]);
        assert_eq!(mouse_command(&mut kbc, 4), [0xfe]); // Bad resolution, retain 2.
        assert_eq!(mouse_command(&mut kbc, 0xe9), [ACK, 0x44, 2, 100]);
        assert_eq!(mouse_command(&mut kbc, 0x42), [0xfe]);
        assert_eq!(mouse_command(&mut kbc, 0xff), [ACK, 0xaa, 0]);
        assert_eq!(mouse_command(&mut kbc, 0xe9), [ACK, 4, 2, 100]);
        assert_eq!(mouse_command(&mut kbc, 0xf4), [ACK]);
        kbc.inject_mouse(5, 0, left);
        kbc.inject_mouse(17, 0, left);
        // Only the first packet has reached the controller. RESEND must not
        // substitute newer motion that is still waiting inside the mouse.
        assert_eq!(mouse_command(&mut kbc, 0xfe), [9, 5, 0, 9, 17, 0, 9, 5, 0]);
    }

    #[test]
    fn mouse_scaling_changes_stream_packets_but_not_polled_counts() {
        let mut kbc = I8042::new();
        assert_eq!(mouse_command(&mut kbc, 0xe7), [ACK]);
        assert_eq!(mouse_command(&mut kbc, 0xf4), [ACK]);
        kbc.inject_mouse(4, 5, Ps2ButtonState::default());
        assert_eq!(read_all(&mut kbc), [0x28, 6, 0xf7]); // Nonlinear 2:1 curve.
        assert_eq!(mouse_command(&mut kbc, 0xf0), [ACK]);
        kbc.inject_mouse(4, 5, Ps2ButtonState::default());
        assert_eq!(mouse_command(&mut kbc, 0xeb), [ACK, 0x28, 4, 0xfb]);
        assert_eq!(mouse_command(&mut kbc, 0xf6), [ACK]);
        kbc.inject_mouse(10, 20, Ps2ButtonState::default());
        assert!(read_all(&mut kbc).is_empty());
    }

    #[test]
    fn full_input_queues_never_publish_partial_key_sequences_or_mouse_packets() {
        let mut kbc = I8042::new();
        kbc.pio_write(0x64, 0xad);
        kbc.inject_scancode_set2(&[0x1c; 255]);
        kbc.inject_scancode_set2(&[0xe0, 0xf0, 0x75]);
        kbc.pio_write(0x64, 0xae);
        assert_eq!(read_all(&mut kbc), [0x1e; 255]);
        assert_eq!(kbc.keyboard_dropped(), 2);
        kbc.inject_key(Ps2Key::B, true);
        assert_eq!(read_all(&mut kbc), [0x30]);

        assert_eq!(mouse_command(&mut kbc, 0xf4), [ACK]);
        kbc.pio_write(0x64, 0xa7);
        for _ in 0..86 {
            kbc.inject_mouse(1, -1, Ps2ButtonState::default());
        }
        kbc.pio_write(0x64, 0xa8);
        assert_eq!(read_all(&mut kbc), [8, 1, 1].repeat(85));
        assert_eq!(kbc.mouse_dropped(), 3);
        kbc.inject_mouse(2, -3, Ps2ButtonState::default());
        assert_eq!(read_all(&mut kbc), [8, 2, 3]);
    }

    #[test]
    fn controller_self_tests_and_mode_round_trip() {
        let mut kbc = I8042::new();
        kbc.pio_write(0x64, 0xaa);
        assert_eq!(kbc.pio_read(0x60), Some(0x55));
        kbc.pio_write(0x64, 0x20);
        assert_eq!(
            kbc.pio_read(0x60),
            Some(MODE_KBD_INT | MODE_MOUSE_INT | MODE_SYS | MODE_TRANSLATE)
        );
        kbc.pio_write(0x64, 0x60);
        kbc.pio_write(0x60, MODE_KBD_INT);
        kbc.pio_write(0x64, 0x20);
        assert_eq!(kbc.pio_read(0x60), Some(MODE_KBD_INT));
    }

    #[test]
    fn aux_loopback_sets_mouse_output_bit_and_irq12() {
        let mut kbc = I8042::new();
        kbc.pio_write(0x64, 0xd3);
        kbc.pio_write(0x60, 0x5a);
        let status = kbc.pio_read(0x64).unwrap();
        assert_ne!(status & STATUS_OBF, 0);
        assert_ne!(status & STATUS_MOUSE_OBF, 0);
        assert!(kbc.irq_level(12));
        assert_eq!(kbc.pio_read(0x60), Some(0x5a));
        assert!(!kbc.irq_level(12));
    }

    #[test]
    fn keyboard_identify_translation_matches_linux_accepted_path() {
        let mut kbc = I8042::new();
        kbc.pio_write(0x60, 0xf2);
        assert_eq!(read_all(&mut kbc), alloc::vec![ACK, 0xab, 0x41]);
    }

    #[test]
    fn keyboard_led_command_consumes_parameter() {
        let mut kbc = I8042::new();
        kbc.pio_write(0x60, 0xed);
        kbc.pio_write(0x60, 0x07);
        assert_eq!(read_all(&mut kbc), alloc::vec![ACK, ACK]);
    }

    #[test]
    fn mouse_reset_and_packet_generation() {
        let mut kbc = I8042::new();
        kbc.pio_write(0x64, 0xd4);
        kbc.pio_write(0x60, 0xff);
        assert_eq!(read_all(&mut kbc), alloc::vec![ACK, 0xaa, 0x00]);
        kbc.pio_write(0x64, 0xd4);
        kbc.pio_write(0x60, 0xf4);
        assert_eq!(read_all(&mut kbc), alloc::vec![ACK]);
        kbc.inject_mouse(
            5,
            3,
            Ps2ButtonState {
                left: true,
                right: false,
                middle: false,
            },
        );
        assert_eq!(read_all(&mut kbc), alloc::vec![0x08 | 0x01 | 0x20, 5, 0xfd]);
    }

    #[test]
    fn reset_and_a20_emit_effects() {
        let mut kbc = I8042::new();
        kbc.pio_write(0x64, 0xfe);
        kbc.pio_write(0x64, 0xd1);
        kbc.pio_write(0x60, 0x00);
        let mut effects = Vec::new();
        kbc.drain_effects(&mut |effect| effects.push(effect));
        assert!(effects.contains(&DeviceEffect::ResetRequest));
        assert!(effects.contains(&DeviceEffect::A20(false)));
    }
}
