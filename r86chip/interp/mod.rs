//! Build-time chip primitives for the one x86 interpreter in `src/x86/interp.rs`.
//! This module owns no x86 decoder, dispatch loop, GPR frame or fallback engine.
//! Ordinary builds use software operations; RV32 `r86chip-native` builds replace
//! selected operations with instructions defined by the Rust ISS in sim/rv32.rs.

#[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
pub mod native;

/// Firmware observation counters, not architectural CPU state or guest time.
/// Only r86chip builds contain them. Native counts include the instructions
/// maintaining these counters; no work is subtracted from acceptance results.
#[derive(Clone, Copy, Debug, Default)]
pub struct Counts {
    pub completed: u64,
    pub fault_attempts: u64,
    pub repeat_chunks: u64,
    pub halted_polls: u64,
}

impl Counts {
    pub const ZERO: Self = Self {
        completed: 0,
        fault_attempts: 0,
        repeat_chunks: 0,
        halted_polls: 0,
    };

    /// Attempts which did not complete an x86 instruction. Observe at batch
    /// boundaries: completed = attempts - these events, including partial REP.
    /// Wrapping arithmetic permits long runs without a diagnostic overflow
    /// changing guest behavior; differences remain valid across counter wrap.
    #[inline(always)]
    pub(crate) fn incomplete(&self) -> u64 {
        self.fault_attempts
            .wrapping_add(self.repeat_chunks)
            .wrapping_add(self.halted_polls)
    }
}

#[cfg(test)]
mod tests {
    use crate::x86::{Bus, Cpu, MemError, PhysAddr};
    struct Ram(alloc::vec::Vec<u8>);
    impl Bus for Ram {
        fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
            self.0
                .get(addr.0 as usize)
                .copied()
                .ok_or(MemError::OutOfRange { addr })
        }
        fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError> {
            if addr.0 < self.0.len() as u64 {
                Ok(())
            } else {
                Err(MemError::OutOfRange { addr })
            }
        }
        fn write_phys_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
            self.probe_phys_write_u8(addr)?;
            self.0[addr.0 as usize] = value;
            Ok(())
        }
    }
    #[test]
    fn completion_counts_exclude_rep_chunks_faults_and_halted_polling() {
        use crate::x86::{Interpreter, Reg32};
        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(0, 0);
        let mut ram = Ram(alloc::vec![0; 0x5000]);
        ram.0[..4].copy_from_slice(&[0xf3, 0xaa, 0xeb, 0xfe]);
        cpu.set_reg(
            Reg32::Ecx,
            crate::x86::interp::MAX_STRING_REPEAT_UNITS_PER_STEP + 2,
        );
        cpu.set_reg(Reg32::Edi, 0x1000);
        Interpreter::step(&mut cpu, &mut ram).unwrap();
        assert_eq!(
            (
                cpu.eip,
                cpu.chip_counts.completed,
                cpu.chip_counts.repeat_chunks
            ),
            (0, 0, 1)
        );
        Interpreter::step(&mut cpu, &mut ram).unwrap();
        assert_eq!(
            (cpu.eip, cpu.chip_counts.completed, cpu.reg(Reg32::Ecx)),
            (2, 1, 0)
        );
        Interpreter::step(&mut cpu, &mut ram).unwrap(); // completed self-branch
        assert_eq!((cpu.eip, cpu.chip_counts.completed), (2, 2));
        ram.0[2] = 0xf4;
        Interpreter::step(&mut cpu, &mut ram).unwrap(); // executing HLT counts once
        Interpreter::step(&mut cpu, &mut ram).unwrap(); // waiting while halted does not
        assert_eq!(
            (cpu.chip_counts.completed, cpu.chip_counts.halted_polls),
            (3, 1)
        );
        cpu.halted = false;
        ram.0[3..5].copy_from_slice(&[0xf0, 0x90]); // invalid LOCK NOP
        assert!(Interpreter::step(&mut cpu, &mut ram).is_err());
        assert_eq!(
            (cpu.chip_counts.completed, cpu.chip_counts.fault_attempts),
            (3, 1)
        );
    }

    #[test]
    fn batched_completion_counts_include_hlt_once_and_rep_once() {
        use crate::x86::{BatchStop, Interpreter, Reg32};
        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(0, 0);
        let mut ram = Ram(alloc::vec![0; 0x5000]);
        ram.0[..5].copy_from_slice(&[0x90, 0xf3, 0xaa, 0x90, 0xf4]);
        cpu.set_reg(
            Reg32::Ecx,
            2 * crate::x86::interp::MAX_STRING_REPEAT_UNITS_PER_STEP + 1,
        );
        cpu.set_reg(Reg32::Edi, 0x1000);
        // Exercise cumulative observations across counter wrap as well as
        // multiple incomplete and complete operations in one execution batch.
        cpu.chip_counts.completed = u64::MAX - 3;
        cpu.chip_counts.repeat_chunks = u64::MAX - 1;
        let result = Interpreter::run_batch(&mut cpu, &mut ram, 0, 64);
        assert!(matches!(result.stop, BatchStop::Step { .. }));
        assert_eq!((result.ran, cpu.tsc(), cpu.eip), (6, 6, 5));
        assert_eq!(cpu.chip_counts.completed, 0); // NOP, REP, NOP, HLT
        assert_eq!(cpu.chip_counts.repeat_chunks, 0); // two incomplete chunks
        assert_eq!(cpu.reg(Reg32::Ecx), 0);
        assert!(cpu.halted);
        assert_eq!(Interpreter::run_batch(&mut cpu, &mut ram, 6, 0).ran, 0);
        assert_eq!(cpu.chip_counts.halted_polls, 0);
        assert_eq!(Interpreter::run_batch(&mut cpu, &mut ram, 6, 64).ran, 1);
        assert_eq!(
            (cpu.chip_counts.completed, cpu.chip_counts.halted_polls),
            (0, 1)
        );
    }

    #[test]
    fn fault_after_completed_instructions_does_not_discard_their_count() {
        use crate::x86::{BatchStop, Interpreter};
        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(0, 0);
        let mut ram = Ram(alloc::vec![0x90, 0x90, 0xf0, 0x90]);
        let result = Interpreter::run_batch(&mut cpu, &mut ram, 0, 64);
        assert!(matches!(result.stop, BatchStop::Step { .. }));
        assert_eq!(result.ran, 3);
        assert_eq!(
            (cpu.chip_counts.completed, cpu.chip_counts.fault_attempts),
            (2, 1)
        );
    }
}
