//! AArch64 data access qualification. This file owns segment/range checks and
//! software TLB lowering.
//! The x86 decoder/ALU and precise exits do not choose a memory backend.
//!
//! A complete access requires current privilege, paging and segment permission.
//! DS/ES/SS remain flat-only; FS/GS allow normal data segments with any base and
//! limit. Offset+width is validated before wrapping base addition, without
//! extra work on ordinary flat accesses (v86 cpu.rs get_seg checks no bounds).
//!
//! RAM hits use address-biased tokens. Cold paths can revalidate exact PDE/PTE
//! witnesses or call the shared probe to fill a translation with already-set
//! A/D bits. Watched code never receives a native write token. Page-table RAM
//! stores need no write barrier: architectural invalidation expires old mappings.
//! The probe may also return an uncached linear-framebuffer pointer after dirty
//! notification; it never publishes that pointer in a RAM or execute cache.
//! Banked/planar VGA and other devices retain their ordinary callbacks.
//!
//! Failed probes, split pages and complex segments return before the scalar
//! instruction. Shared semantics perform A/D updates, device effects and fault
//! delivery. There is no side-effecting read to replay. REP commits complete
//! elements and resumes with its completed count on a slow next access.
//!
//! The host adapter supplies this view only for HostBus's stable pointer tokens,
//! under exclusive machine ownership. It stays alive through the invocation.

use super::a64::{Assembler, Cond, LR, Label, Reg, SP, ZR};
use super::decode::Address;
use super::frame::{AUXILIARY, FLAGS, FRAME, Frame, PARITY, RETURN_TOP};
use super::memory::{AccessRange, emit_address};
use crate::x86::cpu::{
    Cpu, LINEAR_TRANSLATION_CACHE_BITS, LinearTranslationCacheEntry, TranslationRecheck,
};
use core::mem::{offset_of, size_of};

const FLAT_SEGMENT_PROOFS: u32 = {
    use crate::x86::cpu::SegmentReg;
    let segments =
        (1 << SegmentReg::Es as u32) | (1 << SegmentReg::Ss as u32) | (1 << SegmentReg::Ds as u32);
    segments | (segments << 8) | (1 << 6)
};

#[repr(C)]
pub struct View {
    entries: *mut LinearTranslationCacheEntry,
    // Native X16: low data-context bits plus the fixed CS base in the high half.
    state: u64,
    // Four six-bit segment masks, plus the flat SS.B qualification at bit 6.
    // W7 mirrors this; a segment helper refreshes it before native continuation.
    permissions: u32,
    bases: [u32; 2],
    limits: [u32; 2],
    rechecks: *const TranslationRecheck,
    recheck_context: u64,
    walk_root: u64,
    pub cpu: *mut Cpu,
    pub bus: *mut core::ffi::c_void,
    pub refill: usize,
    pub load_segment: usize,
    pub mmx: usize,
    // Native code reads this scalar array through an explicit pointer, never
    // Cpu/FpuState offsets. Only the MMX helper mutates FPU state during a call.
    mmx_registers: *const u64,
    mmx_disabled: u32,
    // One successful native refill, confined to this invocation. This also
    // admits framebuffer pointers, which must never enter the CPU's RAM cache.
    refill_bias: u64,
    refill_tags: u64,
}

/// Cold dispatch path. All arguments are scratch registers, distinct from X5,
/// X9 and X10. The current root and exact words certify an earlier successful
/// execute walk; changes to A/D or permissions require the shared slow path.
pub fn recheck_code(a: &mut Assembler, witness: Reg, linear: Reg, fail: Label) {
    let root = Reg::new(5);
    let word = Reg::new(10);
    // The immutable context and root are adjacent in this invocation's view.
    assert_eq!(
        offset_of!(View, walk_root),
        offset_of!(View, recheck_context) + 8
    );
    a.load_store(true, VIEW, FRAME, offset_of!(Frame, memory) as u32, 3);
    a.pair(
        true,
        VIEW,
        root,
        VIEW,
        offset_of!(View, recheck_context) as i16,
        true,
    );
    a.cbz(root, true, false, fail);
    a.load_store(
        true,
        word,
        witness,
        offset_of!(TranslationRecheck, context) as u32,
        3,
    );
    a.add_sub(ZR, VIEW, word, true, true, true);
    a.b_cond(Cond::Ne, fail);
    a.bitfield(word, linear, 22, 31, false, false);
    a.add_shifted(root, root, word, 2, true);
    a.load_store(true, root, root, 0, 2);
    a.load_store(
        true,
        word,
        witness,
        offset_of!(TranslationRecheck, pde) as u32,
        2,
    );
    a.add_sub(ZR, root, word, false, true, true);
    a.b_cond(Cond::Ne, fail);
    a.load_store(
        true,
        root,
        witness,
        offset_of!(TranslationRecheck, pte_token) as u32,
        3,
    );
    a.load_store(
        true,
        word,
        witness,
        offset_of!(TranslationRecheck, pte) as u32,
        2,
    );
    a.load_store(true, root, root, 0, 2);
    a.add_sub(ZR, root, word, false, true, true);
    a.b_cond(Cond::Ne, fail);
}

impl View {
    /// Only ordinary flat data/stack qualifications specialize code. FS/GS
    /// are often replaced for TLS; including them would split otherwise
    /// identical code and repeatedly discard links during task switches.
    /// The adapter retires links whenever one of these compiled facts changes.
    pub fn block_facts(cpu: &Cpu) -> u32 {
        let (flat_segments, flat_write_segments) = cpu.jit_segment_masks();
        (flat_segments | (flat_write_segments << 8)) & FLAT_SEGMENT_PROOFS
    }

    pub fn new(cpu: &mut Cpu, rechecks: bool) -> Self {
        let (entries, state) = cpu.translation_cache.jit_view();
        let mut view = Self {
            entries,
            state: state | (u64::from(cpu.seg(crate::x86::cpu::SegmentReg::Cs).base) << 32),
            permissions: 0,
            bases: [0; 2],
            limits: [0; 2],
            rechecks: cpu.translation_cache.rechecks.as_ptr(),
            recheck_context: cpu.translation_cache.recheck_context,
            walk_root: if rechecks {
                cpu.translation_cache.walk_root
            } else {
                0
            },
            cpu,
            bus: core::ptr::null_mut(),
            refill: 0,
            load_segment: 0,
            mmx: 0,
            mmx_registers: core::ptr::addr_of_mut!(cpu.fpu.mm).cast(),
            mmx_disabled: cpu.cr0 & (crate::x86::cpu::CR0_EM | crate::x86::cpu::CR0_TS),
            refill_bias: 0,
            refill_tags: 0,
        };
        view.refresh_segments(cpu);
        view
    }

    /// The probe has already checked this page's current mapping and access,
    /// including A/D and host dirty bookkeeping. A write proof also permits
    /// reads. Native execution cannot change its mapping context, code watches
    /// or device decode; recreating the view discards this proof before reentry.
    #[cfg(a64_jit)]
    pub(super) fn remember_refill(&mut self, tag: u32, write: bool, bias: u64) {
        if bias != 0 {
            self.refill_bias = bias;
            self.refill_tags = u64::from(tag) | if write { u64::from(tag) << 32 } else { 0 };
        }
    }

    /// Refresh dynamic segment state. Compiled DS/ES/SS assumptions must be
    /// identical to continue linking; FS/GS always use these current fields.
    pub fn refresh_segments(&mut self, cpu: &Cpu) -> bool {
        let (flat, flat_write) = cpu.jit_segment_masks();
        let (bases, limits, based, based_write) = cpu.jit_based_segments();
        let permissions = flat | (flat_write << 8) | (based << 16) | (based_write << 24);
        let unchanged = (permissions ^ self.permissions) & FLAT_SEGMENT_PROOFS == 0;
        self.permissions = permissions;
        self.bases = bases;
        self.limits = limits;
        unchanged
    }
}

const PERMISSIONS: Reg = Reg::new(7);
const VIEW: Reg = Reg::new(9);
const ADDRESS: Reg = Reg::new(10);
const ENTRY: Reg = Reg::new(11);
const TEMP: Reg = Reg::new(12);
// Mapping context and table addresses stay fixed during the invocation.
// The cold helper may fill individual rows without changing this context.
// These registers survive dispatch to linked blocks and helper calls.
const TABLE: Reg = Reg::new(13);
// Bits 0..11 hold the data context; bits 32..63 hold this invocation's CS base.
// Refills and segment helpers preserve this complete register. Native execution
// cannot replace CS; the next invocation supplies its current base again.
pub(super) const STATE: Reg = Reg::new(16);

// The view is needed only at entry, based-segment checks and cold helpers.
// X6 instead retains the frequently updated native return-cache pointer.
fn view_load(a: &mut Assembler, dst: Reg, offset: u32, size: u8) {
    a.load_store(true, VIEW, FRAME, offset_of!(Frame, memory) as u32, 3);
    a.load_store(true, dst, VIEW, offset, size);
}

pub fn emit_entry(a: &mut Assembler) {
    let register_only = a.label();
    a.load_store(true, VIEW, FRAME, offset_of!(Frame, memory) as u32, 3);
    a.cbz(VIEW, true, false, register_only);
    a.load_store(true, TABLE, VIEW, offset_of!(View, entries) as u32, 3);
    a.load_store(true, STATE, VIEW, offset_of!(View, state) as u32, 3);
    a.load_store(
        true,
        PERMISSIONS,
        VIEW,
        offset_of!(View, permissions) as u32,
        2,
    );
    a.bind(register_only);
}

/// Block-ending helper: preserve packed flags and the fixed mapping registers.
/// AAPCS64 preserves the guest registers, budget and frame. X6 is spilled here.
/// Return 1 continues with refreshed segments, 2 must exit after commit because
/// a compiled flat-segment qualification changed. No local access proof crosses
/// this block boundary; a failed probe returns before the instruction.
pub fn load_segment(a: &mut Assembler, segment: crate::x86::cpu::SegmentReg, miss: Label) {
    a.add_sub_imm(SP, SP, 48, true, true, false);
    a.pair(false, FLAGS, PARITY, SP, 0, true);
    a.pair(false, TABLE, STATE, SP, 16, true);
    a.pair(false, AUXILIARY, RETURN_TOP, SP, 32, true);
    a.mov(Reg::new(2), Reg::new(0), false);
    a.mov(Reg::new(0), FRAME, true);
    a.mov_imm(Reg::new(1), segment as u64, false);
    view_load(a, Reg::new(5), offset_of!(View, load_segment) as u32, 3);
    a.blr(Reg::new(5));
    #[cfg(test)]
    super::frame::poison_helper_return(a, true);
    a.pair(true, FLAGS, PARITY, SP, 0, true);
    a.pair(true, TABLE, STATE, SP, 16, true);
    a.pair(true, AUXILIARY, RETURN_TOP, SP, 32, true);
    a.add_sub_imm(SP, SP, 48, true, false, false);
    view_load(a, PERMISSIONS, offset_of!(View, permissions) as u32, 2);
    a.cbz(Reg::new(0), false, false, miss);
}

/// CR0 cannot change inside an invocation. Preserve interpreter fault priority
/// by leaving before any operand access when EM or TS requires an exception.
pub fn mmx_available(a: &mut Assembler, miss: Label) {
    view_load(a, Reg::new(5), offset_of!(View, mmx_disabled) as u32, 2);
    a.cbz(Reg::new(5), false, true, miss);
}

pub fn read_mmx(a: &mut Assembler, dst: Reg, index: u8) {
    view_load(a, Reg::new(5), offset_of!(View, mmx_registers) as u32, 3);
    a.load_store(true, dst, Reg::new(5), u32::from(index) * 8, 3);
}

/// All faultable operands have completed. The helper touches only FPU state;
/// guest GPRs/time stay resident. No access-range proof crosses this call.
pub fn mmx(a: &mut Assembler, opcode: u8, reg: u8, value: Reg) {
    a.add_sub_imm(SP, SP, 64, true, true, false);
    a.pair(false, FLAGS, PARITY, SP, 0, true);
    a.pair(false, TABLE, STATE, SP, 16, true);
    a.pair(false, AUXILIARY, PERMISSIONS, SP, 32, true);
    a.load_store(false, RETURN_TOP, SP, 48, 3);
    a.mov(Reg::new(2), value, true);
    a.mov_imm(
        Reg::new(1),
        u64::from(opcode) | (u64::from(reg) << 8),
        false,
    );
    view_load(a, Reg::new(5), offset_of!(View, mmx) as u32, 3);
    a.mov(Reg::new(0), VIEW, true);
    a.blr(Reg::new(5));
    #[cfg(test)]
    super::frame::poison_helper_return(a, false);
    a.pair(true, FLAGS, PARITY, SP, 0, true);
    a.pair(true, TABLE, STATE, SP, 16, true);
    a.pair(true, AUXILIARY, PERMISSIONS, SP, 32, true);
    a.load_store(true, RETURN_TOP, SP, 48, 3);
    a.add_sub_imm(SP, SP, 64, true, false, false);
}

/// Compile-time facts valid only along one block's fallthrough path. A failed
/// guard exits before its instruction; a segment load ends the decoded block.
/// REP/count-zero joins reset these facts in the compiler.
#[derive(Clone, Copy)]
pub struct Guard<'a> {
    pub miss: Label,
    pub segments: &'a core::cell::Cell<u32>,
    pub range: Option<AccessRange>,
}

impl Guard<'_> {
    fn require(self, a: &mut Assembler, bit: u8, fail: Label) {
        let mask = 1 << bit;
        let checked = self.segments.get();
        if checked & mask == 0 {
            a.tbz(PERMISSIONS, bit, false, fail);
            self.segments.set(checked | mask);
        }
    }
}

pub fn stack_guard(a: &mut Assembler, guard: Guard<'_>) {
    guard.require(a, 6, guard.miss);
}

/// Consecutive register/immediate PUSHes or POPs may reuse one data translation
/// when their complete byte interval fits a permitted page. This guard performs
/// no access and never walks a future page; a miss restarts at the first opcode.
/// Each store/load still has its own architectural commit and fault location.
pub fn stack_range(a: &mut Assembler, bytes: u8, write: bool, guard: Guard<'_>) {
    stack_guard(a, guard);
    let mut address = Address::stack();
    if write {
        address.displacement = 0u32.wrapping_sub(u32::from(bytes));
    }
    let access = prepare_access(a, address, u16::from(bytes), write, guard, false, None);
    if write {
        // Retain the checked interval's lowest host address. Using that guest
        // address also handles a PUSH run beginning with ESP=0 correctly.
        a.add_shifted(ENTRY, ENTRY, access.linear, 0, true);
    }
}

pub fn stack_load(a: &mut Assembler, value: Reg, bits: u8) -> u32 {
    Access {
        linear: super::frame::guest_reg(4),
        log2_bytes: (bits / 8).trailing_zeros() as u8,
    }
    .access(a, value, false)
}

/// X11 names the minimum address of the dominating PUSH interval. The compiler
/// tracks each member's offset; mixed 16/32-bit pushes need unscaled addressing.
pub fn stack_store(a: &mut Assembler, value: Reg, bits: u8, displacement: u8) -> u32 {
    let offset = a.offset() as u32;
    a.store_unscaled(
        value,
        ENTRY,
        displacement.into(),
        (bits / 8).trailing_zeros() as u8,
    );
    offset
}

/// Returns the offset of the actual LDR, not the preceding guards, for the
/// shared native-PC -> guest-instruction map.
pub fn load(a: &mut Assembler, dst: Reg, address: Address, bits: u8, guard: Guard<'_>) -> u32 {
    prepare(a, address, bits, false, guard).access(a, dst, false)
}

/// Checked RAM address bias in X11. The 32-bit linear address remains in its
/// original guest register or address scratch. Arithmetic emission preserves
/// these until the access completes.
pub struct Access {
    log2_bytes: u8,
    linear: Reg,
}

impl Access {
    pub fn access(&self, a: &mut Assembler, value: Reg, write: bool) -> u32 {
        let offset = a.offset() as u32;
        a.load_store_offset(!write, value, ENTRY, self.linear, self.log2_bytes);
        offset
    }

    pub fn pointer(&self, a: &mut Assembler, dst: Reg) {
        a.add_sub(dst, ENTRY, self.linear, true, false, false);
    }

    /// Number of complete elements remaining in this checked forward page.
    /// Stop before a split element; the shared slow access preserves its fault
    /// ordering. No future page is read or walked merely to find this bound.
    pub fn forward_units(&self, a: &mut Assembler, dst: Reg) {
        a.bitfield(dst, self.linear, 0, 11, false, false);
        a.mov_imm(TEMP, 4096, false);
        a.add_sub(dst, TEMP, dst, false, true, false);
        if self.log2_bytes != 0 {
            a.bitfield(dst, dst, self.log2_bytes, 31, false, false);
        }
    }
}

pub fn prepare(
    a: &mut Assembler,
    address: Address,
    bits: u8,
    write: bool,
    guard: Guard<'_>,
) -> Access {
    let based = matches!(
        address.segment,
        crate::x86::cpu::SegmentReg::Fs | crate::x86::cpu::SegmentReg::Gs
    );
    prepare_access(a, address, u16::from(bits / 8), write, guard, based, None)
}

/// Bit-string register indices adjust the segment offset before checking its
/// limit and adding the base. The compiler supplies a signed byte displacement.
pub fn prepare_offset(
    a: &mut Assembler,
    address: Address,
    bits: u8,
    write: bool,
    guard: Guard<'_>,
    displacement: Reg,
) -> Access {
    let based = matches!(
        address.segment,
        crate::x86::cpu::SegmentReg::Fs | crate::x86::cpu::SegmentReg::Gs
    );
    prepare_access(
        a,
        address,
        u16::from(bits / 8),
        write,
        guard,
        based,
        Some(displacement),
    )
}

/// String chunks need a flat offset: checking their entire linear-page range
/// then also proves no segment-offset wrap. Scalar FS/GS qualification alone
/// only certifies the first element and is insufficient for a whole chunk.
pub fn prepare_flat(
    a: &mut Assembler,
    address: Address,
    bits: u8,
    write: bool,
    guard: Guard<'_>,
) -> Access {
    prepare_access(a, address, u16::from(bits / 8), write, guard, false, None)
}

fn prepare_access(
    a: &mut Assembler,
    address: Address,
    bytes: u16,
    write: bool,
    guard: Guard<'_>,
    based: bool,
    displacement: Option<Reg>,
) -> Access {
    if let Some(range) = guard.range {
        assert!(!based && displacement.is_none());
        if range.first {
            prepare_access(
                a,
                range.address,
                range.bytes,
                range.write,
                Guard {
                    range: None,
                    ..guard
                },
                false,
                None,
            );
            if let Some(slot) = range.spill {
                a.load_store(
                    false,
                    ENTRY,
                    SP,
                    super::frame::PAGE_BIAS_OFFSET + u32::from(slot) * 8,
                    3,
                );
            }
        } else if let Some(slot) = range.spill {
            a.load_store(
                true,
                ENTRY,
                SP,
                super::frame::PAGE_BIAS_OFFSET + u32::from(slot) * 8,
                3,
            );
        }
        return Access {
            log2_bytes: bytes.trailing_zeros() as u8,
            linear: emit_address(a, ADDRESS, address),
        };
    }
    let miss = guard.miss;
    #[cfg(feature = "jit-profile")]
    let reasons: [Label; 4] = core::array::from_fn(|_| a.label());
    #[cfg(not(feature = "jit-profile"))]
    let reasons = [miss; 4];
    let segment = address.segment as u8;
    let permission_bit = segment + if based { 16 } else { 0 } + if write { 8 } else { 0 };
    guard.require(a, permission_bit, reasons[0]);
    let mut offset = emit_address(a, ADDRESS, address);
    if let Some(displacement) = displacement {
        a.add_sub(ADDRESS, offset, displacement, false, false, false);
        offset = ADDRESS;
    }
    let linear = if based {
        // Native guest offsets have zeroed high halves. A 64-bit end offset
        // detects both a limited-segment overrun and 32-bit offset wrap in one
        // comparison, before base addition or any paging effect. The linear
        // base+offset addition below intentionally wraps at 32 bits.
        let end = if bytes > 1 {
            a.add_sub_imm(TEMP, offset, u16::from(bytes - 1), true, false, false);
            TEMP
        } else {
            offset
        };
        view_load(
            a,
            VIEW,
            (offset_of!(View, limits) + usize::from(segment - 4) * 4) as u32,
            2,
        );
        a.add_sub(ZR, end, VIEW, true, true, true);
        a.b_cond(Cond::Hi, reasons[0]);
        view_load(
            a,
            TEMP,
            (offset_of!(View, bases) + usize::from(segment - 4) * 4) as u32,
            2,
        );
        a.add_sub(ADDRESS, offset, TEMP, false, false, false);
        ADDRESS
    } else {
        offset
    };
    // A 16-byte row holds one bias and two exact 32-bit permission tags. The
    // read tag is the low half of X9; the write tag is compared with LSR #32.
    assert_eq!(size_of::<LinearTranslationCacheEntry>(), 16);
    a.bitfield(
        ENTRY,
        linear,
        12,
        11 + LINEAR_TRANSLATION_CACHE_BITS,
        false,
        false,
    );
    a.add_shifted(ENTRY, TABLE, ENTRY, 4, true);
    // Start the dependent row load before independent end-byte/tag arithmetic.
    assert_eq!(offset_of!(LinearTranslationCacheEntry, bias), 0);
    assert_eq!(offset_of!(LinearTranslationCacheEntry, tag), 8);
    assert_eq!(offset_of!(LinearTranslationCacheEntry, write_tag), 12);
    a.pair(true, ENTRY, VIEW, ENTRY, 0, true);
    // Index by the first byte, but compare the page of the final byte. A split
    // spans adjacent pages, which have different direct-cache indices, so the
    // first byte's slot cannot contain a valid matching final-page tag. This
    // folds the split guard into the normal tag comparison, including 32-bit
    // address wrap, without disallowing unaligned accesses within one page.
    if bytes > 1 {
        a.add_sub_imm(TEMP, linear, u16::from(bytes - 1), false, false, false);
    } else {
        a.mov(TEMP, linear, false);
    }
    // Page bits stay untouched; the low twelve bits hold generation and CPL.
    a.bfi32(TEMP, STATE, 0, 12);
    if write {
        a.compare_lsr64(TEMP, VIEW, 32);
    } else {
        a.add_sub(ZR, TEMP, VIEW, false, true, true);
    }
    let recheck = a.label();
    let matched = a.label();
    a.b_cond(Cond::Ne, recheck);
    a.bind(matched);
    a.tail(super::cold::Path::Recheck {
        entry: recheck,
        matched,
        miss: reasons[2],
        linear,
        write,
    });
    #[cfg(feature = "jit-profile")]
    {
        // Diagnose guard failures only in a separate profiling executable.
        // Production blocks contain neither these stores nor this extra branch.
        let access = a.label();
        a.b(access);
        for (reason, label) in reasons.into_iter().enumerate() {
            a.bind(label);
            if reason == 2 {
                a.load_store(
                    false,
                    TEMP,
                    FRAME,
                    offset_of!(Frame, memory_expected_tag) as u32,
                    3,
                );
                a.load_store(
                    false,
                    VIEW,
                    FRAME,
                    offset_of!(Frame, memory_cached_tag) as u32,
                    3,
                );
            }
            let reason = if reason == 0 {
                u64::from(segment) + 1
            } else {
                (reason + 6) as u64
            };
            a.mov_imm(TEMP, reason, false);
            a.load_store(
                false,
                TEMP,
                FRAME,
                offset_of!(Frame, memory_exit_reason) as u32,
                2,
            );
            a.b(miss);
        }
        a.bind(access);
    }
    Access {
        log2_bytes: bytes.trailing_zeros() as u8,
        linear,
    }
}

pub(super) fn emit_recheck_tail(
    a: &mut Assembler,
    recheck: Label,
    matched: Label,
    miss: Label,
    linear: Reg,
    write: bool,
) {
    a.bind(recheck);
    #[cfg(feature = "jit-profile")]
    {
        if write {
            a.bitfield(VIEW, VIEW, 32, 63, true, false);
        } else {
            a.mov(VIEW, VIEW, false);
        }
        a.load_store(
            false,
            VIEW,
            FRAME,
            offset_of!(Frame, memory_cached_tag) as u32,
            3,
        );
    }
    // X12 names the final byte's page. The shared refill must never turn a
    // split access into a one-page hit, including a 32-bit address wrap.
    a.logical(2, VIEW, linear, TEMP, false);
    a.and_high_bits(VIEW, VIEW, 12, false);
    a.cbz(VIEW, false, true, miss);
    let resume = a.label();
    a.adr(LR, resume);
    if write {
        // Native continuations are four-byte aligned. Carry the access kind
        // in bit zero and clear it in the shared stub before returning.
        a.add_sub_imm(LR, LR, 1, true, false, false);
    }
    a.load_store(
        true,
        VIEW,
        FRAME,
        offset_of!(Frame, recheck_entry) as u32,
        3,
    );
    a.br(VIEW);
    a.bind(resume);
    #[cfg(feature = "jit-profile")]
    a.load_store(
        true,
        VIEW,
        FRAME,
        offset_of!(Frame, memory_cached_tag) as u32,
        3,
    );
    a.b_cond(Cond::Eq, miss);
    a.b(matched);
}

/// Cold AAPCS64 boundary for a probe with no architectural effects. All caller-saved
/// integer registers except the returned X11 bias survive; guest registers,
/// budget, frame and view are callee-saved. X18 remains platform-reserved.
/// LR supplies the continuation, X12 the desired tag. Return Z=1 on failure.
/// The failed recheck already saved X0..X5 in 48 bytes. Extend that frame;
/// those original values need not be restored and immediately saved again.
fn emit_refill(a: &mut Assembler, restored: Label) {
    let r = Reg::new;
    // Only a failed ordinary row recheck reaches this extra entry. Keep the
    // common native access and successful RAM witness paths unchanged.
    a.load_store(true, r(0), FRAME, offset_of!(Frame, memory) as u32, 3);
    assert_eq!(
        offset_of!(View, refill_tags),
        offset_of!(View, refill_bias) + 8
    );
    a.pair(
        true,
        r(1),
        r(2),
        r(0),
        offset_of!(View, refill_bias) as i16,
        true,
    );
    let read = a.label();
    let probe = a.label();
    a.tbz(LR, 0, false, read);
    a.bitfield(r(2), r(2), 32, 63, true, false);
    a.bind(read);
    a.add_sub(ZR, TEMP, r(2), false, true, true);
    a.b_cond(Cond::Ne, probe);
    a.mov(ENTRY, r(1), true);
    #[cfg(any(test, feature = "jit-profile"))]
    {
        a.load_store(
            true,
            r(0),
            FRAME,
            offset_of!(Frame, cached_data_refills) as u32,
            3,
        );
        a.add_sub_imm(r(0), r(0), 1, true, false, false);
        a.load_store(
            false,
            r(0),
            FRAME,
            offset_of!(Frame, cached_data_refills) as u32,
            3,
        );
    }
    a.b(restored);
    a.bind(probe);
    a.add_sub_imm(SP, SP, 112, true, true, false);
    for pair in 3..9 {
        a.pair(
            false,
            r(pair * 2),
            r(pair * 2 + 1),
            SP,
            i16::from(pair - 3) * 16,
            true,
        );
    }
    a.load_store(false, r(30), SP, 96, 3);
    a.load_store(true, VIEW, FRAME, offset_of!(Frame, memory) as u32, 3);
    a.mov(r(0), VIEW, true);
    a.mov(r(1), TEMP, false);
    a.bitfield(r(2), LR, 0, 0, false, false);
    a.load_store(true, r(16), VIEW, offset_of!(View, refill) as u32, 3);
    a.blr(r(16));
    #[cfg(test)]
    super::frame::poison_helper_return(a, true);
    a.mov(ENTRY, r(0), true);
    #[cfg(any(test, feature = "jit-profile"))]
    {
        let failed = a.label();
        a.cbz(ENTRY, true, false, failed);
        a.load_store(
            true,
            r(0),
            FRAME,
            offset_of!(Frame, refilled_data) as u32,
            3,
        );
        a.add_sub_imm(r(0), r(0), 1, true, false, false);
        a.load_store(
            false,
            r(0),
            FRAME,
            offset_of!(Frame, refilled_data) as u32,
            3,
        );
        a.bind(failed);
    }
    for pair in 0..9 {
        let offset = if pair < 3 {
            112 + i16::from(pair) * 16
        } else {
            i16::from(pair - 3) * 16
        };
        if pair == 5 {
            a.load_store(true, r(10), SP, offset as u32, 3);
        } else {
            a.pair(true, r(pair * 2), r(pair * 2 + 1), SP, offset, true);
        }
    }
    a.load_store(true, r(30), SP, 96, 3);
    a.add_sub_imm(SP, SP, 160, true, false, false);
    a.add_sub(ZR, ENTRY, ZR, true, true, true);
    a.and_high_bits(LR, LR, 1, true);
    // Entered with ADR/BR, so this must not pop the guest return predictor.
    a.br(LR);
}

/// Cold revalidation uses only RAM comparison witnesses produced by the shared
/// walker. X12 names the requested tag, X11 retains its backing bias and LR
/// the continuation. Save every arithmetic temporary we borrow. Return Z=0
/// after current PDE/PTE words (including A/D) match exactly. On failure try
/// the shared data probe; a semantic miss still exits before the instruction.
pub fn emit_recheck(a: &mut Assembler) {
    let r = Reg::new;
    let fail = a.label();
    let restored = a.label();
    a.add_sub_imm(SP, SP, 48, true, true, false);
    for pair in 0..3 {
        a.pair(
            false,
            r(pair * 2),
            r(pair * 2 + 1),
            SP,
            i16::from(pair) * 16,
            true,
        );
    }
    a.bitfield(
        r(1),
        TEMP,
        12,
        11 + LINEAR_TRANSLATION_CACHE_BITS,
        false,
        false,
    );
    // A witness belongs to the old row's exact page and privilege. Reusing a
    // colliding row or a missing write tag would bypass the actual walk.
    a.cbz(ENTRY, true, false, fail);
    a.add_shifted(r(0), TABLE, r(1), 4, true);
    a.load_store(
        true,
        VIEW,
        r(0),
        offset_of!(LinearTranslationCacheEntry, tag) as u32,
        2,
    );
    let selected = a.label();
    a.tbz(LR, 0, false, selected);
    a.load_store(
        true,
        VIEW,
        r(0),
        offset_of!(LinearTranslationCacheEntry, write_tag) as u32,
        2,
    );
    a.bind(selected);
    a.cbz(VIEW, false, false, fail);
    a.logical(2, VIEW, TEMP, VIEW, false);
    a.bfi32(VIEW, ZR, 1, 11); // Ignore only generation, never page or CPL.
    a.cbz(VIEW, false, true, fail);
    // Load the witness array before the paired load replaces X9's view pointer
    // with its context. All three fields stay fixed during native execution.
    assert_eq!(
        offset_of!(View, walk_root),
        offset_of!(View, recheck_context) + 8
    );
    a.load_store(true, VIEW, FRAME, offset_of!(Frame, memory) as u32, 3);
    a.load_store(true, r(0), VIEW, offset_of!(View, rechecks) as u32, 3);
    a.pair(
        true,
        VIEW,
        r(2),
        VIEW,
        offset_of!(View, recheck_context) as i16,
        true,
    );
    a.cbz(r(2), true, false, fail);
    assert_eq!(size_of::<TranslationRecheck>(), 32);
    a.add_shifted(r(0), r(0), r(1), 5, true);
    a.pair(true, r(4), r(3), r(0), 0, true);
    a.add_sub(ZR, VIEW, r(4), true, true, true);
    a.b_cond(Cond::Ne, fail);
    a.bitfield(VIEW, TEMP, 22, 31, false, false);
    a.add_shifted(r(2), r(2), VIEW, 2, true);
    a.pair(
        true,
        r(4),
        r(5),
        r(0),
        offset_of!(TranslationRecheck, pde) as i16,
        false,
    );
    a.load_store(true, VIEW, r(2), 0, 2);
    a.add_sub(ZR, VIEW, r(4), false, true, true);
    a.b_cond(Cond::Ne, fail);
    a.load_store(true, VIEW, r(3), 0, 2);
    a.add_sub(ZR, VIEW, r(5), false, true, true);
    a.b_cond(Cond::Ne, fail);
    a.add_shifted(r(0), TABLE, r(1), 4, true);
    a.load_store(
        false,
        TEMP,
        r(0),
        offset_of!(LinearTranslationCacheEntry, tag) as u32,
        2,
    );
    // The exact walk witness also preserves any previously certified write
    // permission. Keep the two tags synchronized after a successful read recheck.
    a.load_store(
        true,
        r(2),
        r(0),
        offset_of!(LinearTranslationCacheEntry, write_tag) as u32,
        2,
    );
    let read_only = a.label();
    a.cbz(r(2), false, false, read_only);
    a.load_store(
        false,
        TEMP,
        r(0),
        offset_of!(LinearTranslationCacheEntry, write_tag) as u32,
        2,
    );
    a.bind(read_only);
    #[cfg(any(test, feature = "jit-profile"))]
    {
        a.load_store(
            true,
            r(0),
            FRAME,
            offset_of!(Frame, rechecked_data) as u32,
            3,
        );
        a.add_sub_imm(r(0), r(0), 1, true, false, false);
        a.load_store(
            false,
            r(0),
            FRAME,
            offset_of!(Frame, rechecked_data) as u32,
            3,
        );
    }
    a.bind(restored);
    a.add_sub(ZR, ZR, TEMP, true, true, true); // A valid generation tag is nonzero.
    for pair in 0..3 {
        a.pair(
            true,
            r(pair * 2),
            r(pair * 2 + 1),
            SP,
            i16::from(pair) * 16,
            true,
        );
    }
    a.add_sub_imm(SP, SP, 48, true, false, false);
    a.and_high_bits(LR, LR, 1, true);
    // Entered with ADR/BR, so this must not pop the guest return predictor.
    a.br(LR);
    a.bind(fail);
    emit_refill(a, restored);
}
