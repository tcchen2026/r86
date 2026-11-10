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
//!   bit 1: pointer).
//! - 1, time: EDX:EAX=unsigned UTC seconds since 1970-01-01, EBX=signed
//!   seconds east of UTC. The host epoch and offset are captured once when
//!   realtime mode starts; elapsed time uses the already sampled machine
//!   clock. Deterministic runs start at 2026-01-01 UTC. RTC writes do not
//!   change this time, and a machine reset preserves its epoch.
//! - 2, pointer poll: EAX=1 if an event was returned, else 0; EBX=x in the
//!   low 16 bits and y in the high 16 bits (both 0..65535); ECX=buttons
//!   (bits 0/1/2: left/right/middle); EDX=signed wheel steps; ESI=next sleep
//!   in milliseconds. Empty polls return the last delivered position/buttons
//!   and zero wheel. Sleep is 0 while events remain, 10 while a button is
//!   held or within one second of movement/a held button, otherwise 100.
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

const POINTER_QUEUE_LIMIT: usize = 64;

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

#[derive(Debug)]
pub(super) struct State {
    pub utc_seconds: u64,
    pub utc_offset: i32,
    pub pointer: Pointer,
}

impl Default for State {
    fn default() -> Self {
        Self {
            utc_seconds: 1_767_225_600,
            utc_offset: 0,
            pointer: Pointer::default(),
        }
    }
}

impl State {
    pub fn call(&mut self, cpu: &mut Cpu, ticks: u64) {
        match cpu.reg(Reg32::Ecx) {
            0 => {
                cpu.set_reg(Reg32::Eax, 1);
                cpu.set_reg(Reg32::Ebx, 3);
            }
            1 => {
                let seconds = self.utc_seconds.saturating_add(ticks / TICKS_PER_SECOND);
                cpu.set_reg(Reg32::Eax, seconds as u32);
                cpu.set_reg(Reg32::Edx, (seconds >> 32) as u32);
                cpu.set_reg(Reg32::Ebx, self.utc_offset as u32);
            }
            2 => self.pointer.poll(cpu, ticks),
            _ => cpu.set_reg(Reg32::Eax, u32::MAX),
        }
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
        assert_eq!(m.cpu.reg(Reg32::Ebx), 3);

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
}
