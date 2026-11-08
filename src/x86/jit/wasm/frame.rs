//! Explicit Wasm32 invocation record. All ABI addresses are linear-memory u32s.
//! GPRs, status and budget travel in Wasm arguments across module tail calls;
//! only a return to Rust commits this record. TLB misses also record their
//! address/intent for the core; shared interpretation owns the unfinished access.
//! Engine owns its stable allocation until its modules are retired. Generated
//! functions embed this linear address; the imported memory's minimum size
//! lets the host compiler prove that these internal field stores are in bounds.

#[derive(Default)]
#[repr(C)]
pub struct Frame {
    pub regs: [u32; 8],
    pub eip: u32,
    pub eflags: u32,
    pub remaining: u32,
    pub exit: u32, // 0: dispatch, 1: semantic boundary, 2: budget, 3: IRQ, 4: STI shadow, 5: unsupported instruction, 6: REP continuation, 7: data-segment load
    pub tlb: u32,
    pub tlb_state: u32,
    pub permissions: u32,
    pub bases: [u32; 2],
    pub limits: [u32; 2],
    pub selectors: [u32; 6],
    pub popf_mask: u32,
    pub cs_base: u32,
    pub repeat: crate::x86::interp::RepeatContinuation,
    pub refill_linear: u32,
    pub refill_intent: u32, // 0: none, 1: read, 2: write; cold TLB miss
    pub flag_lhs: u32,
    pub flag_result: u32,
    pub flag_kind: u32, // same deferred arithmetic record as the invocation ABI
    pub segment_load: SegmentLoad,
}

/// A qualified selector operand, before any segment effect or instruction
/// charge. The core tries the shared cached-descriptor path after returning
/// from generated code and restoring the RAM-only TLB. On a miss, EIP and
/// budget still name the original instruction for ordinary interpretation.
#[derive(Default)]
#[repr(C)]
pub struct SegmentLoad {
    pub segment: u32,
    pub selector: u32,
    pub next_eip: u32,
}

pub const LINK_SLOTS: usize = 1024;
pub const LINK_PLANES: usize = 2; // supervisor and user execute proofs
pub const REGION_SLOTS: usize = 4096;
pub const GROUP_BITS: u32 = 3;
pub const GROUP_FUNCTIONS: usize = 1 << GROUP_BITS;
pub const ENTRY_BITS: u32 = 5;
pub const ENTRY_MASK: u32 = (1 << ENTRY_BITS) - 1;

/// One invocation requires flat, readable/writable DS/ES/SS and a 32-bit stack.
/// Segment loads return to the core and end this proof, so it holds across
/// every generated edge. FS/GS retain their per-access base/limit guards.
pub const ENTRY_SEGMENTS: u32 = {
    use crate::x86::SegmentReg;
    let data =
        (1 << SegmentReg::Ds as u32) | (1 << SegmentReg::Es as u32) | (1 << SegmentReg::Ss as u32);
    data | (data << 8) | (1 << 6)
};

/// Published execute proof. Separate supervisor/user planes survive CPL
/// transitions; mapping and CS-base changes invalidate both. The source owner
/// outlives every invocation, and writes invalidate its version.
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Link {
    pub tag: u32,
    pub function: u32,
    pub entries: u32,
    pub version_ptr: u32,
    pub version: u64,
    pub page: u32, // owning page slot; also qualifies untranslated entries
}

#[cfg(any(test, target_arch = "wasm32"))]
impl Frame {
    /// The shared Cpu uses this same arithmetic record; committing it needs
    /// no operation/width conversion or flag calculation. Non-status flags
    /// (including IF) commit immediately.
    pub(crate) fn commit_flags(&self, cpu: &mut crate::x86::Cpu) {
        cpu.jit_commit_arithmetic(self.eflags, self.flag_lhs, self.flag_result, self.flag_kind);
    }
}

#[cfg(target_arch = "wasm32")]
impl Frame {
    /// Refresh inputs only. Registers, flags and budget enter as arguments;
    /// every generated return writes their output fields. Clearing/rebuilding
    /// that output storage before each invocation would copy it twice. The
    /// core can reuse these inputs across native-only dispatches in one batch;
    /// interpretation and Machine returns always end that proof.
    pub(crate) fn prepare(&mut self, cpu: &mut crate::x86::Cpu) {
        let (tlb, tlb_state) = cpu.translation_cache.jit_view();
        let (read, write) = cpu.jit_segment_masks();
        let (bases, limits, based_read, based_write) = cpu.jit_based_segments();
        self.tlb = tlb as u32;
        self.tlb_state = tlb_state as u32;
        self.permissions = read | (write << 8) | (based_read << 16) | (based_write << 24);
        self.bases = bases;
        self.limits = limits;
        self.selectors = core::array::from_fn(|i| {
            u32::from(
                cpu.seg(crate::x86::SegmentReg::from_modrm_reg(i as u8))
                    .selector,
            )
        });
        self.popf_mask = crate::x86::interp::popfd_change_mask(cpu)
            .expect("Wasm entry excludes VM86 POPF failure");
        // POPF replaces every arithmetic flag in both operand sizes and at
        // every admitted privilege level, so old status need not materialize.
        debug_assert_eq!(self.popf_mask & 0x8d5, 0x8d5);
        self.cs_base = cpu.seg(crate::x86::SegmentReg::Cs).base;
        // The address and REP record are read only when a generated exit has
        // written them. Intent alone must not survive a previous invocation.
        self.refill_intent = 0;
    }
}
