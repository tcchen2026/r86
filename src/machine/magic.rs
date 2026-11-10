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
//! - 0, version: EAX=protocol version (1), EBX=capabilities (bit 0: time).
//! - 1, time: EDX:EAX=unsigned UTC seconds since 1970-01-01, EBX=signed
//!   seconds east of UTC. The host epoch and offset are captured once when
//!   realtime mode starts; elapsed time uses the already sampled machine
//!   clock. Deterministic runs start at 2026-01-01 UTC. RTC writes do not
//!   change this time, and a machine reset preserves its epoch.

use super::TICKS_PER_SECOND;
use crate::x86::{Cpu, Reg32};

#[derive(Debug)]
pub(super) struct State {
    pub utc_seconds: u64,
    pub utc_offset: i32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            utc_seconds: 1_767_225_600,
            utc_offset: 0,
        }
    }
}

impl State {
    pub fn call(&mut self, cpu: &mut Cpu, ticks: u64) {
        match cpu.reg(Reg32::Ecx) {
            0 => {
                cpu.set_reg(Reg32::Eax, 1);
                cpu.set_reg(Reg32::Ebx, 1);
            }
            1 => {
                let seconds = self.utc_seconds.saturating_add(ticks / TICKS_PER_SECOND);
                cpu.set_reg(Reg32::Eax, seconds as u32);
                cpu.set_reg(Reg32::Edx, (seconds >> 32) as u32);
                cpu.set_reg(Reg32::Ebx, self.utc_offset as u32);
            }
            _ => cpu.set_reg(Reg32::Eax, u32::MAX),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::Machine;
    use crate::x86::{Bus, Interpreter};

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
        assert_eq!(m.cpu.reg(Reg32::Ebx), 1);

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
