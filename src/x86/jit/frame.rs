//! Invocation ABI shared by ARM64 generated code and its memory adapter.
//!
//! W19..W26 hold EAX..EDI, X27 holds the instruction budget after reserving the
//! current block, and X28 points to this frame. Block exits refund uncompleted
//! steps. The entry preserves all AAPCS64 callee-saved GPRs it touches.
//! X18 is reserved to the platform. Calls may clobber X0..X17 and NZCV; lowering
//! must spill live temporary/flag state explicitly before a helper call.
//! SoftMMU reserves W7 for packed segment permissions and X13/X16 for its
//! immutable translation context; X16's high half also holds the CS base.
//! X6 holds the native return-cache top; helpers must preserve it explicitly.
//! X29 always names the entry's saved X29/X30 frame record. Native CALL edges
//! and cold memory continuations may overwrite LR without changing that record.
//! Every SP adjustment is a multiple of 16; generated code leaves D8..D15 alone.
//! Native-called Rust helpers take only u32/u64/pointers: Darwin requires the
//! caller to extend narrower arguments. X18 remains reserved, including in tests.
//! W15 carries N/Z/C/V in bits 31/30/29/28, with C meaning x86 carry/borrow.
//! W17 carries a byte whose parity gives PF; bit 4 of W8 XOR W17 gives AF. Non-status
//! EFLAGS remain in Frame.eflags; CLD/STD, CLI/STI and POPF update their permitted
//! bits. POPF enabling IF and STI exit for machine interrupt attention. The
//! adapter handles STI's IF 0->1 exit by installing the shadow before a machine
//! interrupt check.
//! These registers survive
//! links and memory guards. Only
//! the exit materializes both into Frame.eflags.
//! W14 carries the next x86 PC at dispatch/return boundaries. Direct same-page
//! edges skip its materialization; budget/memory exits name their own exact PC.
//! Frame.remaining and Frame.tsc
//! retain the invocation's starting values until exit: completed steps are the
//! initial budget minus the refunded X27. FaultSite.steps_adjust describes that
//! refund for each native access. Each exit uses its guest PC,
//! since CALL may already have its target in W14. Deferred updates never change
//! a machine event deadline.
//!
//! The adapter uses this explicit representation and may not embed Rust
//! object offsets into emitted code.
//! Only one invocation owns the live CPU state. Commit/reload at a helper or
//! machine boundary; do not let Rust observers read stale architectural state.

use super::a64::{Assembler, Reg, SP};
use crate::x86::cpu::Cpu;
use core::ffi::c_void;
use core::mem::offset_of;

pub const BUDGET: Reg = Reg::new(27);
pub const FRAME: Reg = Reg::new(28);
pub const RETURN_TOP: Reg = Reg::new(6);
pub const FLAGS: Reg = Reg::new(15);
pub const PARITY: Reg = Reg::new(17);
pub const AUXILIARY: Reg = Reg::new(8);
pub const PC: Reg = Reg::new(14);
// Block-local translation proofs can survive unrelated memory operations.
// Each slot is written by its dominating guard before any read; block links
// enter at the first instruction and cannot enter a proof's interior.
pub(super) const PAGE_BIAS_OFFSET: u32 = 96;
pub(super) const PAGE_BIAS_SLOTS: u8 = 16;
const SAVED_GPR_BYTES: u16 = 96 + PAGE_BIAS_SLOTS as u16 * 8;

/// Integer state at the Rust/native boundary. Exit fields use integer tags so
/// an invalid generated tag can be reported without creating an invalid Rust enum.
#[repr(C)]
pub struct Frame {
    pub regs: [u32; 8],
    pub eip: u32,
    pub eflags: u32,
    pub tsc: u64,
    pub remaining: u64,
    pub completed: u64,
    /// Kind 5 requests attention; kind 6 also requires the adapter to install
    /// STI's interrupt shadow before handing control back to the machine.
    pub exit_kind: u32,
    /// Exit kind 3 resumes the unfinished REP instruction after native memory
    /// elements. While running, completed temporarily stores its starting ECX.
    pub repeat: crate::x86::interp::RepeatContinuation,
    pub memory: *mut c_void,
    pub native_entry: usize,
    pub dispatch_entry: usize,
    pub return_entry: usize,
    pub recheck_entry: usize,
    /// Live SLOTS-entry table when dispatch_entry uses the shared dispatcher.
    pub links: *mut super::links::Link,
    pub link_rechecks: *const crate::x86::cpu::TranslationRecheck,
    pub link_context: u64,
    pub popf_mask: u32,
    pub selectors: [u16; 6],
    /// Nonnull, owned return stack for any invocation that can execute CALL.
    pub call_base: *mut super::links::ReturnStack,
    /// Shared CALL dispatcher, with the same validation as dispatch_entry.
    pub call_dispatch_entry: usize,
    #[cfg(any(test, feature = "jit-profile"))]
    pub memory_reads: u64,
    #[cfg(any(test, feature = "jit-profile"))]
    pub memory_writes: u64,
    #[cfg(any(test, feature = "jit-profile"))]
    pub rechecked_data: u64,
    #[cfg(any(test, feature = "jit-profile"))]
    pub rechecked_code: u64,
    #[cfg(any(test, feature = "jit-profile"))]
    pub refilled_data: u64,
    #[cfg(any(test, feature = "jit-profile"))]
    pub cached_data_refills: u64,
    #[cfg(any(test, feature = "jit-profile"))]
    pub cached_returns: u64,
    #[cfg(any(test, feature = "jit-profile"))]
    pub call_overflows: u64,
    #[cfg(any(test, feature = "jit-profile"))]
    pub loaded_segments: u64,
    #[cfg(feature = "jit-profile")]
    pub memory_exit_reason: u32,
    #[cfg(feature = "jit-profile")]
    pub memory_expected_tag: u64,
    #[cfg(feature = "jit-profile")]
    pub memory_cached_tag: u64,
}

impl Frame {
    pub fn from_cpu(cpu: &Cpu, budget: u64) -> Self {
        Self {
            regs: cpu.regs,
            eip: cpu.eip,
            eflags: cpu.effective_eflags(),
            tsc: cpu.tsc(),
            remaining: budget,
            completed: 0,
            exit_kind: 0,
            repeat: Default::default(),
            memory: core::ptr::null_mut(),
            native_entry: 0,
            dispatch_entry: 0,
            return_entry: 0,
            recheck_entry: 0,
            links: core::ptr::null_mut(),
            link_rechecks: core::ptr::null(),
            link_context: 0,
            popf_mask: crate::x86::interp::popfd_change_mask(cpu).unwrap_or(0),
            selectors: core::array::from_fn(|i| {
                cpu.seg(crate::x86::cpu::SegmentReg::from_modrm_reg(i as u8))
                    .selector
            }),
            call_base: core::ptr::null_mut(),
            call_dispatch_entry: 0,
            #[cfg(any(test, feature = "jit-profile"))]
            memory_reads: 0,
            #[cfg(any(test, feature = "jit-profile"))]
            memory_writes: 0,
            #[cfg(any(test, feature = "jit-profile"))]
            rechecked_data: 0,
            #[cfg(any(test, feature = "jit-profile"))]
            rechecked_code: 0,
            #[cfg(any(test, feature = "jit-profile"))]
            refilled_data: 0,
            #[cfg(any(test, feature = "jit-profile"))]
            cached_data_refills: 0,
            #[cfg(any(test, feature = "jit-profile"))]
            cached_returns: 0,
            #[cfg(any(test, feature = "jit-profile"))]
            call_overflows: 0,
            #[cfg(any(test, feature = "jit-profile"))]
            loaded_segments: 0,
            #[cfg(feature = "jit-profile")]
            memory_exit_reason: 0,
            #[cfg(feature = "jit-profile")]
            memory_expected_tag: 0,
            #[cfg(feature = "jit-profile")]
            memory_cached_tag: 0,
        }
    }

    /// Transfer resident data state. The adapter must then consume exit_kind,
    /// including STI's shadow, before allowing machine events or another opcode.
    pub fn commit(&self, cpu: &mut Cpu) {
        cpu.regs = self.regs;
        cpu.eip = self.eip;
        cpu.jit_commit_flags(self.eflags);
        cpu.set_tsc(self.tsc);
    }
}

pub const fn guest_reg(index: usize) -> Reg {
    assert!(index < 8);
    Reg::new(19 + index as u8)
}

/// Enter once per native invocation. Chained blocks will use the resident
/// registers directly and branch to a shared exit rather than repeat this work.
pub fn emit_entry(code: &mut Assembler) {
    code.add_sub_imm(SP, SP, SAVED_GPR_BYTES, true, true, false);
    for pair in 0..6 {
        code.pair(
            false,
            Reg::new(19 + pair * 2),
            Reg::new(20 + pair * 2),
            SP,
            i16::from(pair) * 16,
            true,
        );
    }
    code.add_sub_imm(Reg::new(29), SP, 80, true, false, false);
    code.mov(FRAME, Reg::new(0), true);
    for pair in 0..4 {
        code.pair(
            true,
            guest_reg(pair * 2),
            guest_reg(pair * 2 + 1),
            FRAME,
            (offset_of!(Frame, regs) + pair * 8) as i16,
            false,
        );
    }
    code.load_store(true, BUDGET, FRAME, offset_of!(Frame, remaining) as u32, 3);
    code.load_store(
        true,
        RETURN_TOP,
        FRAME,
        offset_of!(Frame, call_base) as u32,
        3,
    );
}

/// Exit after lowering has stored the exact guest PC, flags and progress.
/// Restoring these GPRs must not depend on the guest's ESP or segment state.
pub fn emit_return(code: &mut Assembler) {
    for pair in 0..4 {
        code.pair(
            false,
            guest_reg(pair * 2),
            guest_reg(pair * 2 + 1),
            FRAME,
            (offset_of!(Frame, regs) + pair * 8) as i16,
            false,
        );
    }
    code.load_store(false, BUDGET, FRAME, offset_of!(Frame, remaining) as u32, 3);
    for pair in 0..6 {
        code.pair(
            true,
            Reg::new(19 + pair * 2),
            Reg::new(20 + pair * 2),
            SP,
            i16::from(pair) * 16,
            true,
        );
    }
    code.add_sub_imm(SP, SP, SAVED_GPR_BYTES, true, false, false);
    code.ret();
}

/// Native tests invalidate every caller-saved GPR after a Rust helper. Keep
/// only its declared X0 result, so missing spills cannot depend on Rust's
/// incidental register allocation. No poison instructions enter release code.
#[cfg(test)]
pub(super) fn poison_helper_return(code: &mut Assembler, result: bool) {
    if result {
        code.add_sub_imm(SP, SP, 16, true, true, false);
        code.load_store(false, Reg::new(0), SP, 0, 3);
    }
    for index in 0..18 {
        code.mov_imm(
            Reg::new(index),
            0xbad0_0000_0000_0000 | u64::from(index),
            true,
        );
    }
    if result {
        code.load_store(true, Reg::new(0), SP, 0, 3);
        code.add_sub_imm(SP, SP, 16, true, false, false);
    }
}
