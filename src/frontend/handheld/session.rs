//! A running VM on the handheld screen. One thread interleaves guest slices,
//! keyboard input, display updates and remote-management requests. The
//! in-VM menu (Fn+Esc, Start on a controller, or Ctrl+Alt+M on other
//! keyboards) pauses the guest.
//!
//! Remote management serves the r86 web console (ws_frontend::Remote) on the
//! Wi-Fi: screen, keyboard, mouse, power, virtual CD and, while the VM is off,
//! its disk's files. While it is on, a powered-off VM stays in the session
//! and the panel shows the address instead of returning to the library.
use std::cell::RefCell;
use std::fs::File;
use std::net::TcpListener;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use r86::devices::{Ps2ButtonState, RenderMode};
use r86::machine::{GuestStepStatus, Machine};

use super::device::{Device, KeyEvent, stop_requested};
use super::keymap;
use super::osk::Osk;
use super::screens::{Action, Ctx, Outcome, OverlayLabels, Stack, VmOverlay};
use super::text80;
use super::ui::{self, BLACK, Canvas, Font, Rect, TEXT};
use crate::keys::{InputKey, inject_input_key};
use crate::vm_store::{self, Vm};
use crate::ws_frontend::Remote;

/// Guest instructions between input polls: a few milliseconds on a Cortex-A53.
const SLICE_STEPS: u64 = 200_000;
const FRAME_INTERVAL: Duration = Duration::from_millis(40);
const TOAST: Duration = Duration::from_secs(4);
/// The standard VGA BIOS plus 640x340 (twice the CardputerZero panel) and
/// 300x200 RGB565 VBE modes; see firmware/seabios/handheld-modes.patch.
const HANDHELD_VGA_BIOS: &str = "vgabios-handheld.bin";
/// Remote management listens on the first free port.
const REMOTE_PORTS: std::ops::RangeInclusive<u16> = 8086..=8095;
/// How often a held session checks for remote requests: quickly while a
/// browser is connected, slowly otherwise to save the battery.
const REMOTE_POLL_MS: i32 = 20;
const REMOTE_IDLE_MS: i32 = 200;

pub enum End {
    PoweredOff,
    TurnedOff,
    Signal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Aspect,
    Stretch,
    Integer,
}

impl View {
    fn label(self) -> &'static str {
        match self {
            View::Aspect => "keep aspect",
            View::Stretch => "stretch",
            View::Integer => "integer",
        }
    }
}

#[derive(Default)]
struct Modifiers {
    fn_key: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
}

struct Session<'a> {
    vm: Vm,
    machine: Machine,
    ctx: &'a Ctx,
    view: View,
    /// Keys this frontend holds down in the guest, by evdev code.
    held: Vec<(u16, InputKey)>,
    buttons: Ps2ButtonState,
    /// Approximate guest pointer position, for wide frames at integer scale.
    pointer: (i32, i32),
    modifiers: Modifiers,
    rgba: Vec<u8>,
    frame: (i32, i32),
    guest: Canvas,
    overlay: Option<Stack>,
    labels: Rc<RefCell<OverlayLabels>>,
    toast: Option<(String, Instant)>,
    redraw: bool,
    reset_count: u64,
    /// Drawing 80-column text natively, so the cursor blinks.
    native_text: bool,
    blink_phase: u128,
    remote: Option<Remote>,
    /// What the panel last showed for a remote-held VM: (off, clients).
    remote_shown: Option<(bool, usize)>,
    /// The on-screen keyboard; it keeps its selection while hidden.
    osk: Osk,
    osk_shown: bool,
}

/// Run `vm` until it powers off, the user turns it off, or a stop signal.
/// With `remote`, the VM starts powered off under remote management.
pub fn run(
    mut vm: Vm,
    ctx: &Ctx,
    device: &mut dyn Device,
    canvas: &mut Canvas,
    remote: bool,
) -> Result<End, String> {
    let _lock = vm_store::lock_vm(&vm.directory)?;
    // Remote management also serves the disk's files, so a moved or deleted
    // CD image leaves the drive empty for this session instead of failing.
    let missing_cd = remote && vm.iso.as_ref().is_some_and(|iso| !iso.exists());
    if missing_cd {
        vm.iso = None;
    }
    let mut machine = vm.machine(&ctx.share, HANDHELD_VGA_BIOS)?;
    let remote = if remote {
        machine.power_off()?;
        Some(start_remote(&ctx.iso)?)
    } else {
        None
    };
    let view = match vm.scale.as_str() {
        "stretch" => View::Stretch,
        "integer" => View::Integer,
        _ => View::Aspect,
    };
    let mut session = Session {
        reset_count: machine.reset_count(),
        vm,
        machine,
        view,
        ctx,
        held: Vec::new(),
        buttons: Ps2ButtonState::default(),
        pointer: (0, 0),
        modifiers: Modifiers::default(),
        rgba: Vec::new(),
        frame: (0, 0),
        guest: Canvas::new(canvas.width, canvas.height),
        overlay: None,
        labels: Rc::new(RefCell::new(OverlayLabels::default())),
        toast: None,
        redraw: true,
        native_text: false,
        blink_phase: 0,
        remote,
        remote_shown: None,
        osk: Osk::new(),
        osk_shown: false,
    };
    if missing_cd {
        session.notify("The CD image is missing; the drive is empty");
    } else if session.remote.is_none() {
        session.notify(&format!("{}: r86 menu", device.menu_keys()));
    }
    let result = session.run(device, canvas);
    let display_closed = device.set_hdmi(false, &session.vm.name);
    let flushed = session.machine.flush_disks();
    let end = result?;
    display_closed?;
    flushed.map(|()| end)
}

impl Session<'_> {
    fn run(&mut self, device: &mut dyn Device, canvas: &mut Canvas) -> Result<End, String> {
        let mut events = Vec::new();
        let mut next_frame = Instant::now();
        let started = Instant::now();
        loop {
            if stop_requested() {
                return Ok(End::Signal);
            }
            device.read(&mut events);
            for event in events.drain(..) {
                if let Some(end) = self.key(event, device)? {
                    return Ok(end);
                }
            }
            if device.take_display_disconnect() {
                self.release_guest_input();
                if self.overlay.is_some() {
                    self.render_guest();
                }
                *self.labels.borrow_mut() = self.current_labels(device);
                self.notify("HDMI disconnected");
            }
            if self
                .toast
                .as_ref()
                .is_some_and(|(_, until)| Instant::now() >= *until)
            {
                self.toast = None;
                self.redraw = true;
            }
            let (dx, dy) = device.pointer_motion();
            if (dx, dy) != (0, 0) && self.overlay.is_none() && !self.machine.powered_off() {
                self.move_pointer(dx, dy);
            }
            if let Some(remote) = &mut self.remote {
                remote.poll(&mut self.machine);
                if let Some(iso) = remote.take_disc_change() {
                    self.vm.cdrom = true;
                    self.vm.iso = iso;
                    // This session holds the VM lock.
                    self.vm.save_locked()?;
                }
            }
            if self.machine.reset_count() != self.reset_count {
                // A board reset drops device input; forget what we held.
                self.reset_count = self.machine.reset_count();
                self.forget_input();
            }
            if let Some(overlay) = &mut self.overlay {
                if self.redraw {
                    if device.hdmi() == Some(true) {
                        canvas.fill(ui::BG);
                    } else {
                        canvas.pixels.copy_from_slice(&self.guest.pixels);
                    }
                    overlay.draw(canvas, self.ctx);
                    if let Some((text, _)) = &self.toast {
                        draw_toast(canvas, text, canvas.height);
                    }
                    device.present(canvas)?;
                    self.redraw = false;
                }
                device.wait(self.remote_wait(500));
                continue;
            }
            if self.held_by_remote(device, canvas)? {
                device.wait(self.remote_wait(REMOTE_IDLE_MS));
                continue;
            }
            self.remote_shown = None;
            let report = match &mut self.remote {
                Some(remote) => remote.run_slice(&mut self.machine, SLICE_STEPS)?,
                None => self.machine.run_steps(SLICE_STEPS)?,
            };
            let mut idle = None;
            match report.stop {
                Some(GuestStepStatus::Halted) => idle = Some(self.machine.halted_wait_duration()),
                // Remote management keeps a powered-off VM; see held_by_remote.
                Some(GuestStepStatus::PoweredOff) if self.remote.is_some() => self.redraw = true,
                Some(GuestStepStatus::PoweredOff) => return Ok(End::PoweredOff),
                Some(GuestStepStatus::Stopped { message, eip }) => {
                    let at = eip.map_or(String::new(), |eip| format!(" at EIP {eip:#010x}"));
                    return Err(format!("The VM stopped{at}: {message}"));
                }
                _ => {}
            }
            let now = Instant::now();
            // VGA blinks the cursor every 16 frames at 60 Hz.
            let phase = started.elapsed().as_millis() / 267;
            if self.native_text && phase != self.blink_phase {
                self.redraw = true;
            }
            self.blink_phase = phase;
            if now >= next_frame && (self.redraw || self.machine.vga().whole_frame_dirty()) {
                let note = self.toast.as_ref().map(|(text, _)| text.clone());
                self.show_frame(device, canvas, note.as_deref())?;
                self.redraw = false;
                next_frame = now + FRAME_INTERVAL;
            }
            if let Some(idle) = idle {
                let until_frame = next_frame.saturating_duration_since(now);
                device.wait(
                    idle.min(until_frame.max(Duration::from_millis(1)))
                        .as_millis() as i32,
                );
            }
        }
    }

    /// Under remote management, a powered-off VM shows the console address,
    /// and a VM paused from the browser shows its last frame with a note.
    /// Returns whether the guest must not run now.
    fn held_by_remote(
        &mut self,
        device: &mut dyn Device,
        canvas: &mut Canvas,
    ) -> Result<bool, String> {
        let Some(remote) = &self.remote else {
            return Ok(false);
        };
        let off = self.machine.powered_off();
        if !off && remote.running() {
            return Ok(false);
        }
        let state = (off, remote.clients());
        if self.remote_shown == Some(state) && !self.redraw {
            return Ok(true);
        }
        if off {
            draw_remote_off(canvas, &self.vm.name, &remote.url(), state.1);
            if let Some((text, _)) = &self.toast {
                draw_toast(canvas, text, canvas.height);
            }
            device.present_vm(canvas)?;
        } else {
            let note = self
                .toast
                .as_ref()
                .map_or("Paused from the browser", |(text, _)| text)
                .to_string();
            self.show_frame(device, canvas, Some(&note))?;
        }
        self.remote_shown = Some(state);
        self.redraw = false;
        Ok(true)
    }

    /// Milliseconds to sleep while the guest is not running.
    fn remote_wait(&self, idle: i32) -> i32 {
        match &self.remote {
            Some(remote) if remote.clients() > 0 => REMOTE_POLL_MS,
            Some(_) => idle.min(REMOTE_IDLE_MS),
            None => idle,
        }
    }

    fn notify(&mut self, text: &str) {
        self.toast = Some((text.into(), Instant::now() + TOAST));
        self.redraw = true;
    }

    /// Present the guest frame with an optional note: as its own scaled
    /// layer when the device has one, else drawn into the UI canvas.
    fn show_frame(
        &mut self,
        device: &mut dyn Device,
        canvas: &mut Canvas,
        note: Option<&str>,
    ) -> Result<(), String> {
        let layer = device.guest_layer() && self.render_rgba();
        if layer {
            self.native_text = false;
        } else {
            self.render_guest();
            canvas.pixels.copy_from_slice(&self.guest.pixels);
        }
        // The keyboard sits at the bottom and a note just above it; on a
        // guest layer only that strip of the canvas is shown.
        let osk = self.osk_shown.then(|| self.osk.draw(canvas));
        let bottom = osk.map_or(canvas.height, |area| area.y);
        let toast = note.map(|text| draw_toast(canvas, text, bottom));
        if !layer {
            return device.present_vm(canvas);
        }
        let area = match (toast, osk) {
            (Some(toast), Some(osk)) => Some(Rect {
                h: osk.y + osk.h - toast.y,
                ..toast
            }),
            (toast, osk) => toast.or(osk),
        };
        // The guest moves above the keyboard instead of hiding under it.
        let view = Rect {
            x: 0,
            y: 0,
            w: canvas.width,
            h: osk.map_or(canvas.height, |area| area.y),
        };
        let stretch = self.view == View::Stretch;
        device.present_frame(&self.rgba, self.frame, stretch, view, canvas, area)
    }

    /// Render the guest frame into `rgba` at its own size; false if the
    /// screen is blank.
    fn render_rgba(&mut self) -> bool {
        let info = self.machine.vga().mode_info();
        let (w, h) = (i32::from(info.width), i32::from(info.height));
        self.rgba.resize((w * h * 4) as usize, 0);
        let rendered = self.machine.vga().render_rgba(&mut self.rgba).is_some();
        self.machine.clear_frame_dirty();
        if self.frame != (w, h) {
            self.frame = (w, h);
            self.pointer = (w / 2, h / 2);
        }
        rendered && w != 0 && h != 0 && info.mode != RenderMode::Blank
    }

    fn render_guest(&mut self) {
        let info = self.machine.vga().mode_info();
        // Native 80-column text fills the panel; integer scale shows the
        // 720x400 text 1:1 around the cursor instead.
        if self.view != View::Integer && info.mode == RenderMode::Text {
            let vga = self.machine.vga();
            let text = vga.text_snapshot();
            if text80::fits(&text, &self.guest) {
                let colors: [u16; 16] = std::array::from_fn(|i| {
                    let [r, g, b, _] = vga.attribute_rgba(i as u8);
                    ui::rgb(r, g, b)
                });
                self.guest.fill(BLACK);
                text80::draw(
                    &mut self.guest,
                    &text,
                    &colors,
                    self.blink_phase.is_multiple_of(2),
                );
                self.machine.clear_frame_dirty();
                self.native_text = true;
                return;
            }
        }
        self.native_text = false;
        let shown = self.render_rgba();
        let (w, h) = self.frame;
        // Frames no wider than the panel are cropped in the middle; wider
        // ones follow the text cursor or the pointer.
        let follow = self.view == View::Integer && w > self.guest.width;
        let focus = if follow {
            self.focus(w, h)
        } else {
            (w / 2, h / 2)
        };
        let c = &mut self.guest;
        c.fill(BLACK);
        if !shown {
            return;
        }
        let whole = Rect { x: 0, y: 0, w, h };
        match self.view {
            View::Stretch => c.blit_rgba(
                Rect {
                    x: 0,
                    y: 0,
                    w: c.width,
                    h: c.height,
                },
                &self.rgba,
                w as usize,
                whole,
            ),
            View::Aspect => {
                // Shrink to fit, keeping the aspect ratio; never enlarge past 2x.
                let scale = (c.width as f32 / w as f32)
                    .min(c.height as f32 / h as f32)
                    .min(2.0);
                let (dw, dh) = ((w as f32 * scale) as i32, (h as f32 * scale) as i32);
                let to = Rect {
                    x: (c.width - dw) / 2,
                    y: (c.height - dh) / 2,
                    w: dw,
                    h: dh,
                };
                c.blit_rgba(to, &self.rgba, w as usize, whole);
            }
            View::Integer => {
                let k = if w <= c.width {
                    (c.width / w).min(c.height / h).max(1)
                } else {
                    1
                };
                let (fx, fy) = focus;
                let (vw, vh) = ((c.width / k).min(w), (c.height / k).min(h));
                let from = Rect {
                    x: (fx - vw / 2).clamp(0, w - vw),
                    y: (fy - vh / 2).clamp(0, h - vh),
                    w: vw,
                    h: vh,
                };
                let to = Rect {
                    x: (c.width - vw * k) / 2,
                    y: (c.height - vh * k) / 2,
                    w: vw * k,
                    h: vh * k,
                };
                c.blit_rgba(to, &self.rgba, w as usize, from);
            }
        }
    }

    /// The zoomed view follows the text cursor, or the approximate pointer.
    fn focus(&self, w: i32, h: i32) -> (i32, i32) {
        if self.machine.vga().mode_info().mode == RenderMode::Text {
            let text = self.machine.vga().text_snapshot();
            if let Some((col, row)) = text.cursor {
                let cell_w = w / i32::from(text.cols.max(1));
                let cell_h = h / i32::from(text.rows.max(1));
                return (
                    i32::from(col) * cell_w + cell_w / 2,
                    i32::from(row) * cell_h + cell_h / 2,
                );
            }
        }
        self.pointer
    }

    fn current_labels(&self, device: &dyn Device) -> OverlayLabels {
        OverlayLabels {
            name: self.vm.name.clone(),
            view: self.view.label(),
            display: device.hdmi(),
            cd: self.vm.iso.as_ref().map(|iso| {
                iso.file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            }),
            remote: self.remote.as_ref().map(Remote::url),
        }
    }

    fn key(&mut self, event: KeyEvent, device: &mut dyn Device) -> Result<Option<End>, String> {
        let down = event.value != 0;
        match event.code {
            keymap::FN => self.modifiers.fn_key = down,
            keymap::LEFT_CTRL | keymap::RIGHT_CTRL => self.modifiers.ctrl = down,
            keymap::LEFT_ALT | keymap::RIGHT_ALT => self.modifiers.alt = down,
            keymap::LEFT_SHIFT | keymap::RIGHT_SHIFT => self.modifiers.shift = down,
            _ => {}
        }
        let menu_chord = (self.modifiers.fn_key && event.code == keymap::ESC)
            || (self.modifiers.ctrl && self.modifiers.alt && event.code == keymap::KEY_M)
            || event.code == keymap::MENU;
        if let Some(overlay) = &mut self.overlay {
            if event.value == 1 && menu_chord {
                return self.act(Action::Resume, device);
            }
            let ui_key = (event.value != 0)
                .then(|| keymap::ui_key(event.code, self.modifiers.shift))
                .flatten();
            if let Some(key) = ui_key {
                self.redraw = true;
                match overlay.key(key, self.ctx) {
                    Some(Outcome::Action(action)) => return self.act(action, device),
                    Some(Outcome::Closed) => return self.act(Action::Resume, device),
                    _ => {}
                }
            }
            return Ok(None);
        }
        if let Some(remote) = &mut self.remote
            && self.machine.powered_off()
        {
            match (event.value, event.code) {
                (1, keymap::ENTER) => {
                    remote.power_on(&mut self.machine)?;
                    self.redraw = true;
                }
                (1, keymap::ESC) => return Ok(Some(End::TurnedOff)),
                _ => {}
            }
            return Ok(None);
        }
        if event.value == 1 && menu_chord {
            self.release_guest_input();
            // HDMI menus share a plain canvas with the LCD; never put the
            // guest on the LCD while it belongs to the external display.
            if device.hdmi() != Some(true) {
                self.render_guest();
            }
            *self.labels.borrow_mut() = self.current_labels(device);
            let overlay = VmOverlay::new(self.labels.clone());
            self.overlay = Some(Stack::new(Box::new(overlay)));
            self.toast = None;
            self.redraw = true;
            return Ok(None);
        }
        let osk_chord = self.modifiers.ctrl && self.modifiers.alt && event.code == keymap::KEY_K;
        if event.code == keymap::OSK || osk_chord {
            if event.value == 1 {
                self.toggle_osk();
            }
            return Ok(None);
        }
        if self.osk_shown
            && let keymap::UP
            | keymap::DOWN
            | keymap::LEFT
            | keymap::RIGHT
            | keymap::ENTER
            | keymap::ESC = event.code
        {
            let osk = &mut self.osk;
            let effects = match (event.code, event.value) {
                (keymap::ENTER, 1) => osk.press(),
                (keymap::ENTER, 2) => osk.repeat(),
                (keymap::ENTER, 0) => osk.release(),
                (keymap::ESC, 1) => {
                    self.toggle_osk();
                    return Ok(None);
                }
                (keymap::ESC | keymap::ENTER, _) => Vec::new(),
                (code, 1 | 2) => {
                    osk.step(code);
                    Vec::new()
                }
                _ => return Ok(None),
            };
            for (code, value) in effects {
                self.guest_key(KeyEvent { code, value });
            }
            self.redraw = true;
            return Ok(None);
        }
        if let keymap::BTN_LEFT | keymap::BTN_RIGHT = event.code {
            if event.value != 2 {
                let down = event.value == 1;
                if event.code == keymap::BTN_LEFT {
                    self.buttons.left = down;
                } else {
                    self.buttons.right = down;
                }
                self.machine.inject_mouse(0, 0, self.buttons);
            }
            return Ok(None);
        }
        if event.code == keymap::ESC && event.value == 1 {
            // Esc stays the guest's; remind the user how to reach r86.
            self.notify(&format!("{}: r86 menu", self.ctx.menu_keys));
        }
        self.guest_key(event);
        Ok(None)
    }

    /// Show or hide the on-screen keyboard; hiding releases its keys.
    fn toggle_osk(&mut self) {
        self.osk_shown = !self.osk_shown;
        if !self.osk_shown {
            for (code, value) in self.osk.release_all() {
                self.guest_key(KeyEvent { code, value });
            }
        }
        self.redraw = true;
    }

    /// After a reset the guest holds nothing; forget what this frontend held.
    fn forget_input(&mut self) {
        self.held.clear();
        self.buttons = Ps2ButtonState::default();
        self.osk.forget();
    }

    /// Press, repeat or release a guest key, tracking what this frontend holds.
    fn guest_key(&mut self, event: KeyEvent) {
        let held = self.held.iter().position(|(code, _)| *code == event.code);
        match (event.value, held) {
            (0, Some(index)) => {
                let (_, key) = self.held.remove(index);
                inject_input_key(&mut self.machine, key, false);
            }
            // Autorepeat re-sends make codes, as a PC keyboard's typematic does.
            (2, Some(index)) => inject_input_key(&mut self.machine, self.held[index].1, true),
            (1, None) => {
                if let Some(key) = keymap::guest_key(event.code) {
                    self.held.push((event.code, key));
                    inject_input_key(&mut self.machine, key, true);
                }
            }
            _ => {}
        }
    }

    /// Move the PS/2 mouse and the approximate pointer that zoomed views follow.
    fn move_pointer(&mut self, dx: i32, dy: i32) {
        self.machine
            .inject_mouse(dx as i16, dy as i16, self.buttons);
        let (w, h) = self.frame;
        self.pointer = (
            (self.pointer.0 + dx).clamp(0, w.max(1) - 1),
            (self.pointer.1 + dy).clamp(0, h.max(1) - 1),
        );
        if self.view == View::Integer {
            self.redraw = true;
        }
    }

    fn release_guest_input(&mut self) {
        for (_, key) in self.held.drain(..) {
            inject_input_key(&mut self.machine, key, false);
        }
        self.osk.forget();
        if self.buttons != Ps2ButtonState::default() {
            self.buttons = Ps2ButtonState::default();
            self.machine.inject_mouse(0, 0, self.buttons);
        }
    }

    fn act(&mut self, action: Action, device: &mut dyn Device) -> Result<Option<End>, String> {
        let mut close = true;
        match action {
            Action::Resume => {
                if let Some(remote) = &mut self.remote
                    && !remote.running()
                    && !self.machine.powered_off()
                {
                    remote.set_running(&self.machine, true);
                }
            }
            Action::CycleView => {
                self.view = match self.view {
                    View::Aspect => View::Stretch,
                    View::Stretch => View::Integer,
                    View::Integer => View::Aspect,
                };
                if self.view == View::Integer {
                    self.pointer = (self.frame.0 / 2, self.frame.1 / 2);
                }
                if device.hdmi() != Some(true) {
                    self.render_guest();
                }
                close = false;
            }
            Action::Display => {
                if let Some(active) = device.hdmi() {
                    if let Err(err) = device.set_hdmi(!active, &self.vm.name) {
                        eprintln!("r86: HDMI: {err}");
                        self.notify("No HDMI display");
                    }
                    if device.hdmi() == Some(false) {
                        self.render_guest();
                    }
                }
                close = false;
            }
            Action::CtrlAltDel => {
                for name in ["controlleft", "altleft", "delete"] {
                    crate::keys::inject_named_key(&mut self.machine, name, true)?;
                }
                for name in ["delete", "altleft", "controlleft"] {
                    crate::keys::inject_named_key(&mut self.machine, name, false)?;
                }
            }
            Action::Cd(path) => {
                let result = match &path {
                    Some(path) => File::open(path)
                        .map_err(|err| format!("Cannot open {}: {err}", path.display()))
                        .and_then(|file| self.machine.change_file_cdrom(file)),
                    None => self
                        .machine
                        .change_cdrom(Vec::new())
                        .map_err(str::to_string),
                };
                match result {
                    Ok(()) => {
                        self.vm.cdrom = true;
                        self.vm.iso = path;
                        // This session holds the VM lock.
                        self.vm.save_locked()?;
                        let name = self.current_labels(device).cd;
                        if let Some(remote) = &mut self.remote {
                            remote.media_changed(&self.machine, name);
                        }
                        self.notify(if self.vm.iso.is_some() {
                            "CD inserted"
                        } else {
                            "CD ejected"
                        });
                    }
                    Err(err) => self.notify(&err),
                }
            }
            Action::PowerButton => {
                self.machine.press_power_button();
                self.notify("Power button pressed");
            }
            Action::Reset => {
                match &mut self.remote {
                    Some(remote) => remote.reset(&mut self.machine)?,
                    None => self.machine.reboot()?,
                }
                self.forget_input();
            }
            // Under remote management the VM stays, off, for the browser.
            Action::TurnOff => match &mut self.remote {
                Some(remote) => remote.power_off(&mut self.machine)?,
                None => return Ok(Some(End::TurnedOff)),
            },
            Action::Remote => {
                if let Some(remote) = self.remote.take() {
                    remote.close(&mut self.machine);
                    self.notify("Remote management stopped");
                } else {
                    match start_remote(&self.ctx.iso) {
                        Ok(mut remote) => {
                            remote.set_running(&self.machine, true);
                            self.remote = Some(remote);
                            close = false;
                        }
                        Err(err) => self.notify(&err),
                    }
                }
            }
        }
        if close {
            self.overlay = None;
        }
        *self.labels.borrow_mut() = self.current_labels(device);
        self.redraw = true;
        Ok(None)
    }
}

/// The console's port on the home Wi-Fi, for remote management and imports;
/// anyone on that network can use it.
pub fn console_listener() -> Result<TcpListener, String> {
    REMOTE_PORTS
        .clone()
        .find_map(|port| TcpListener::bind(("0.0.0.0", port)).ok())
        .ok_or_else(|| "No free network port (8086-8095).".into())
}

/// Discs from the browser are saved in `iso_dir`, beside the others.
fn start_remote(iso_dir: &Path) -> Result<Remote, String> {
    let listener = console_listener()?;
    let remote = Remote::new(listener, SLICE_STEPS, None, false, Some(iso_dir.into()))?;
    // The device log also gets the address, for setups without a view of the panel.
    eprintln!("r86-handheld: remote management at {}", remote.url());
    Ok(remote)
}

fn draw_remote_off(c: &mut Canvas, name: &str, url: &str, clients: usize) {
    c.fill(ui::BG);
    ui::header(c, name, "off");
    c.text_centered(30, "Remote management: on a phone or", TEXT, Font::Small);
    c.text_centered(40, "computer on the same Wi-Fi, open", TEXT, Font::Small);
    c.text_centered(64, url, ui::ACCENT, Font::Normal);
    let status = match clients {
        0 => "No browser connected".to_string(),
        1 => "1 browser connected".to_string(),
        n => format!("{n} browsers connected"),
    };
    c.text_centered(104, &status, ui::MUTED, Font::Small);
    c.text_centered(
        118,
        "The VM is off; its disk can be changed.",
        ui::MUTED,
        Font::Small,
    );
    ui::footer(c, "Enter: power on   Esc: exit");
}

/// A note across the canvas, ending at `bottom`; returns its area.
fn draw_toast(c: &mut Canvas, text: &str, bottom: i32) -> Rect {
    let columns = (c.width / 8 - 2) as usize;
    let lines = ui::wrap(text, columns);
    let h = lines.len() as i32 * 10 + 6;
    let area = Rect {
        x: 0,
        y: bottom - h,
        w: c.width,
        h,
    };
    c.dim(area);
    c.dim(area);
    for (i, line) in lines.iter().enumerate() {
        c.text(8, area.y + 3 + i as i32 * 10, line, TEXT, Font::Small);
    }
    area
}
