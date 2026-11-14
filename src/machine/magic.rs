//! r86 magic instruction protocol, version 1.
//!
//! Discover with CPUID EAX=40000000h: EAX=40000000h and EBX:ECX:EDX contain
//! the twelve ASCII bytes "r86r86r86r86". CPUID.1:ECX[31] remains clear.
//! IN EAX,DX (EDh, 32-bit operand), DX=5286h, EAX=21363872h calls this
//! protocol before the I/O permission check, in every CPU mode and CPL.
//! ECX selects the command; EBX/ESI/EDI are reserved arguments. There is no
//! guest memory access. Registers not listed as outputs and EFLAGS survive.
//! Unknown commands return EAX=FFFFFFFFh without an exception.
//!
//! Commands:
//! - 0, version: EAX=protocol version (1), EBX=capabilities (bit 0: time,
//!   bit 1: pointer, bit 2: host requests and guest reports).
//! - 1, time: EDX:EAX=unsigned UTC seconds since 1970-01-01, EBX=signed
//!   seconds east of UTC. The host epoch and offset are captured once when
//!   realtime mode starts; elapsed time uses the already sampled machine
//!   clock. Deterministic runs start at 2026-01-01 UTC. RTC writes do not
//!   change this time, and a machine reset preserves its epoch.
//! - 2, pointer poll: EAX=1 if an event was returned, else 0; EBX=x in the
//!   low 16 bits and y in the high 16 bits (both 0..65535); ECX=buttons
//!   (bits 0/1/2: left/right/middle); EDX=signed wheel steps; ESI=next sleep
//!   in milliseconds; EDI=queued host requests. Empty polls return the last
//!   delivered position/buttons and zero wheel. Sleep is 0 while events
//!   remain, 10 while a button is held or within one second of movement/a
//!   held button, otherwise 100.
//! - 3, request: takes the oldest host request: EAX=kind (0: none), EBX,
//!   ECX and EDX its arguments. Kind 1, display mode: EBX=width, ECX=height,
//!   EDX=bits per pixel. A newer request replaces a queued one of its kind,
//!   and up to 16 kinds queue. A tool polls
//!   this when the pointer poll reports requests, and every 500 ms or so
//!   while it does not poll the pointer.
//! - 4, report: EBX=kind, ESI and EDI its arguments; EAX=0, or FFFFFFFFh for
//!   an unknown kind. Kind 1 starts the display mode list, discarding the
//!   old one. Kind 2 adds a mode to it (up to 128): ESI=width | height << 16,
//!   EDI=bits per pixel. Kind 3 is a display request's result: ESI=width |
//!   height << 16, EDI=bits | result << 16; result 0 switched, 1 saved for
//!   the next Windows restart, 2 not offered by Windows, 3 failed, 4 added to
//!   the driver's mode list (Windows 9x): restart Windows and request again.
//!
//! Commands 2, 3 and 4 renew a two-second tool lease in guest time.
//! Machine::guest_tools_active() also needs a command 3 or 4 since reset, so
//! a pointer-only tool gets no requests; host requests need it. Reset clears
//! the requests, the reports and the lease.
//!
//! Pointer motion coalesces to the latest position. Up to 64 button/wheel
//! transitions retain their order and positions. A full queue rejects a new
//! transition without changing input state; retry while the tool is active.
//! Motion alone always fits. Each poll renews a 250 ms lease in guest time for
//! Machine::guest_pointer_active(). The first poll, or one after lease expiry,
//! discards queued transitions and returns the latest accepted position and
//! button state, with no stale wheel steps. A tool must stop polling when its
//! desktop cannot receive input. Reset clears the pointer queue and lease.

use super::TICKS_PER_SECOND;
use crate::x86::{Cpu, Reg32};
use alloc::collections::VecDeque;
use alloc::vec::Vec;

const POINTER_QUEUE_LIMIT: usize = 64;
const REQUEST_QUEUE_LIMIT: usize = 16;
const DISPLAY_MODE_LIMIT: usize = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PointerEvent {
    x: u16,
    y: u16,
    buttons: u8,
    wheel: i16,
}

#[derive(Debug, Default)]
pub(super) struct Pointer {
    valid: bool,
    latest: PointerEvent,
    delivered: Option<PointerEvent>,
    transitions: VecDeque<PointerEvent>,
    last_activity: Option<u64>,
    last_poll: Option<u64>,
}

impl Pointer {
    pub fn inject(
        &mut self,
        x: u16,
        y: u16,
        buttons: u8,
        wheel: i16,
        now: u64,
    ) -> Result<(), &'static str> {
        if buttons & !7 != 0 {
            return Err("pointer buttons must be 0..7");
        }
        let transition = buttons != self.latest.buttons || wheel != 0;
        if transition && self.transitions.len() == POINTER_QUEUE_LIMIT {
            return Err("pointer queue full; retry the transition");
        }
        if (x, y) != (self.latest.x, self.latest.y) || buttons != 0 || self.latest.buttons != 0 {
            self.last_activity = Some(now);
        }
        self.valid = true;
        self.latest = PointerEvent {
            x,
            y,
            buttons,
            wheel: 0,
        };
        if transition {
            self.transitions.push_back(PointerEvent {
                wheel,
                ..self.latest
            });
        }
        Ok(())
    }

    fn pending_motion(&self) -> bool {
        self.valid && self.delivered != Some(self.latest)
    }

    fn poll(&mut self, cpu: &mut Cpu, now: u64) {
        if !self.active(now) {
            self.transitions.clear();
            self.delivered = None;
        }
        self.last_poll = Some(now);
        if self.latest.buttons != 0 {
            self.last_activity = Some(now);
        }
        let event = self
            .transitions
            .pop_front()
            .or_else(|| self.pending_motion().then_some(self.latest));
        cpu.set_reg(Reg32::Eax, u32::from(event.is_some()));
        if let Some(event) = event {
            self.delivered = Some(PointerEvent { wheel: 0, ..event });
        }
        let event = event.unwrap_or(self.latest);
        cpu.set_reg(Reg32::Ebx, u32::from(event.x) | (u32::from(event.y) << 16));
        cpu.set_reg(Reg32::Ecx, u32::from(event.buttons));
        cpu.set_reg(Reg32::Edx, i32::from(event.wheel) as u32);
        let hint = if !self.transitions.is_empty() || self.pending_motion() {
            0
        } else if self
            .last_activity
            .is_some_and(|t| now.saturating_sub(t) < TICKS_PER_SECOND)
        {
            10
        } else {
            100
        };
        cpu.set_reg(Reg32::Esi, hint);
    }

    pub fn active(&self, now: u64) -> bool {
        self.last_poll
            .is_some_and(|t| now.saturating_sub(t) < TICKS_PER_SECOND / 4)
    }
}

/// A host request to the guest tool, taken with command 3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    DisplayMode { width: u16, height: u16, bits: u8 },
}

/// A display mode as the guest tool reports it: width, height, bits.
pub type DisplayMode = (u16, u16, u8);

/// The guest tool's answer to a display request (report kind 3); `id`
/// counts results so a frontend can tell a new one from the last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayResult {
    pub id: u32,
    pub mode: DisplayMode,
    pub result: u8,
}

#[derive(Debug)]
pub(super) struct State {
    pub utc_seconds: u64,
    pub utc_offset: i32,
    pub pointer: Pointer,
    requests: VecDeque<Request>,
    pub display_modes: Vec<DisplayMode>,
    pub display_result: Option<DisplayResult>,
    last_tool_call: Option<u64>,
    takes_requests: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            utc_seconds: 1_767_225_600,
            utc_offset: 0,
            pointer: Pointer::default(),
            requests: VecDeque::new(),
            display_modes: Vec::new(),
            display_result: None,
            last_tool_call: None,
            takes_requests: false,
        }
    }
}

impl State {
    pub fn call(&mut self, cpu: &mut Cpu, ticks: u64) {
        let command = cpu.reg(Reg32::Ecx);
        if (2..=4).contains(&command) {
            self.last_tool_call = Some(ticks);
            self.takes_requests |= command != 2;
        }
        match command {
            0 => {
                cpu.set_reg(Reg32::Eax, 1);
                cpu.set_reg(Reg32::Ebx, 7);
            }
            1 => {
                let seconds = self.utc_seconds.saturating_add(ticks / TICKS_PER_SECOND);
                cpu.set_reg(Reg32::Eax, seconds as u32);
                cpu.set_reg(Reg32::Edx, (seconds >> 32) as u32);
                cpu.set_reg(Reg32::Ebx, self.utc_offset as u32);
            }
            2 => {
                self.pointer.poll(cpu, ticks);
                cpu.set_reg(Reg32::Edi, self.requests.len() as u32);
            }
            3 => {
                let (kind, b, c, d) = match self.requests.pop_front() {
                    None => (0, 0, 0, 0),
                    Some(Request::DisplayMode {
                        width,
                        height,
                        bits,
                    }) => (1, width.into(), height.into(), bits.into()),
                };
                cpu.set_reg(Reg32::Eax, kind);
                cpu.set_reg(Reg32::Ebx, b);
                cpu.set_reg(Reg32::Ecx, c);
                cpu.set_reg(Reg32::Edx, d);
            }
            4 => {
                let known = self.report(
                    cpu.reg(Reg32::Ebx),
                    cpu.reg(Reg32::Esi),
                    cpu.reg(Reg32::Edi),
                );
                cpu.set_reg(Reg32::Eax, if known { 0 } else { u32::MAX });
            }
            _ => cpu.set_reg(Reg32::Eax, u32::MAX),
        }
    }

    fn report(&mut self, kind: u32, size: u32, value: u32) -> bool {
        let mode = (size as u16, (size >> 16) as u16, value as u8);
        match kind {
            1 => self.display_modes.clear(),
            2 => {
                if self.display_modes.len() < DISPLAY_MODE_LIMIT
                    && !self.display_modes.contains(&mode)
                {
                    self.display_modes.push(mode);
                }
            }
            3 => {
                let id = self.display_result.map_or(1, |r| r.id.wrapping_add(1));
                self.display_result = Some(DisplayResult {
                    id,
                    mode,
                    result: (value >> 16) as u8,
                });
            }
            _ => return false,
        }
        true
    }

    pub fn tools_active(&self, now: u64) -> bool {
        self.takes_requests
            && self
                .last_tool_call
                .is_some_and(|t| now.saturating_sub(t) < 2 * TICKS_PER_SECOND)
    }

    pub fn request(&mut self, request: Request) -> Result<(), &'static str> {
        let kind = core::mem::discriminant(&request);
        self.requests
            .retain(|queued| core::mem::discriminant(queued) != kind);
        if self.requests.len() == REQUEST_QUEUE_LIMIT {
            return Err("guest request queue full; retry");
        }
        self.requests.push_back(request);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::Machine;
    use crate::x86::{Bus, Interpreter};

    fn poll(pointer: &mut Pointer, now: u64) -> (bool, u16, u16, u8, i32, u32) {
        let mut cpu = Cpu::new();
        pointer.poll(&mut cpu, now);
        (
            cpu.reg(Reg32::Eax) != 0,
            cpu.reg(Reg32::Ebx) as u16,
            (cpu.reg(Reg32::Ebx) >> 16) as u16,
            cpu.reg(Reg32::Ecx) as u8,
            cpu.reg(Reg32::Edx) as i32,
            cpu.reg(Reg32::Esi),
        )
    }

    #[test]
    fn magic_pointer_order_motion_and_sleep_hints() {
        let mut p = Pointer::default();
        assert_eq!(poll(&mut p, 0), (false, 0, 0, 0, 0, 100));
        p.inject(0, 0, 0, 0, 1).unwrap();
        assert_eq!(poll(&mut p, 1), (true, 0, 0, 0, 0, 100));
        p.inject(10, 20, 0, 0, 2).unwrap();
        p.inject(30, 40, 0, 0, 3).unwrap();
        assert_eq!(poll(&mut p, 3), (true, 30, 40, 0, 0, 10));
        p.inject(31, 41, 1, 0, 4).unwrap();
        p.inject(32, 42, 1, -3, 5).unwrap();
        p.inject(33, 43, 0, 0, 6).unwrap();
        p.inject(u16::MAX, u16::MAX, 0, 0, 7).unwrap();
        assert_eq!(poll(&mut p, 7), (true, 31, 41, 1, 0, 0));
        assert_eq!(poll(&mut p, 7), (true, 32, 42, 1, -3, 0));
        assert_eq!(poll(&mut p, 7), (true, 33, 43, 0, 0, 0));
        assert_eq!(poll(&mut p, 7), (true, 65535, 65535, 0, 0, 10));
        assert_eq!(poll(&mut p, TICKS_PER_SECOND + 7).5, 100);
        p.inject(65535, 65535, 4, 0, 2 * TICKS_PER_SECOND).unwrap();
        assert_eq!(poll(&mut p, 10 * TICKS_PER_SECOND).5, 10);
        p.inject(65535, 65535, 0, 0, 11 * TICKS_PER_SECOND).unwrap();
        assert_eq!(poll(&mut p, 11 * TICKS_PER_SECOND).5, 10);
        assert_eq!(poll(&mut p, 12 * TICKS_PER_SECOND - 1).5, 10);
        assert_eq!(poll(&mut p, 12 * TICKS_PER_SECOND).5, 100);
    }

    #[test]
    fn magic_pointer_full_queue_rejects_transition_without_losing_edges() {
        let mut p = Pointer::default();
        poll(&mut p, 0);
        for i in 0..POINTER_QUEUE_LIMIT {
            p.inject(i as u16, 0, (i % 2 + 1) as u8, i as i16, 0)
                .unwrap();
        }
        assert!(p.inject(999, 999, 0, 100, 0).is_err());
        assert!(p.inject(999, 999, 8, 0, 0).is_err());
        p.inject(65535, 65535, 2, 0, 0).unwrap(); // Motion never consumes a slot.
        for i in 0..POINTER_QUEUE_LIMIT {
            assert_eq!(
                poll(&mut p, 0),
                (true, i as u16, 0, (i % 2 + 1) as u8, i as i32, 0)
            );
        }
        assert_eq!(poll(&mut p, 0), (true, 65535, 65535, 2, 0, 10));
        p.inject(999, 999, 0, 100, 0).unwrap(); // Retry the rejected release.
        assert_eq!(poll(&mut p, 0), (true, 999, 999, 0, 100, 10));
        assert!(!poll(&mut p, 0).0);
    }

    #[test]
    fn magic_pointer_expired_lease_returns_state_without_stale_edges() {
        let mut p = Pointer::default();
        p.inject(10, 20, 1, 3, 0).unwrap();
        p.inject(30, 40, 0, -2, 0).unwrap();
        assert_eq!(poll(&mut p, 0), (true, 30, 40, 0, 0, 10));
        assert!(!poll(&mut p, 0).0);

        p.inject(50, 60, 1, 2, 1).unwrap();
        p.inject(70, 80, 0, 1, 2).unwrap();
        let expires = TICKS_PER_SECOND / 4;
        assert_eq!(poll(&mut p, expires - 1), (true, 50, 60, 1, 2, 0));
        p.inject(90, 100, 4, 5, expires).unwrap();
        assert_eq!(poll(&mut p, 2 * expires - 1), (true, 90, 100, 4, 0, 10));
        assert!(!poll(&mut p, 2 * expires - 1).0);
    }

    #[test]
    fn magic_pointer_machine_lease_and_reset() {
        let mut m = Machine::new(0x10000);
        assert!(!m.guest_pointer_active());
        m.inject_pointer(65535, 0, 1, 1).unwrap();
        m.cpu.set_reg(Reg32::Ecx, 2);
        m.bus.magic_instruction(&mut m.cpu);
        assert!(m.guest_pointer_active());
        m.step = TICKS_PER_SECOND / 4 - 1;
        assert!(m.guest_pointer_active());
        m.step += 1;
        assert!(!m.guest_pointer_active());
        m.bus.current_step = m.step;
        m.cpu.set_reg(Reg32::Ecx, 2);
        m.bus.magic_instruction(&mut m.cpu);
        assert!(m.guest_pointer_active());
        m.bus.reset(m.step);
        assert!(!m.guest_pointer_active());
        m.cpu.set_reg(Reg32::Ecx, 2);
        m.bus.magic_instruction(&mut m.cpu);
        assert_eq!(m.cpu.reg(Reg32::Eax), 0);
    }

    #[test]
    fn magic_instruction_version_and_sampled_time() {
        let mut m = Machine::new(0x10000);
        m.cpu.enter_protected_flat(0, 0);
        m.bus.ram[0] = 0xed;
        m.cpu.set_reg(Reg32::Eax, 0x2136_3872);
        m.cpu.set_reg(Reg32::Edx, 0x5286);
        m.cpu.set_reg(Reg32::Ecx, 0);
        Interpreter::step(&mut m.cpu, &mut m.bus).unwrap();
        assert_eq!(m.cpu.reg(Reg32::Eax), 1);
        assert_eq!(m.cpu.reg(Reg32::Ebx), 7);

        m.bus.magic.utc_seconds = 0x1_ffff_fffe;
        m.bus.magic.utc_offset = -8 * 3600;
        m.bus.current_step = 3 * TICKS_PER_SECOND;
        m.cpu.set_reg(Reg32::Ecx, 1);
        m.bus.magic_instruction(&mut m.cpu);
        assert_eq!(m.cpu.reg(Reg32::Eax), 1);
        assert_eq!(m.cpu.reg(Reg32::Edx), 2);
        assert_eq!(m.cpu.reg(Reg32::Ebx) as i32, -8 * 3600);
        m.bus.rtc_epoch_seconds = 0;
        m.bus.reset(m.bus.current_step);
        m.bus.magic_instruction(&mut m.cpu);
        assert_eq!(m.cpu.reg(Reg32::Eax), 1);
        assert_eq!(m.cpu.reg(Reg32::Edx), 2);
    }

    fn call(state: &mut State, command: u32, ebx: u32, esi: u32, edi: u32, now: u64) -> [u32; 5] {
        let mut cpu = Cpu::new();
        for (reg, value) in [
            (Reg32::Ecx, command),
            (Reg32::Ebx, ebx),
            (Reg32::Esi, esi),
            (Reg32::Edi, edi),
        ] {
            cpu.set_reg(reg, value);
        }
        state.call(&mut cpu, now);
        [Reg32::Eax, Reg32::Ebx, Reg32::Ecx, Reg32::Edx, Reg32::Edi].map(|reg| cpu.reg(reg))
    }

    #[test]
    fn magic_requests_and_reports() {
        let mut s = State::default();
        let mode = Request::DisplayMode {
            width: 640,
            height: 340,
            bits: 16,
        };
        // A pointer-only (version 2) tool takes no requests.
        call(&mut s, 2, 0, 0, 0, 0);
        assert!(!s.tools_active(0));
        assert_eq!(call(&mut s, 3, 0, 0, 0, 1)[0], 0);
        assert!(s.tools_active(1));
        assert!(!s.tools_active(1 + 2 * TICKS_PER_SECOND));

        s.request(Request::DisplayMode {
            width: 800,
            height: 600,
            bits: 32,
        })
        .unwrap();
        s.request(mode).unwrap(); // replaces the queued display request
        assert_eq!(call(&mut s, 2, 0, 0, 0, 2)[4], 1);
        assert_eq!(call(&mut s, 3, 0, 0, 0, 2)[..4], [1, 640, 340, 16]);
        assert_eq!(call(&mut s, 3, 0, 0, 0, 2)[0], 0);
        assert_eq!(call(&mut s, 2, 0, 0, 0, 2)[4], 0);

        let size = 640 | (340 << 16);
        assert_eq!(call(&mut s, 4, 1, 0, 0, 3)[0], 0);
        call(&mut s, 4, 2, size, 16, 3);
        call(&mut s, 4, 2, size, 16, 3);
        call(&mut s, 4, 2, 320 | (170 << 16), 32, 3);
        assert_eq!(s.display_modes, [(640, 340, 16), (320, 170, 32)]);
        call(&mut s, 4, 1, 0, 0, 3);
        assert!(s.display_modes.is_empty());
        call(&mut s, 4, 3, size, 16 | (2 << 16), 3);
        call(&mut s, 4, 3, size, 16, 3);
        assert_eq!(
            s.display_result,
            Some(DisplayResult {
                id: 2,
                mode: (640, 340, 16),
                result: 0
            })
        );
        assert_eq!(call(&mut s, 4, 9, 0, 0, 3)[0], u32::MAX);
    }
}
