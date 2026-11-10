use crate::x86::addr::{LinearAddr, PhysAddr};
use crate::x86::mem::{BackingToken, PhysWrite};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reg32 {
    Eax = 0,
    Ecx = 1,
    Edx = 2,
    Ebx = 3,
    Esp = 4,
    Ebp = 5,
    Esi = 6,
    Edi = 7,
}

impl Reg32 {
    #[inline(always)]
    pub const fn from_u3(value: u8) -> Self {
        match value & 7 {
            0 => Self::Eax,
            1 => Self::Ecx,
            2 => Self::Edx,
            3 => Self::Ebx,
            4 => Self::Esp,
            5 => Self::Ebp,
            6 => Self::Esi,
            _ => Self::Edi,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentReg {
    Es = 0,
    Cs = 1,
    Ss = 2,
    Ds = 3,
    Fs = 4,
    Gs = 5,
}

impl SegmentReg {
    #[inline(always)]
    pub const fn from_modrm_reg(value: u8) -> Self {
        match value & 7 {
            0 => Self::Es,
            1 => Self::Cs,
            2 => Self::Ss,
            3 => Self::Ds,
            4 => Self::Fs,
            _ => Self::Gs,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SegmentCache {
    pub selector: u16,
    pub base: u32,
    pub limit: u32,
    pub flags: u16,
}

impl SegmentCache {
    #[inline]
    pub const fn new(selector: u16, base: u32, limit: u32, flags: u16) -> Self {
        Self {
            selector,
            base,
            limit,
            flags,
        }
    }

    #[inline]
    pub const fn flat(selector: u16, flags: u16) -> Self {
        Self::new(selector, 0, 0xffff_ffff, flags)
    }

    #[inline(always)]
    pub const fn architectural_flags(self) -> u16 {
        self.flags
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TableReg {
    pub base: u32,
    pub limit: u16,
}

impl TableReg {
    #[inline]
    pub const fn empty() -> Self {
        Self { base: 0, limit: 0 }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FpuState {
    pub control: u16,
    pub status: u16,
    pub top: u8,
    pub mxcsr: u32,
    pub st: [f64; 8],
    pub mm: [u64; 8],
    pub mmx_active: bool,
}

impl FpuState {
    pub const fn new() -> Self {
        Self {
            control: 0x037f,
            status: 0,
            top: 0,
            mxcsr: 0x0000_1f80,
            st: [0.0; 8],
            mm: [0; 8],
            mmx_active: false,
        }
    }

    pub fn init(&mut self) {
        *self = Self::new();
    }

    pub fn clear_exceptions(&mut self) {
        self.status &= !0x80ff;
    }

    pub fn status_word(&self) -> u16 {
        (self.status & !0x3800) | (((self.top as u16) & 7) << 11)
    }

    pub fn push(&mut self, value: f64) {
        self.top = self.top.wrapping_sub(1) & 7;
        self.set(0, value);
    }

    pub fn pop(&mut self) {
        self.top = self.top.wrapping_add(1) & 7;
    }

    pub fn get(&self, index: u8) -> f64 {
        self.st[((self.top.wrapping_add(index)) & 7) as usize]
    }

    pub fn set(&mut self, index: u8, value: f64) {
        self.st[((self.top.wrapping_add(index)) & 7) as usize] = value;
        self.mmx_active = false;
    }

    pub fn mmx(&self, index: u8) -> u64 {
        self.mm[(index & 7) as usize]
    }

    pub fn set_mmx(&mut self, index: u8, value: u64) {
        self.top = 0;
        self.mm[(index & 7) as usize] = value;
        self.mmx_active = true;
    }

    pub fn prepare_mmx(&mut self) {
        self.top = 0;
        self.mmx_active = true;
    }

    pub fn emms(&mut self) {
        self.top = 0;
        self.mmx_active = false;
    }
}

/// Compact arithmetic record shared with generated Wasm. Operands/results are
/// already masked to the guest width. Bits 0..3 select the operation, bit 4
/// preserves EFLAGS.CF (INC/DEC), bit 6 holds ADC/SBB or shift carry, and bits
/// 8..12 give the sign-bit index. Zero means the status bits are materialized.
/// The right operand is redundant: reconstruct it only for AF/OF or a native
/// flag primitive. Ordinary carry/zero/sign tests need no reconstruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LazyFlags {
    lhs: u32,
    result: u32,
    kind: u32,
}

impl LazyFlags {
    const LOGIC: u32 = 1;
    const ADD: u32 = 2;
    const ADC: u32 = 3;
    const SUB: u32 = 4;
    const SBB: u32 = 5;
    const SHIFT: u32 = 6;
    const KEEP_CF: u32 = 16;
    const NONE: Self = Self {
        lhs: 0,
        result: 0,
        kind: 0,
    };

    #[inline(always)]
    const fn new(op: u32, bits: u32, lhs: u32, result: u32, carry: u32) -> Self {
        Self {
            lhs,
            result,
            kind: op | ((bits - 1) << 8) | (carry << 6),
        }
    }

    #[inline(always)]
    const fn pending(self) -> bool {
        self.kind != 0
    }

    #[inline(always)]
    fn rhs(self) -> u32 {
        let carry = (self.kind >> 6) & 1;
        if self.kind & 15 < Self::SUB {
            self.result.wrapping_sub(self.lhs).wrapping_sub(carry)
        } else {
            self.lhs.wrapping_sub(self.result).wrapping_sub(carry)
        }
    }

    #[inline(always)]
    #[cfg(not(all(feature = "r86chip-native", target_arch = "riscv32")))]
    fn flag(self, previous: u32, mask: u32) -> bool {
        let op = self.kind & 15;
        let carry = self.kind & (1 << 6) != 0;
        match mask {
            EFLAGS_CF => {
                if self.kind & Self::KEEP_CF != 0 {
                    return previous & EFLAGS_CF != 0;
                }
                match op {
                    Self::ADD | Self::ADC => {
                        self.result < self.lhs || (carry && self.result == self.lhs)
                    }
                    Self::SUB | Self::SBB => {
                        self.lhs < self.result || (carry && self.result == self.lhs)
                    }
                    Self::SHIFT => carry,
                    _ => false,
                }
            }
            EFLAGS_PF => parity8(self.result as u8),
            EFLAGS_AF => {
                matches!(op, Self::ADD | Self::ADC | Self::SUB | Self::SBB)
                    && (self.lhs ^ self.rhs() ^ self.result) & 0x10 != 0
            }
            EFLAGS_ZF => self.result == 0,
            EFLAGS_SF => self.result & (1 << (self.kind >> 8)) != 0,
            EFLAGS_OF => {
                let changed_sign = (self.lhs ^ self.result) & (1 << (self.kind >> 8));
                match op {
                    Self::ADD | Self::ADC => !(self.lhs ^ self.rhs()) & changed_sign != 0,
                    Self::SUB | Self::SBB => (self.lhs ^ self.rhs()) & changed_sign != 0,
                    Self::SHIFT => self.lhs != 0,
                    _ => false,
                }
            }
            _ => false,
        }
    }

    #[inline(always)]
    #[cfg(not(all(feature = "r86chip-native", target_arch = "riscv32")))]
    fn status_bits(self, previous: u32) -> u32 {
        flag_bit(EFLAGS_CF, self.flag(previous, EFLAGS_CF))
            | flag_bit(EFLAGS_PF, self.flag(previous, EFLAGS_PF))
            | flag_bit(EFLAGS_AF, self.flag(previous, EFLAGS_AF))
            | flag_bit(EFLAGS_ZF, self.flag(previous, EFLAGS_ZF))
            | flag_bit(EFLAGS_SF, self.flag(previous, EFLAGS_SF))
            | flag_bit(EFLAGS_OF, self.flag(previous, EFLAGS_OF))
    }

    #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
    #[inline(always)]
    fn chip_eflags_width<const WIDTH: u32>(self, previous: u32) -> u32 {
        use crate::chipinterp::native::flags;
        let carry = (self.kind >> 6) & 1;
        match self.kind & 31 {
            Self::LOGIC => flags::<WIDTH, 1>(self.result, 0, 0, previous),
            Self::ADD => flags::<WIDTH, 0>(self.lhs, self.rhs(), 0, previous),
            Self::ADC => flags::<WIDTH, 2>(self.lhs, self.rhs(), carry, previous),
            Self::SUB => flags::<WIDTH, 5>(self.lhs, self.rhs(), 0, previous),
            Self::SBB => flags::<WIDTH, 3>(self.lhs, self.rhs(), carry, previous),
            18 => flags::<WIDTH, 32>(self.lhs, 1, previous & 1, previous),
            20 => flags::<WIDTH, 37>(self.lhs, 1, previous & 1, previous),
            Self::SHIFT => {
                flags::<WIDTH, 1>(self.result, 0, 0, previous) | carry | (self.lhs << 11)
            }
            _ => unreachable!("invalid native arithmetic operation"),
        }
    }

    #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
    #[inline(always)]
    fn chip_eflags(self, previous: u32) -> u32 {
        match self.kind >> 8 {
            7 => self.chip_eflags_width::<2>(previous),
            15 => self.chip_eflags_width::<1>(previous),
            31 => self.chip_eflags_width::<0>(previous),
            0 if self.kind == 0 => previous,
            _ => unreachable!("invalid native arithmetic width"),
        }
    }
}

// Keep frequently accessed state together, ahead of the large inline caches.
// This avoids materializing large offsets in Wasm/ARM64 and fits RV32's signed
// 12-bit displacements. The layout is private, not a serialized or host ABI.
#[repr(C)]
#[derive(Clone, Debug)]
pub struct Cpu {
    pub regs: [u32; 8],
    pub eip: u32,
    pub eflags: u32,
    lazy_flags: LazyFlags,
    // Observe time through tsc()/set_tsc(), including chip batch progress.
    tsc_base: u64,
    #[cfg(feature = "r86chip")]
    tsc_remaining: u32,
    pub segment_override: Option<SegmentReg>,
    segments: [SegmentCache; 6],
    flat_segment_read_mask: u8,
    flat_segment_write_mask: u8,
    #[cfg(feature = "jit")]
    jit_based_read_mask: u8,
    #[cfg(feature = "jit")]
    jit_based_write_mask: u8,
    code_default16: bool,
    cached_cpl: u8,
    pub halted: bool,
    interrupt_shadow: bool,
    interrupt_attention: bool,
    pub fetch_cursor: FetchCursor,
    pub cr0: u32,
    pub cr2: u32,
    pub cr3: u32,
    pub cr4: u32,
    pub pae_pdptrs: [u64; 4],
    pub dr: [u32; 8],
    pub gdtr: TableReg,
    pub idtr: TableReg,
    pub ldtr: u16,
    pub ldtr_cache: SegmentCache,
    pub tr: u16,
    pub tr_cache: SegmentCache,
    pub msr_apic_base: u64,
    pub msr_test_ctl: u32,
    pub msr_bios_update_trigger: u64,
    pub msr_bbl_cr_data: [u64; 4],
    pub msr_bbl_cr_addr: u64,
    pub msr_bbl_cr_decc: u64,
    pub msr_bbl_cr_ctl: u64,
    pub msr_bbl_cr_trig: u64,
    pub msr_bbl_cr_busy: u64,
    pub msr_bbl_cr_ctl3: u64,
    pub msr_misc_enable: u64,
    pub msr_perfctr: [u64; 2],
    pub msr_evntsel: [u64; 2],
    pub msr_mtrr_phys: [u64; 16],
    pub msr_mtrr_fix64k_00000: u64,
    pub msr_mtrr_fix16k: [u64; 2],
    pub msr_mtrr_fix4k: [u64; 8],
    pub msr_mtrr_def_type: u64,
    pub msr_pat: u64,
    pub msr_mcg_ctl: u64,
    pub msr_mca_ctl: [u64; 5],
    pub msr_sysenter_cs: u64,
    pub msr_sysenter_esp: u64,
    pub msr_sysenter_eip: u64,
    pub fpu: FpuState,
    pub fetch_cache: FetchCache,
    pub translation_cache: LinearTranslationCache,
    // Any cached 2/4 MiB translation makes INVLPG cover its containing 4 MiB
    // region. The bitmap also covers execute links which outlive the fetch
    // cache. Extra eviction of the adjacent PAE 2 MiB page is permitted.
    large_translation_regions: [u64; 16],
    // Native links can outlive execute-cache entries. Advance even when that
    // cache is empty: its occupancy is not proof that old native links expired.
    #[cfg(feature = "jit")]
    jit_mapping_epoch: u64,
    #[cfg(feature = "jit")]
    jit_global_epoch: u64,
    #[cfg(feature = "jit")]
    jit_invalidated_range: Option<(u32, u32)>,
    pub cache_stats: CpuCacheStats,
    #[cfg(feature = "r86chip")]
    pub chip_counts: crate::chipinterp::Counts,
}

pub const EFLAGS_CF: u32 = 1 << 0;
pub const EFLAGS_PF: u32 = 1 << 2;
pub const EFLAGS_AF: u32 = 1 << 4;
pub const EFLAGS_ZF: u32 = 1 << 6;
pub const EFLAGS_SF: u32 = 1 << 7;
pub const EFLAGS_IF: u32 = 1 << 9;
pub const EFLAGS_DF: u32 = 1 << 10;
pub const EFLAGS_OF: u32 = 1 << 11;
const EFLAGS_STATUS_MASK: u32 =
    EFLAGS_CF | EFLAGS_PF | EFLAGS_AF | EFLAGS_ZF | EFLAGS_SF | EFLAGS_OF;

pub const SEGMENT_FLAG_DEFAULT_BIG: u16 = 1 << 14;

pub const CR0_PE: u32 = 1 << 0;
pub const CR0_MP: u32 = 1 << 1;
pub const CR0_EM: u32 = 1 << 2;
pub const CR0_TS: u32 = 1 << 3;
pub const CR0_WP: u32 = 1 << 16;
pub const CR0_PG: u32 = 1 << 31;
pub const CR4_VME: u32 = 1 << 0;
pub const CR4_PVI: u32 = 1 << 1;
pub const CR4_TSD: u32 = 1 << 2;
pub const CR4_DE: u32 = 1 << 3;
pub const CR4_PSE: u32 = 1 << 4;
pub const CR4_PAE: u32 = 1 << 5;
pub const CR4_MCE: u32 = 1 << 6;
pub const CR4_PGE: u32 = 1 << 7;
pub const CR4_PCE: u32 = 1 << 8;
pub const CR4_OSFXSR: u32 = 1 << 9;

const FETCH_PAGE_MASK: u32 = 0xffff_f000;
const FETCH_PAGE_OFFSET_MASK: u32 = 0x0000_0fff;
const CACHE_CONTEXT_EFLAGS_VM: u32 = 1 << 17;
const FETCH_CACHE_SLOTS: usize = 64;
const FETCH_PHYS_FILTER_WORDS: usize = 64;
const FETCH_PHYS_FILTER_BITS: u64 = (FETCH_PHYS_FILTER_WORDS as u64) * 64;
const FETCH_CACHE_GENERATION_STEP: u64 = 1 << 32;
const FETCH_CACHE_GENERATION_MASK: u64 = 0xffff_ffff_0000_0000;
pub(crate) const LINEAR_TRANSLATION_CACHE_BITS: u8 = if cfg!(feature = "jit") { 11 } else { 9 };
const LINEAR_TRANSLATION_CACHE_SLOTS: usize = 1 << LINEAR_TRANSLATION_CACHE_BITS;
const LINEAR_TRANSLATION_PAGE_MASK: u32 = 0xffff_f000;
#[cfg(feature = "jit")]
type TranslationTag = u32;
#[cfg(not(feature = "jit"))]
type TranslationTag = u64;
pub(crate) const LINEAR_TRANSLATION_USER: TranslationTag =
    if cfg!(feature = "jit") { 1 } else { 4 };
const LINEAR_TRANSLATION_GENERATION_STEP: TranslationTag =
    if cfg!(feature = "jit") { 2 } else { 1u64 << 32 } as TranslationTag;
const LINEAR_TRANSLATION_GENERATION_MASK: TranslationTag = if cfg!(feature = "jit") {
    0xffe
} else {
    0xffff_ffff_0000_0000u64
} as TranslationTag;
const LINEAR_TRANSLATION_WRITE_WORDS: usize = LINEAR_TRANSLATION_CACHE_SLOTS / 64;
const FLAT_32BIT_STACK_MASK: u8 = 1 << 7;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FetchCacheEntry {
    backing_page: u64,
    phys_page: u64,
    // Context-changing operations advance the cache generation. The low word
    // remains the x86 linear page, keeping lookup to one tag comparison.
    tag: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FetchCache {
    // Translation-only cache for instruction bytes. Bytes are still read from
    // the bus so self-modifying code observes committed memory contents.
    entries: [FetchCacheEntry; FETCH_CACHE_SLOTS],
    phys_filter: [u64; FETCH_PHYS_FILTER_WORDS],
    generation: u64,
    valid_count: u8,
    global_entries: u64,
    #[cfg(feature = "jit")]
    rechecks: [TranslationRecheck; FETCH_CACHE_SLOTS],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FetchCursor {
    window_len: u16,
    u16_start_count: u16,
    u32_start_count: u16,
    start_eip: u32,
    start_backing: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FetchCacheHit {
    pub phys: PhysAddr,
    pub backing: BackingToken,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CpuCacheStats {
    pub fetch_translation_hits: u64,
    pub fetch_translation_fills: u64,
    pub fetch_translation_invalidations: u64,
    pub linear_translation_hits: u64,
    pub linear_translation_fills: u64,
    pub linear_translation_invalidations: u64,
}

impl CpuCacheStats {
    pub const fn new() -> Self {
        Self {
            fetch_translation_hits: 0,
            fetch_translation_fills: 0,
            fetch_translation_invalidations: 0,
            linear_translation_hits: 0,
            linear_translation_fills: 0,
            linear_translation_invalidations: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct LinearTranslationCacheEntry {
    // Context changes advance the cache-wide generation, keeping the hot tag
    // independent of CR0/CR3/CR4 while preserving explicit invalidation.
    // Adding the complete 32-bit linear address yields the backing token.
    // Cache identity still uses the linear page; no host/guest alignment is
    // assumed, and wrapping arithmetic also supports physical-address tokens.
    pub(crate) bias: u64,
    pub(crate) tag: TranslationTag,
    // JIT rows keep exact page/CPL plus eleven generation bits in each tag.
    // Generation wrap clears all rows before reusing a tag. A single LDP loads
    // the bias and both tags; stores compare the high tag, reads the low tag.
    // Zero never grants write permission. Ordinary builds retain their u64 tag.
    #[cfg(feature = "jit")]
    pub(crate) write_tag: u32,
}

/// A successful ordinary non-PAE walk, retained as a comparison witness only.
/// Native revalidation must read the current root's PDE and this PTE and find
/// both complete words unchanged. It performs no paging semantics or A/D writes.
/// The control epoch excludes reuse after permission/format/physical-map changes;
/// the ordinary cache tag still qualifies the exact linear page and privilege.
#[cfg(feature = "jit")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(32))]
pub struct TranslationRecheck {
    pub context: u64,
    pub pte_token: u64,
    pub pde: u32,
    pub pte: u32,
}

#[cfg(feature = "jit")]
impl TranslationRecheck {
    pub const EMPTY: Self = Self {
        context: 0,
        pte_token: 0,
        pde: 0,
        pte: 0,
    };
}

impl LinearTranslationCacheEntry {
    pub const EMPTY: Self = Self {
        bias: 0,
        tag: 0,
        #[cfg(feature = "jit")]
        write_tag: 0,
    };

    #[inline]
    pub const fn is_valid(self) -> bool {
        self.tag != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinearTranslationCache {
    // Generation plus the current CPL's user bit. Keeping both in one word
    // lets the dominant read hit compare a prebuilt tag.
    state: TranslationTag,
    entries: [LinearTranslationCacheEntry; LINEAR_TRANSLATION_CACHE_SLOTS],
    // Read hits need only the entry tag. Writes pay the separate permission
    // test so the much more common read path does not mask every tag.
    write_permissions: [u64; LINEAR_TRANSLATION_WRITE_WORDS],
    global_entries: [u64; LINEAR_TRANSLATION_WRITE_WORDS],
    // Device grants are evicted at mapping/presentation boundaries, never
    // checked on an ordinary RAM hit. Fill removes stale ownership on reuse.
    ioram_entries: [u64; LINEAR_TRANSLATION_WRITE_WORDS],
    ioram_active: bool,
    #[cfg(feature = "jit")]
    pub(crate) rechecks: [TranslationRecheck; LINEAR_TRANSLATION_CACHE_SLOTS],
    #[cfg(feature = "jit")]
    pub(crate) recheck_context: u64,
    #[cfg(feature = "jit")]
    pub(crate) walk_root: u64,
}

impl FetchCacheEntry {
    const EMPTY: Self = Self {
        backing_page: 0,
        phys_page: 0,
        tag: 0,
    };

    #[inline(always)]
    fn valid_for(self, generation: u64) -> bool {
        (self.tag & FETCH_CACHE_GENERATION_MASK) == generation
    }
}

impl LinearTranslationCache {
    /// The JIT adapter exclusively borrows these entries for one invocation.
    /// Native rechecks and the cold Rust refill may update derived cache rows;
    /// control/segment state stays fixed and live GPRs/flags are not observed.
    /// Tags and pointers use a C ABI, not Cpu or cache object layout.
    #[cfg(feature = "jit")]
    pub(crate) fn jit_view(&mut self) -> (*mut LinearTranslationCacheEntry, u64) {
        (self.entries.as_mut_ptr(), u64::from(self.state))
    }

    pub(crate) fn revoke_writable_aliases(&mut self, backing_page: u64) {
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        crate::chipinterp::native::flush();
        // This token names byte zero of the guest page. Pointer-based tokens
        // need not be host-page aligned.
        // Revoke all guest aliases of this RAM page without evicting unrelated
        // data writes when execution discovers a code page or page table.
        for (slot, entry) in self.entries.iter_mut().enumerate() {
            if entry
                .bias
                .wrapping_add(u64::from(entry.tag) & u64::from(LINEAR_TRANSLATION_PAGE_MASK))
                == backing_page
            {
                self.write_permissions[slot >> 6] &= !(1u64 << (slot & 63));
                #[cfg(feature = "jit")]
                {
                    entry.write_tag = 0;
                }
            }
        }
    }

    pub const fn new() -> Self {
        Self {
            state: LINEAR_TRANSLATION_GENERATION_STEP,
            entries: [LinearTranslationCacheEntry::EMPTY; LINEAR_TRANSLATION_CACHE_SLOTS],
            write_permissions: [0; LINEAR_TRANSLATION_WRITE_WORDS],
            global_entries: [0; LINEAR_TRANSLATION_WRITE_WORDS],
            ioram_entries: [0; LINEAR_TRANSLATION_WRITE_WORDS],
            ioram_active: false,
            #[cfg(feature = "jit")]
            rechecks: [TranslationRecheck::EMPTY; LINEAR_TRANSLATION_CACHE_SLOTS],
            #[cfg(feature = "jit")]
            recheck_context: 1,
            #[cfg(feature = "jit")]
            walk_root: 0,
        }
    }

    #[inline]
    pub fn invalidate(&mut self) {
        #[cfg(feature = "jit")]
        {
            self.recheck_context = self.recheck_context.wrapping_add(1);
            if self.recheck_context == 0 {
                self.rechecks.fill(TranslationRecheck::EMPTY);
                self.recheck_context = 1;
            }
        }
        self.invalidate_for_cr3();
    }

    fn invalidate_for_cr3(&mut self) {
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        crate::chipinterp::native::flush();
        #[cfg(feature = "jit")]
        {
            self.walk_root = 0;
        }
        self.global_entries.fill(0);
        let user = self.state & LINEAR_TRANSLATION_USER;
        let next_generation = (self.state & LINEAR_TRANSLATION_GENERATION_MASK)
            .wrapping_add(LINEAR_TRANSLATION_GENERATION_STEP)
            & LINEAR_TRANSLATION_GENERATION_MASK;
        if next_generation == 0 {
            self.entries = [LinearTranslationCacheEntry::EMPTY; LINEAR_TRANSLATION_CACHE_SLOTS];
            self.write_permissions = [0; LINEAR_TRANSLATION_WRITE_WORDS];
            self.state = LINEAR_TRANSLATION_GENERATION_STEP | user;
        } else {
            self.state = next_generation | user;
        }
    }

    fn invalidate_non_global(&mut self) {
        let old = self.state & LINEAR_TRANSLATION_GENERATION_MASK;
        let globals = self.global_entries;
        self.invalidate_for_cr3();
        let generation = self.state & LINEAR_TRANSLATION_GENERATION_MASK;
        for (word, mut bits) in globals.into_iter().enumerate() {
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let entry = &mut self.entries[word * 64 + bit];
                if entry.tag & LINEAR_TRANSLATION_GENERATION_MASK == old {
                    entry.tag = generation | (entry.tag & !LINEAR_TRANSLATION_GENERATION_MASK);
                    #[cfg(feature = "jit")]
                    if entry.write_tag != 0 {
                        entry.write_tag = entry.tag;
                    }
                    self.global_entries[word] |= 1 << bit;
                }
            }
        }
    }

    fn invalidate_linear_range(&mut self, base: u32, mask: u32) {
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        crate::chipinterp::native::flush();
        let entries = if mask == LINEAR_TRANSLATION_PAGE_MASK {
            let slot = Self::slot(LinearAddr(base));
            &mut self.entries[slot..slot + 1]
        } else {
            &mut self.entries[..]
        };
        for entry in entries {
            if entry.tag as u32 & mask == base {
                *entry = LinearTranslationCacheEntry::EMPTY;
            }
        }
        // A stale bitmap bit cannot grant access without a matching tag. Fill
        // replaces it before publishing a new translation in the empty row.
    }

    /// A JIT hardware access miss must reach the shared walker even when this
    /// derived software row survived eviction of the separate shadow cache.
    /// This is a data-cache eviction, not guest INVLPG or code invalidation.
    #[cfg(all(feature = "jit", feature = "monitor", target_os = "none"))]
    pub(crate) fn discard_hard_access_miss(&mut self, linear: LinearAddr) {
        self.invalidate_linear_range(
            linear.0 & LINEAR_TRANSLATION_PAGE_MASK,
            LINEAR_TRANSLATION_PAGE_MASK,
        );
    }

    #[inline]
    fn set_current_user(&mut self, user: bool) {
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        if user != (self.state & LINEAR_TRANSLATION_USER != 0) {
            crate::chipinterp::native::disable_guest();
            crate::chipinterp::native::flush();
        }
        // Entries already include their access privilege in the tag. A CPL
        // transition selects that tag; it does not change any page mapping.
        // Supervisor entries remain unavailable to user accesses, including
        // their write tokens. Architectural mapping invalidation retires both.
        self.state = (self.state & !LINEAR_TRANSLATION_USER)
            | if user { LINEAR_TRANSLATION_USER } else { 0 };
    }

    #[inline(always)]
    fn slot(linear: LinearAddr) -> usize {
        ((linear.0 >> 12) as usize) & (LINEAR_TRANSLATION_CACHE_SLOTS - 1)
    }

    #[inline(always)]
    fn write_allowed(&self, slot: usize) -> bool {
        (self.write_permissions[slot >> 6] & (1u64 << (slot & 63))) != 0
    }

    #[inline(always)]
    fn set_write_allowed(&mut self, slot: usize, allowed: bool) {
        let bit = 1u64 << (slot & 63);
        let word = &mut self.write_permissions[slot >> 6];
        *word = (*word & !bit) | (u64::from(allowed) << (slot & 63));
    }

    #[inline(always)]
    fn lookup(&self, linear: LinearAddr, user: bool, write: bool) -> Option<BackingToken> {
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        if user == (self.state & LINEAR_TRANSLATION_USER != 0) {
            let hit = if write {
                crate::chipinterp::native::lookup::<true>(self, linear.0)
            } else {
                crate::chipinterp::native::lookup::<false>(self, linear.0)
            };
            if hit.is_some() {
                return hit;
            }
        }
        let slot = Self::slot(linear);
        let entry = self.entries[slot];
        let linear_page = linear.0 & LINEAR_TRANSLATION_PAGE_MASK;
        let user_flag = if user { LINEAR_TRANSLATION_USER } else { 0 };
        let expected = Self::tag(
            linear_page,
            (self.state & LINEAR_TRANSLATION_GENERATION_MASK) | user_flag,
        );
        if entry.tag != expected || (write && !self.write_allowed(slot)) {
            return None;
        }
        let backing = BackingToken(entry.bias.wrapping_add(u64::from(linear.0)));
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        if user == (self.state & LINEAR_TRANSLATION_USER != 0) {
            crate::chipinterp::native::publish(self, linear.0, backing, self.write_allowed(slot));
        }
        Some(backing)
    }

    #[inline(always)]
    fn lookup_current_read(&self, linear: LinearAddr) -> Option<BackingToken> {
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        if let Some(hit) = crate::chipinterp::native::lookup::<false>(self, linear.0) {
            return Some(hit);
        }
        let slot = Self::slot(linear);
        let entry = self.entries[slot];
        let linear_page = linear.0 & LINEAR_TRANSLATION_PAGE_MASK;
        if entry.tag != Self::tag(linear_page, self.state) {
            return None;
        }
        let backing = BackingToken(entry.bias.wrapping_add(u64::from(linear.0)));
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        crate::chipinterp::native::publish(self, linear.0, backing, self.write_allowed(slot));
        Some(backing)
    }

    #[inline(always)]
    fn lookup_current_write(&self, linear: LinearAddr) -> Option<BackingToken> {
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        if let Some(hit) = crate::chipinterp::native::lookup::<true>(self, linear.0) {
            return Some(hit);
        }
        let slot = Self::slot(linear);
        let entry = self.entries[slot];
        let linear_page = linear.0 & LINEAR_TRANSLATION_PAGE_MASK;
        if entry.tag != Self::tag(linear_page, self.state) || !self.write_allowed(slot) {
            return None;
        }
        let backing = BackingToken(entry.bias.wrapping_add(u64::from(linear.0)));
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        crate::chipinterp::native::publish(self, linear.0, backing, self.write_allowed(slot));
        Some(backing)
    }

    #[inline(always)]
    fn fill(&mut self, linear: LinearAddr, backing: BackingToken, user: bool, write: bool) {
        let slot = Self::slot(linear);
        self.ioram_entries[slot >> 6] &= !(1u64 << (slot & 63));
        #[cfg(feature = "jit")]
        {
            self.rechecks[slot] = TranslationRecheck::EMPTY;
        }
        self.global_entries[slot >> 6] &= !(1u64 << (slot & 63));
        let linear_page = linear.0 & LINEAR_TRANSLATION_PAGE_MASK;
        let bias = backing.0.wrapping_sub(u64::from(linear.0));
        let user_flag = if user { LINEAR_TRANSLATION_USER } else { 0 };
        let current = self.entries[slot];
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        crate::chipinterp::native::invalidate(current.tag as u32 & LINEAR_TRANSLATION_PAGE_MASK);
        let expected_identity = Self::tag(
            linear_page,
            (self.state & LINEAR_TRANSLATION_GENERATION_MASK) | user_flag,
        );
        let preserve_write =
            current.tag == expected_identity && current.bias == bias && self.write_allowed(slot);
        self.set_write_allowed(slot, write || preserve_write);
        self.entries[slot] = LinearTranslationCacheEntry {
            bias,
            tag: expected_identity,
            #[cfg(feature = "jit")]
            write_tag: if write || preserve_write {
                expected_identity
            } else {
                0
            },
        };
    }

    pub(crate) fn mark_ioram(&mut self, linear: LinearAddr) {
        self.ioram_active = true;
        let slot = Self::slot(linear);
        self.ioram_entries[slot >> 6] |= 1u64 << (slot & 63);
        // Native resident rechecks only certify guest page tables. They must
        // not resurrect a device pointer without asking its owner again.
        #[cfg(feature = "jit")]
        {
            self.rechecks[slot] = TranslationRecheck::EMPTY;
        }
    }

    pub(crate) fn invalidate_ioram(&mut self) {
        if !core::mem::take(&mut self.ioram_active) {
            return;
        }
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        crate::chipinterp::native::flush();
        for (word, slots) in self.ioram_entries.iter_mut().enumerate() {
            let mut bits = core::mem::take(slots);
            self.global_entries[word] &= !bits;
            self.write_permissions[word] &= !bits;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let slot = word * 64 + bit;
                self.entries[slot] = LinearTranslationCacheEntry::EMPTY;
                #[cfg(feature = "jit")]
                {
                    self.rechecks[slot] = TranslationRecheck::EMPTY;
                }
            }
        }
    }

    #[inline(always)]
    const fn tag(linear_page: u32, state: TranslationTag) -> TranslationTag {
        (linear_page as TranslationTag) | state
    }
}

impl FetchCache {
    pub const fn new() -> Self {
        Self {
            entries: [FetchCacheEntry::EMPTY; FETCH_CACHE_SLOTS],
            phys_filter: [0; FETCH_PHYS_FILTER_WORDS],
            generation: FETCH_CACHE_GENERATION_STEP,
            valid_count: 0,
            global_entries: 0,
            #[cfg(feature = "jit")]
            rechecks: [TranslationRecheck::EMPTY; FETCH_CACHE_SLOTS],
        }
    }

    #[inline]
    pub fn is_valid(&self) -> bool {
        self.valid_count != 0
    }

    #[inline]
    pub fn invalidate(&mut self) {
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        crate::chipinterp::native::flush();
        self.global_entries = 0;
        let next = self.generation.wrapping_add(FETCH_CACHE_GENERATION_STEP);
        if next == 0 {
            self.entries = [FetchCacheEntry::EMPTY; FETCH_CACHE_SLOTS];
            self.generation = FETCH_CACHE_GENERATION_STEP;
        } else {
            self.generation = next;
        }
        self.phys_filter = [0; FETCH_PHYS_FILTER_WORDS];
        self.valid_count = 0;
    }

    fn invalidate_non_global(&mut self) {
        let old = self.generation;
        let mut globals = self.global_entries;
        self.invalidate();
        while globals != 0 {
            let slot = globals.trailing_zeros() as usize;
            globals &= globals - 1;
            let entry = &mut self.entries[slot];
            if entry.valid_for(old) {
                entry.tag = self.generation | (entry.tag & !FETCH_CACHE_GENERATION_MASK);
                let phys = entry.phys_page;
                self.record_phys_page(phys);
                self.valid_count += 1;
                self.global_entries |= 1 << slot;
            }
        }
    }

    fn invalidate_linear_range(&mut self, base: u32, mask: u32) -> bool {
        let entries = if mask == FETCH_PAGE_MASK {
            let slot = Self::slot(LinearAddr(base));
            &mut self.entries[slot..slot + 1]
        } else {
            &mut self.entries[..]
        };
        let mut invalidated = false;
        for entry in entries {
            if entry.valid_for(self.generation) && entry.tag as u32 & mask == base {
                entry.tag = 0;
                self.valid_count -= 1;
                invalidated = true;
            }
        }
        // The physical filter may keep false positives, never false negatives.
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        if invalidated {
            crate::chipinterp::native::flush();
        }
        invalidated
    }

    #[inline(always)]
    fn slot(linear: LinearAddr) -> usize {
        ((linear.0 >> 12) as usize) & (FETCH_CACHE_SLOTS - 1)
    }

    #[inline]
    fn phys_filter_index(phys_page: u64) -> (usize, u64) {
        let bit = (phys_page >> 12) & (FETCH_PHYS_FILTER_BITS - 1);
        ((bit >> 6) as usize, 1u64 << (bit & 63))
    }

    #[inline]
    fn phys_filter_contains(&self, phys_page: u64) -> bool {
        let (word, bit) = Self::phys_filter_index(phys_page);
        (self.phys_filter[word] & bit) != 0
    }

    #[inline(always)]
    fn record_phys_page(&mut self, phys_page: u64) {
        let (word, bit) = Self::phys_filter_index(phys_page);
        self.phys_filter[word] |= bit;
    }

    #[inline(always)]
    fn lookup(&self, linear: LinearAddr) -> Option<FetchCacheHit> {
        let entry = self.entries[Self::slot(linear)];
        let linear_page = linear.0 & FETCH_PAGE_MASK;
        if entry.tag != self.generation | u64::from(linear_page) {
            return None;
        }
        let offset = u64::from(linear.0 & FETCH_PAGE_OFFSET_MASK);
        Some(FetchCacheHit {
            phys: PhysAddr(entry.phys_page | offset),
            backing: BackingToken(entry.backing_page.wrapping_add(offset)),
        })
    }

    #[inline(always)]
    fn fill(&mut self, linear: LinearAddr, phys: PhysAddr, backing: BackingToken, global: bool) {
        let phys_page = phys.0 & !u64::from(FETCH_PAGE_OFFSET_MASK);
        let backing_page = backing
            .0
            .wrapping_sub(phys.0 & u64::from(FETCH_PAGE_OFFSET_MASK));
        let slot = Self::slot(linear);
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        if self.entries[slot].valid_for(self.generation) {
            crate::chipinterp::native::invalidate(self.entries[slot].tag as u32 & FETCH_PAGE_MASK);
        }
        #[cfg(feature = "jit")]
        {
            self.rechecks[slot] = TranslationRecheck::EMPTY;
        }
        self.global_entries = (self.global_entries & !(1 << slot)) | (u64::from(global) << slot);
        if !self.entries[slot].valid_for(self.generation) {
            self.valid_count = self.valid_count.saturating_add(1);
        }
        self.entries[slot] = FetchCacheEntry {
            backing_page,
            phys_page,
            tag: self.generation | u64::from(linear.0 & FETCH_PAGE_MASK),
        };
        self.record_phys_page(phys_page);
    }

    #[inline]
    fn invalidate_for_phys_write(&mut self, write: PhysWrite) -> bool {
        if write.len == 0 || self.valid_count == 0 {
            return false;
        }
        self.invalidate_for_phys_range(write.addr, u32::from(write.len))
    }

    #[inline]
    fn invalidate_for_phys_range(&mut self, addr: PhysAddr, len: u32) -> bool {
        if len == 0 || self.valid_count == 0 {
            return false;
        }
        let Some(end) = addr.0.checked_add(u64::from(len - 1)) else {
            let was_valid = self.is_valid();
            self.invalidate();
            return was_valid;
        };
        let start_page = addr.0 & !u64::from(FETCH_PAGE_OFFSET_MASK);
        let end_page = end & !u64::from(FETCH_PAGE_OFFSET_MASK);
        let page_count = ((end_page - start_page) >> 12) + 1;
        let mut maybe_touches_cached_page = page_count >= FETCH_PHYS_FILTER_BITS;
        if !maybe_touches_cached_page {
            let mut page = start_page;
            while page <= end_page {
                if self.phys_filter_contains(page) {
                    maybe_touches_cached_page = true;
                    break;
                }
                if page == end_page {
                    break;
                }
                page += 4096;
            }
        }
        if !maybe_touches_cached_page {
            return false;
        }
        let mut invalidated = false;
        let mut index = 0;
        while index < self.entries.len() {
            let entry = &mut self.entries[index];
            if entry.valid_for(self.generation) {
                let page_start = entry.phys_page;
                let page_end = page_start | u64::from(FETCH_PAGE_OFFSET_MASK);
                if addr.0 <= page_end && end >= page_start {
                    #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
                    crate::chipinterp::native::invalidate(entry.tag as u32 & FETCH_PAGE_MASK);
                    entry.tag = 0;
                    self.valid_count -= 1;
                    invalidated = true;
                }
            }
            index += 1;
        }
        invalidated
    }
}

impl FetchCursor {
    pub const fn new() -> Self {
        Self {
            window_len: 0,
            u16_start_count: 0,
            u32_start_count: 0,
            start_eip: 0,
            start_backing: 0,
        }
    }

    #[inline(always)]
    pub fn invalidate(&mut self) {
        self.window_len = 0;
        self.u16_start_count = 0;
        self.u32_start_count = 0;
    }

    #[inline(always)]
    pub fn take(&self, eip: u32, len: u32) -> Option<BackingToken> {
        let offset = eip.checked_sub(self.start_eip)?;
        if len == 0 || offset.checked_add(len)? > u32::from(self.window_len) {
            return None;
        }
        Some(BackingToken(
            self.start_backing.wrapping_add(u64::from(offset)),
        ))
    }

    #[inline(always)]
    pub fn take_u8(&self, eip: u32) -> Option<BackingToken> {
        let offset = eip.wrapping_sub(self.start_eip);
        if offset >= u32::from(self.window_len) {
            return None;
        }
        Some(BackingToken(
            self.start_backing.wrapping_add(u64::from(offset)),
        ))
    }

    #[inline(always)]
    pub fn take_u16(&self, eip: u32) -> Option<BackingToken> {
        let offset = eip.wrapping_sub(self.start_eip);
        if offset >= u32::from(self.u16_start_count) {
            return None;
        }
        Some(BackingToken(
            self.start_backing.wrapping_add(u64::from(offset)),
        ))
    }

    #[inline(always)]
    pub fn take_u32(&self, eip: u32) -> Option<BackingToken> {
        let offset = eip.wrapping_sub(self.start_eip);
        if offset >= u32::from(self.u32_start_count) {
            return None;
        }
        Some(BackingToken(
            self.start_backing.wrapping_add(u64::from(offset)),
        ))
    }

    #[inline(always)]
    pub fn fill_after(&mut self, eip: u32, backing: BackingToken, len: u32, window_len: u32) {
        if len == 0 || window_len < len || window_len > 4096 {
            self.invalidate();
            return;
        }
        self.window_len = window_len as u16;
        self.u16_start_count = window_len.saturating_sub(1) as u16;
        self.u32_start_count = window_len.saturating_sub(3) as u16;
        self.start_eip = eip;
        self.start_backing = backing.0;
    }
}

impl Cpu {
    pub const fn new() -> Self {
        let null_seg = SegmentCache::new(0, 0, 0, 0);
        // Bochs init.cc initializes real caches as present, accessed RW data.
        let real_seg = SegmentCache::new(0, 0, 0xffff, 0x0093);
        Self {
            regs: [0; 8],
            eip: 0,
            eflags: 0x0000_0002,
            lazy_flags: LazyFlags::NONE,
            cr0: 0,
            cr2: 0,
            cr3: 0,
            cr4: 0,
            pae_pdptrs: [0; 4],
            dr: [0; 8],
            gdtr: TableReg::empty(),
            idtr: TableReg::empty(),
            ldtr: 0,
            ldtr_cache: null_seg,
            tr: 0,
            tr_cache: null_seg,
            msr_apic_base: 0,
            msr_test_ctl: 0,
            msr_bios_update_trigger: 0,
            msr_bbl_cr_data: [0; 4],
            msr_bbl_cr_addr: 0,
            msr_bbl_cr_decc: 0,
            msr_bbl_cr_ctl: 0,
            msr_bbl_cr_trig: 0,
            msr_bbl_cr_busy: 0,
            msr_bbl_cr_ctl3: 0,
            msr_misc_enable: 0,
            msr_perfctr: [0; 2],
            msr_evntsel: [0; 2],
            msr_mtrr_phys: [0; 16],
            msr_mtrr_fix64k_00000: 0,
            msr_mtrr_fix16k: [0; 2],
            msr_mtrr_fix4k: [0; 8],
            msr_mtrr_def_type: 0,
            msr_pat: 0x0007_0406_0007_0406,
            msr_mcg_ctl: 0,
            msr_mca_ctl: [0; 5],
            msr_sysenter_cs: 0,
            msr_sysenter_esp: 0,
            msr_sysenter_eip: 0,
            tsc_base: 0,
            #[cfg(feature = "r86chip")]
            tsc_remaining: 0,
            fpu: FpuState::new(),
            segment_override: None,
            segments: [real_seg; 6],
            flat_segment_read_mask: 0,
            flat_segment_write_mask: 0,
            #[cfg(feature = "jit")]
            jit_based_read_mask: 0,
            #[cfg(feature = "jit")]
            jit_based_write_mask: 0,
            code_default16: true,
            cached_cpl: 0,
            halted: false,
            interrupt_shadow: false,
            interrupt_attention: false,
            fetch_cache: FetchCache::new(),
            fetch_cursor: FetchCursor::new(),
            translation_cache: LinearTranslationCache::new(),
            large_translation_regions: [0; 16],
            #[cfg(feature = "jit")]
            jit_mapping_epoch: 0,
            #[cfg(feature = "jit")]
            jit_global_epoch: 0,
            #[cfg(feature = "jit")]
            jit_invalidated_range: None,
            cache_stats: CpuCacheStats::new(),
            #[cfg(feature = "r86chip")]
            chip_counts: crate::chipinterp::Counts::ZERO,
        }
    }

    pub fn reset_bios(&mut self) {
        let null_seg = SegmentCache::new(0, 0, 0, 0);
        let real_seg = SegmentCache::new(0, 0, 0xffff, 0x0093);
        self.regs = [0; 8];
        self.eip = 0x0000_fff0;
        self.eflags = 0x0000_0002;
        self.lazy_flags = LazyFlags::NONE;
        self.cr0 = 0x0000_0010;
        self.cr2 = 0;
        self.cr3 = 0;
        self.cr4 = 0;
        self.pae_pdptrs = [0; 4];
        self.dr = [0; 8];
        self.gdtr = TableReg::empty();
        // The reset IVT is usable before firmware executes LIDT.
        self.idtr = TableReg {
            base: 0,
            limit: 0xffff,
        };
        self.ldtr = 0;
        self.ldtr_cache = null_seg;
        self.tr = 0;
        self.tr_cache = null_seg;
        self.msr_apic_base = 0;
        self.msr_test_ctl = 0;
        self.msr_bios_update_trigger = 0;
        self.msr_bbl_cr_data = [0; 4];
        self.msr_bbl_cr_addr = 0;
        self.msr_bbl_cr_decc = 0;
        self.msr_bbl_cr_ctl = 0;
        self.msr_bbl_cr_trig = 0;
        self.msr_bbl_cr_busy = 0;
        self.msr_bbl_cr_ctl3 = 0;
        self.msr_misc_enable = 0;
        self.msr_perfctr = [0; 2];
        self.msr_evntsel = [0; 2];
        self.msr_mtrr_phys = [0; 16];
        self.msr_mtrr_fix64k_00000 = 0;
        self.msr_mtrr_fix16k = [0; 2];
        self.msr_mtrr_fix4k = [0; 8];
        self.msr_mtrr_def_type = 0;
        self.msr_pat = 0x0007_0406_0007_0406;
        self.msr_mcg_ctl = 0;
        self.msr_mca_ctl = [0; 5];
        self.msr_sysenter_cs = 0;
        self.msr_sysenter_esp = 0;
        self.msr_sysenter_eip = 0;
        self.set_tsc(0);
        self.fpu.init();
        self.segment_override = None;
        self.segments = [real_seg; 6];
        self.flat_segment_read_mask = 0;
        self.flat_segment_write_mask = 0;
        #[cfg(feature = "jit")]
        {
            self.jit_based_read_mask = 0;
            self.jit_based_write_mask = 0;
        }
        self.set_seg_cache(
            SegmentReg::Cs,
            SegmentCache::new(0xf000, 0x000f_0000, 0xffff, 0x0093),
        );
        self.halted = false;
        self.interrupt_shadow = false;
        self.interrupt_attention = false;
        self.cache_stats = CpuCacheStats::default();
        self.fetch_cursor.invalidate();
        self.invalidate_translation_context();
    }

    pub fn enter_protected_flat(&mut self, eip: u32, boot_params: u32) {
        let null_seg = SegmentCache::new(0, 0, 0, 0);
        self.regs = [0; 8];
        self.regs[Reg32::Esi as usize] = boot_params;
        self.regs[Reg32::Esp as usize] = 0x0009_0000;
        self.eip = eip;
        self.eflags = 0x0000_0002;
        self.lazy_flags = LazyFlags::NONE;
        self.cr0 = CR0_PE;
        self.cr2 = 0;
        self.cr3 = 0;
        self.cr4 = 0;
        self.dr = [0; 8];
        self.gdtr = TableReg::empty();
        self.idtr = TableReg::empty();
        self.ldtr = 0;
        self.ldtr_cache = null_seg;
        self.tr = 0;
        self.tr_cache = null_seg;
        self.msr_apic_base = 0;
        self.msr_test_ctl = 0;
        self.msr_bios_update_trigger = 0;
        self.msr_bbl_cr_data = [0; 4];
        self.msr_bbl_cr_addr = 0;
        self.msr_bbl_cr_decc = 0;
        self.msr_bbl_cr_ctl = 0;
        self.msr_bbl_cr_trig = 0;
        self.msr_bbl_cr_busy = 0;
        self.msr_bbl_cr_ctl3 = 0;
        self.msr_misc_enable = 0;
        self.msr_perfctr = [0; 2];
        self.msr_evntsel = [0; 2];
        self.msr_mtrr_phys = [0; 16];
        self.msr_mtrr_fix64k_00000 = 0;
        self.msr_mtrr_fix16k = [0; 2];
        self.msr_mtrr_fix4k = [0; 8];
        self.msr_mtrr_def_type = 0;
        self.msr_pat = 0x0007_0406_0007_0406;
        self.msr_mcg_ctl = 0;
        self.msr_mca_ctl = [0; 5];
        self.msr_sysenter_cs = 0;
        self.msr_sysenter_esp = 0;
        self.msr_sysenter_eip = 0;
        self.set_tsc(0);
        self.fpu.init();
        self.segment_override = None;
        self.set_seg_cache(SegmentReg::Cs, SegmentCache::flat(0x10, 0xc09a));
        for seg in [
            SegmentReg::Ds,
            SegmentReg::Es,
            SegmentReg::Ss,
            SegmentReg::Fs,
            SegmentReg::Gs,
        ] {
            self.set_seg_cache(seg, SegmentCache::flat(0x18, 0xc092));
        }
        self.halted = false;
        self.interrupt_shadow = false;
        self.interrupt_attention = false;
        self.cache_stats = CpuCacheStats::default();
        self.fetch_cursor.invalidate();
        self.invalidate_translation_context();
    }

    #[inline(always)]
    pub fn reg(&self, reg: Reg32) -> u32 {
        self.regs[reg as usize]
    }

    #[inline(always)]
    pub fn set_reg(&mut self, reg: Reg32, value: u32) {
        self.regs[reg as usize] = value;
    }

    #[inline(always)]
    pub fn seg(&self, seg: SegmentReg) -> SegmentCache {
        self.segments[seg as usize]
    }

    pub fn segments(&self) -> &[SegmentCache; 6] {
        &self.segments
    }

    #[inline(always)]
    pub fn cpl(&self) -> u8 {
        self.cached_cpl
    }

    /// Current deterministic guest counter, including the current attempt.
    /// On the chip, a batch publishes its u32 countdown once per instruction;
    /// this getter subtracts it from the 64-bit base at every observation. Time
    /// is never rounded to a batch boundary. Other builds store the full value
    /// directly and compile this accessor to the original single field read.
    #[inline(always)]
    pub fn tsc(&self) -> u64 {
        #[cfg(feature = "r86chip")]
        return self.tsc_base.wrapping_sub(u64::from(self.tsc_remaining));
        #[cfg(not(feature = "r86chip"))]
        self.tsc_base
    }

    /// WRMSR, reset and state import take effect immediately, even midway
    /// through a chip batch. Preserve its progress coordinate by adjusting the
    /// base; the next attempt advances from the newly written value by one.
    /// Both addition and observation wrap modulo 2^64, as guest TSC does.
    #[inline(always)]
    pub fn set_tsc(&mut self, value: u64) {
        #[cfg(feature = "r86chip")]
        let value = value.wrapping_add(u64::from(self.tsc_remaining));
        self.tsc_base = value;
    }

    /// Charge time outside a chip batch (ordinary interpretation, JIT blocks
    /// or deterministic halted fast-forward). Existing batch progress remains
    /// represented exactly; adding to the base advances the visible total.
    #[inline(always)]
    pub fn advance_tsc(&mut self, elapsed: u64) {
        self.tsc_base = self.tsc_base.wrapping_add(elapsed);
    }

    /// Re-anchor before a new batch/chunk reuses the local u32 countdown.
    /// No guest state is duplicated or deferred to a later observer. In
    /// particular, faults/REP exits expose the latest elapsed attempt too.
    #[cfg(all(feature = "r86chip", any(target_arch = "riscv32", test)))]
    #[inline(always)]
    pub(crate) fn begin_tsc_batch(&mut self, attempts: u32) {
        self.tsc_base = self.tsc().wrapping_add(u64::from(attempts));
        self.tsc_remaining = attempts;
    }

    /// Caller supplies the batch attempts remaining AFTER the current attempt.
    /// This is the execution loop's existing countdown; no separate increment
    /// or subtraction from its original budget is needed on the common path.
    /// A larger caller budget starts another chunk after re-anchoring. Exits
    /// may leave a nonzero remainder: tsc() is still the exact current value,
    /// and the next batch re-anchors without charging unexecuted instructions.
    /// Publishing this word replaces a per-instruction 64-bit increment on RV32.
    #[cfg(all(feature = "r86chip", any(target_arch = "riscv32", test)))]
    #[inline(always)]
    pub(crate) fn set_tsc_remaining(&mut self, remaining: u32) {
        self.tsc_remaining = remaining;
    }

    /// CPL after loading CS with `selector`. It is CS.RPL only in protected
    /// mode: real mode runs at CPL 0 and virtual-8086 mode at CPL 3, whatever
    /// the selector's low bits. Setting CR0.PE keeps CPL 0 until the next CS
    /// load, so real-mode code at an odd paragraph (CS=0x029E) can still run
    /// LGDT/LLDT and load selectors before its far jump. IRET to V86 loads CS
    /// before VM is set and an interrupt from V86 loads CS before VM clears;
    /// replace_eflags recomputes CPL when VM changes.
    fn mode_cpl(&self, selector: u16) -> u8 {
        if self.cr0 & CR0_PE == 0 {
            0
        } else if self.eflags & CACHE_CONTEXT_EFLAGS_VM != 0 {
            3
        } else {
            (selector & 3) as u8
        }
    }

    /// During task entry, an invalid incoming SS requires a stack switch if
    /// qualification faults. This transient CPL is independent of visible
    /// CS.RPL until SS has been loaded; ordinary CS loads restore the invariant.
    pub(super) fn task_switch_cpl(&mut self, cpl: u8) {
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        crate::chipinterp::native::disable_guest();
        self.cached_cpl = cpl;
    }

    /// Commits a segment-cache update together with all derived execution state.
    /// Direct segment mutation would leave permission and fetch caches stale.
    #[inline(always)]
    pub fn set_seg_cache(&mut self, seg: SegmentReg, cache: SegmentCache) {
        let previous = self.segments[seg as usize];
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        if previous != cache {
            crate::chipinterp::native::disable_guest();
        }
        self.segments[seg as usize] = cache;
        self.update_flat_segment_mask(seg);
        if matches!(seg, SegmentReg::Cs) {
            self.cached_cpl = self.mode_cpl(cache.selector);
            self.code_default16 = (cache.flags & SEGMENT_FLAG_DEFAULT_BIG) == 0;
            // The fetch cursor is CS-relative, whereas data translations are
            // linear and privilege-tagged. Reloading the same CS must not
            // discard valid mappings on every interrupt/IRET. A changed CS
            // still expires execute links and their embedded guest PCs.
            // Segment/prefetch changes are distinct from a paging TLB flush
            // (v86 cpu.rs also keeps the data TLB on CS cache updates).
            if previous != cache {
                #[cfg(feature = "jit")]
                let same_code = previous.base == cache.base
                    && previous.limit == cache.limit
                    && (previous.flags ^ cache.flags) & !0x60 == 0;
                #[cfg(not(feature = "jit"))]
                let same_code = false;
                if same_code {
                    // JIT execute permissions are cached separately for user
                    // and supervisor. A CPL change selects the corresponding
                    // table; it does not alter its checked mapping. The ordinary
                    // fetch cache has no privilege tag and still expires here.
                    self.invalidate_fetch_translations();
                } else {
                    self.invalidate_fetch_cache();
                }
            }
            self.fetch_cursor.invalidate();
            self.translation_cache
                .set_current_user(self.cached_cpl == 3);
        }
    }

    #[inline(always)]
    pub(crate) fn code_default16(&self) -> bool {
        self.code_default16
    }

    #[inline(always)]
    pub(crate) fn flat_segment_access(&self, seg: SegmentReg, write: bool) -> bool {
        let mask = if write {
            self.flat_segment_write_mask
        } else {
            self.flat_segment_read_mask
        };
        (mask & (1u8 << seg as u8)) != 0
    }

    #[inline(always)]
    pub(crate) fn flat_32bit_stack(&self) -> bool {
        (self.flat_segment_write_mask & FLAT_32BIT_STACK_MASK) != 0
    }

    #[cfg(feature = "jit")]
    pub(crate) fn jit_segment_masks(&self) -> (u32, u32) {
        // The frame view uses bit 6 for SS's flat 32-bit stack qualification.
        (
            u32::from(self.flat_segment_read_mask)
                | (u32::from(self.flat_segment_write_mask & FLAT_32BIT_STACK_MASK) >> 1),
            u32::from(self.flat_segment_write_mask & 0x3f),
        )
    }

    #[cfg(feature = "jit")]
    pub(crate) fn jit_based_segments(&self) -> ([u32; 2], [u32; 2], u32, u32) {
        (
            [self.seg(SegmentReg::Fs).base, self.seg(SegmentReg::Gs).base],
            [
                self.seg(SegmentReg::Fs).limit,
                self.seg(SegmentReg::Gs).limit,
            ],
            u32::from(self.jit_based_read_mask),
            u32::from(self.jit_based_write_mask),
        )
    }

    #[inline(never)]
    fn update_flat_segment_mask(&mut self, seg: SegmentReg) {
        let bit = 1u8 << seg as u8;
        self.flat_segment_read_mask &= !bit;
        self.flat_segment_write_mask &= !bit;
        if seg == SegmentReg::Ss {
            self.flat_segment_write_mask &= !FLAT_32BIT_STACK_MASK;
        }
        let cache = self.segments[seg as usize];
        #[cfg(feature = "jit")]
        if matches!(seg, SegmentReg::Fs | SegmentReg::Gs) {
            // Segment loads own qualification, just as for the flat masks.
            // Rechecking descriptors at each native entry duplicates work;
            // emitted scalar accesses still check offset+width before base.
            self.jit_based_read_mask &= !bit;
            self.jit_based_write_mask &= !bit;
            let mut based = cache;
            based.base = 0;
            // The native FS/GS access checks its actual limit before paging.
            // Reuse the normal-data qualification, including null/read-only
            // and expand-down rejection, independently of the segment range.
            based.limit = u32::MAX;
            if flat_data_segment(based, false) {
                self.jit_based_read_mask |= bit;
                if flat_data_segment(based, true) {
                    self.jit_based_write_mask |= bit;
                }
            }
        }
        if flat_data_segment(cache, false) {
            self.flat_segment_read_mask |= bit;
        }
        if flat_data_segment(cache, true) {
            self.flat_segment_write_mask |= bit;
            if seg == SegmentReg::Ss && (cache.flags & SEGMENT_FLAG_DEFAULT_BIG) != 0 {
                self.flat_segment_write_mask |= FLAT_32BIT_STACK_MASK;
            }
        }
    }

    #[inline(always)]
    pub fn fetch_cache_lookup(&mut self, linear: LinearAddr) -> Option<FetchCacheHit> {
        let hit = self.fetch_cache.lookup(linear);
        #[cfg(test)]
        if hit.is_some() {
            self.cache_stats.fetch_translation_hits =
                self.cache_stats.fetch_translation_hits.wrapping_add(1);
        }
        hit
    }

    #[inline(always)]
    pub fn fetch_cache_phys(&mut self, linear: LinearAddr) -> Option<PhysAddr> {
        self.fetch_cache_lookup(linear).map(|hit| hit.phys)
    }

    #[inline(always)]
    pub fn fetch_cursor_backing(&mut self, eip: u32, len: u32) -> Option<BackingToken> {
        self.fetch_cursor.take(eip, len)
    }

    #[inline(always)]
    pub fn fetch_cursor_backing_u8(&self, eip: u32) -> Option<BackingToken> {
        self.fetch_cursor.take_u8(eip)
    }

    #[inline(always)]
    pub fn fetch_cursor_backing_u16(&self, eip: u32) -> Option<BackingToken> {
        self.fetch_cursor.take_u16(eip)
    }

    #[inline(always)]
    pub fn fetch_cursor_backing_u32(&self, eip: u32) -> Option<BackingToken> {
        self.fetch_cursor.take_u32(eip)
    }

    #[inline(always)]
    pub fn fill_fetch_cursor_after(
        &mut self,
        eip: u32,
        backing: BackingToken,
        len: u32,
        window_len: u32,
    ) {
        self.fetch_cursor.fill_after(eip, backing, len, window_len);
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        crate::chipinterp::native::publish_fetch(self, eip, backing);
    }

    #[inline(always)]
    pub fn fill_fetch_cache(&mut self, linear: LinearAddr, phys: PhysAddr) {
        self.fill_fetch_cache_with_backing(linear, phys, BackingToken(phys.0), false);
    }

    #[inline(always)]
    pub fn fill_fetch_cache_with_backing(
        &mut self,
        linear: LinearAddr,
        phys: PhysAddr,
        backing: BackingToken,
        global: bool,
    ) {
        self.fetch_cache.fill(linear, phys, backing, global);
        #[cfg(test)]
        {
            self.cache_stats.fetch_translation_fills =
                self.cache_stats.fetch_translation_fills.wrapping_add(1);
        }
    }

    #[inline]
    pub fn invalidate_fetch_cache(&mut self) {
        #[cfg(feature = "jit")]
        {
            self.jit_invalidated_range = None;
            self.jit_mapping_epoch = self
                .jit_mapping_epoch
                .checked_add(1)
                .expect("JIT mapping epoch exhausted");
            self.jit_global_epoch = self.jit_mapping_epoch;
        }
        self.invalidate_fetch_translations();
    }

    #[inline]
    fn invalidate_fetch_translations(&mut self) {
        if self.fetch_cache.is_valid() {
            #[cfg(test)]
            {
                self.cache_stats.fetch_translation_invalidations = self
                    .cache_stats
                    .fetch_translation_invalidations
                    .wrapping_add(1);
            }
            self.fetch_cache.invalidate();
            self.fetch_cursor.invalidate();
        }
    }

    #[cfg(any(
        all(feature = "jit", target_arch = "arm"),
        any(
            all(feature = "wasm-jit", target_arch = "wasm32"),
            all(
                feature = "jit",
                target_arch = "aarch64",
                any(
                    all(feature = "host", target_os = "linux"),
                    all(feature = "monitor", feature = "machine", target_os = "none")
                )
            )
        )
    ))]
    pub(crate) fn jit_mapping_epoch(&self) -> u64 {
        self.jit_mapping_epoch
    }

    #[cfg(any(
        all(feature = "jit", target_arch = "arm"),
        all(
            feature = "jit",
            target_arch = "aarch64",
            any(
                all(feature = "host", target_os = "linux"),
                all(feature = "monitor", feature = "machine", target_os = "none")
            )
        )
    ))]
    pub(crate) fn jit_global_epoch(&self) -> u64 {
        self.jit_global_epoch
    }

    #[cfg(any(
        all(feature = "jit", target_arch = "arm"),
        all(feature = "wasm-jit", target_arch = "wasm32"),
        all(
            feature = "jit",
            target_arch = "aarch64",
            any(
                all(feature = "host", target_os = "linux"),
                all(feature = "monitor", feature = "machine", target_os = "none")
            )
        )
    ))]
    pub(crate) fn fetch_translation_is_global(&self, linear: LinearAddr) -> bool {
        self.fetch_cache.global_entries & (1 << FetchCache::slot(linear)) != 0
    }

    #[cfg(any(
        all(feature = "jit", target_arch = "arm"),
        any(
            all(feature = "wasm-jit", target_arch = "wasm32"),
            all(
                feature = "jit",
                target_arch = "aarch64",
                any(
                    all(feature = "host", target_os = "linux"),
                    all(feature = "monitor", feature = "machine", target_os = "none")
                )
            )
        )
    ))]
    pub(crate) fn take_jit_invalidated_range(&mut self) -> Option<(u32, u32)> {
        self.jit_invalidated_range.take()
    }

    #[inline]
    pub fn invalidate_fetch_cache_for_phys_write(&mut self, write: PhysWrite) {
        if self.fetch_cache.invalidate_for_phys_write(write) {
            self.fetch_cursor.invalidate();
            #[cfg(test)]
            {
                self.cache_stats.fetch_translation_invalidations = self
                    .cache_stats
                    .fetch_translation_invalidations
                    .wrapping_add(1);
            }
        }
    }

    #[inline]
    pub fn invalidate_fetch_cache_for_phys_range(&mut self, addr: PhysAddr, len: u32) {
        if self.fetch_cache.invalidate_for_phys_range(addr, len) {
            self.fetch_cursor.invalidate();
            #[cfg(test)]
            {
                self.cache_stats.fetch_translation_invalidations = self
                    .cache_stats
                    .fetch_translation_invalidations
                    .wrapping_add(1);
            }
        }
    }

    #[inline(always)]
    pub fn cached_read_translation(
        &mut self,
        linear: LinearAddr,
        user: bool,
    ) -> Option<BackingToken> {
        let backing = self.translation_cache.lookup(linear, user, false);
        #[cfg(test)]
        if backing.is_some() {
            self.cache_stats.linear_translation_hits =
                self.cache_stats.linear_translation_hits.wrapping_add(1);
        }
        backing
    }

    #[inline(always)]
    pub(crate) fn cached_current_read_translation(
        &mut self,
        linear: LinearAddr,
    ) -> Option<BackingToken> {
        let backing = self.translation_cache.lookup_current_read(linear);
        #[cfg(test)]
        if backing.is_some() {
            self.cache_stats.linear_translation_hits =
                self.cache_stats.linear_translation_hits.wrapping_add(1);
        }
        backing
    }

    #[inline(always)]
    pub(crate) fn cached_current_write_translation(
        &mut self,
        linear: LinearAddr,
    ) -> Option<BackingToken> {
        let backing = self.translation_cache.lookup_current_write(linear);
        #[cfg(test)]
        if backing.is_some() {
            self.cache_stats.linear_translation_hits =
                self.cache_stats.linear_translation_hits.wrapping_add(1);
        }
        backing
    }

    #[inline(always)]
    pub fn cached_write_translation(
        &mut self,
        linear: LinearAddr,
        user: bool,
    ) -> Option<BackingToken> {
        let backing = self.translation_cache.lookup(linear, user, true);
        #[cfg(test)]
        if backing.is_some() {
            self.cache_stats.linear_translation_hits =
                self.cache_stats.linear_translation_hits.wrapping_add(1);
        }
        backing
    }

    #[inline(always)]
    pub fn fill_cached_read_translation(
        &mut self,
        linear: LinearAddr,
        backing: BackingToken,
        user: bool,
    ) {
        self.translation_cache.fill(linear, backing, user, false);
        #[cfg(test)]
        {
            self.cache_stats.linear_translation_fills =
                self.cache_stats.linear_translation_fills.wrapping_add(1);
        }
    }

    #[inline(always)]
    pub fn fill_cached_write_translation(
        &mut self,
        linear: LinearAddr,
        backing: BackingToken,
        user: bool,
    ) {
        self.translation_cache.fill(linear, backing, user, true);
        #[cfg(test)]
        {
            self.cache_stats.linear_translation_fills =
                self.cache_stats.linear_translation_fills.wrapping_add(1);
        }
    }

    #[inline]
    pub fn invalidate_translation_cache(&mut self) {
        #[cfg(test)]
        {
            self.cache_stats.linear_translation_invalidations = self
                .cache_stats
                .linear_translation_invalidations
                .wrapping_add(1);
        }
        self.translation_cache.invalidate();
    }

    #[inline(never)]
    pub fn invalidate_translation_context(&mut self) {
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        crate::chipinterp::native::disable_guest();
        self.invalidate_fetch_cache();
        self.invalidate_translation_cache();
        self.large_translation_regions.fill(0);
    }

    pub(crate) fn invalidate_ioram_translations(&mut self) {
        // Device data grants never certify instruction fetch. Retire their
        // shared rows and resident rechecks; native refill proofs last only
        // until the current invocation returns, before this sync can run.
        // Keep execute links qualified. Changes to executable backing (PAM,
        // PCI decode, A20) use invalidate_translation_context instead.
        self.translation_cache.invalidate_ioram();
    }

    /// CR3 reload retires nonglobal translations. Preserve global rows by
    /// retagging the small recorded set here, keeping ordinary hits unchanged.
    /// INVLPG, CR0 paging/permission changes and CR4 paging changes still expire
    /// globals. v86 agrees: cpu.rs keeps globals on CR3 and flushes them on CR0
    /// PG/WP; instructions_0f.rs flushes them on CR4 PGE/PSE/PAE changes.
    pub(crate) fn invalidate_non_global_translations(&mut self) {
        if self.cr4 & CR4_PGE == 0 {
            self.invalidate_fetch_cache();
            self.translation_cache.invalidate_for_cr3();
            self.large_translation_regions.fill(0);
            return;
        }
        #[cfg(feature = "jit")]
        {
            self.jit_mapping_epoch = self
                .jit_mapping_epoch
                .checked_add(1)
                .expect("JIT mapping epoch exhausted");
        }
        self.fetch_cache.invalidate_non_global();
        self.fetch_cursor.invalidate();
        self.translation_cache.invalidate_non_global();
        // Keep large-page hints: globals can still contain cached pieces.
    }

    pub(crate) fn mark_cached_translation_global(&mut self, linear: LinearAddr, global: bool) {
        let slot = LinearTranslationCache::slot(linear);
        if global {
            self.translation_cache.global_entries[slot >> 6] |= 1 << (slot & 63);
        }
    }

    #[cfg(feature = "jit")]
    pub(crate) fn record_translation_recheck(
        &mut self,
        linear: LinearAddr,
        pte_token: BackingToken,
        words: [u32; 2],
    ) {
        self.translation_cache.rechecks[LinearTranslationCache::slot(linear)] =
            TranslationRecheck {
                context: self.translation_cache.recheck_context,
                pte_token: pte_token.0,
                pde: words[0],
                pte: words[1],
            };
    }

    #[cfg(feature = "jit")]
    pub(crate) fn record_fetch_recheck(
        &mut self,
        linear: LinearAddr,
        pte_token: BackingToken,
        words: [u32; 2],
    ) {
        self.fetch_cache.rechecks[FetchCache::slot(linear)] = TranslationRecheck {
            context: self.translation_cache.recheck_context,
            pte_token: pte_token.0,
            pde: words[0],
            pte: words[1],
        };
    }

    #[cfg(all(
        feature = "jit",
        target_arch = "aarch64",
        any(
            all(feature = "host", target_os = "linux"),
            all(feature = "monitor", feature = "machine", target_os = "none")
        )
    ))]
    pub(crate) fn fetch_recheck(&self, linear: LinearAddr) -> TranslationRecheck {
        self.fetch_cache.rechecks[FetchCache::slot(linear)]
    }

    /// The JIT adapter has already validated this exact execute page/context.
    /// Restore its ordinary fetch entry without a second walk or new A/D work.
    #[cfg(all(
        feature = "jit",
        target_arch = "aarch64",
        any(
            all(feature = "host", target_os = "linux"),
            all(feature = "monitor", feature = "machine", target_os = "none")
        )
    ))]
    pub(crate) fn restore_fetch_translation(
        &mut self,
        linear: LinearAddr,
        hit: FetchCacheHit,
        global: bool,
        recheck: TranslationRecheck,
    ) {
        self.fill_fetch_cache_with_backing(linear, hit.phys, hit.backing, global);
        self.fetch_cache.rechecks[FetchCache::slot(linear)] = recheck;
    }

    pub(crate) fn note_large_translation(&mut self, linear: LinearAddr) {
        let region = (linear.0 >> 22) as usize;
        self.large_translation_regions[region >> 6] |= 1 << (region & 63);
    }

    /// INVLPG expires the addressed translation, including all cached pieces
    /// of a large page. Unrelated pages retain their checked mappings. v86
    /// cpu.rs invlpg clears only the addressed 4 KiB slot.
    pub fn invalidate_translation_page(&mut self, linear: LinearAddr) {
        let region = (linear.0 >> 22) as usize;
        let large = self.large_translation_regions[region >> 6] & (1 << (region & 63)) != 0;
        let mask = if large { 0xffc0_0000 } else { FETCH_PAGE_MASK };
        let base = linear.0 & mask;
        self.large_translation_regions[region >> 6] &= !(1 << (region & 63));
        let _invalidated = self.fetch_cache.invalidate_linear_range(base, mask);
        self.fetch_cursor.invalidate();
        self.translation_cache.invalidate_linear_range(base, mask);
        #[cfg(test)]
        {
            self.cache_stats.linear_translation_invalidations += 1;
            self.cache_stats.fetch_translation_invalidations += u64::from(_invalidated);
        }
        #[cfg(feature = "jit")]
        {
            let range = (base, mask);
            match self.jit_invalidated_range {
                None => self.jit_invalidated_range = Some(range),
                Some(previous) if previous == range => {}
                Some(_) => {
                    // Ordinary JIT boundaries consume one invalidation at a
                    // time. An interpreter-only batch may issue several: a
                    // full execute epoch safely coalesces them without a queue.
                    self.invalidate_fetch_cache();
                }
            }
        }
    }

    #[inline(always)]
    pub fn flag(&self, mask: u32) -> bool {
        #[cfg(not(all(feature = "r86chip-native", target_arch = "riscv32")))]
        if self.lazy_flags.pending() && (mask & EFLAGS_STATUS_MASK) != 0 {
            if mask.count_ones() == 1 {
                return self.lazy_flags.flag(self.eflags, mask);
            }
            return (self.effective_eflags() & mask) != 0;
        }
        (self.eflags & mask) != 0
    }

    #[inline(always)]
    pub fn effective_eflags(&self) -> u32 {
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        {
            return self.eflags | 2;
        }
        #[cfg(not(all(feature = "r86chip-native", target_arch = "riscv32")))]
        if self.lazy_flags.pending() {
            (self.eflags & !EFLAGS_STATUS_MASK)
                | self.lazy_flags.status_bits(self.eflags)
                | 0x0000_0002
        } else {
            self.eflags | 0x0000_0002
        }
    }

    #[inline]
    pub fn materialize_flags(&mut self) {
        #[cfg(not(all(feature = "r86chip-native", target_arch = "riscv32")))]
        if self.lazy_flags.pending() {
            self.eflags = (self.eflags & !EFLAGS_STATUS_MASK)
                | self.lazy_flags.status_bits(self.eflags)
                | 0x0000_0002;
            self.lazy_flags = LazyFlags::NONE;
        }
    }

    #[inline]
    pub fn replace_eflags(&mut self, value: u32) {
        let was_interruptible = self.maskable_interrupts_enabled();
        let vm_changed = ((self.eflags ^ value) & CACHE_CONTEXT_EFLAGS_VM) != 0;
        self.eflags = value | 0x0000_0002;
        self.lazy_flags = LazyFlags::NONE;
        if !was_interruptible && self.maskable_interrupts_enabled() {
            self.interrupt_attention = true;
        }
        if vm_changed {
            self.cached_cpl = self.mode_cpl(self.segments[SegmentReg::Cs as usize].selector);
            self.translation_cache
                .set_current_user(self.cached_cpl == 3);
            self.invalidate_translation_context();
        }
    }

    /// Native blocks may commit POPF/CLI/STI but never change VM. An IF 0->1
    /// transition exits with attention; STI's exit handler then installs its
    /// shadow before the machine can observe interruptibility. No subsequent
    /// guest instruction runs in the same native chain.
    #[cfg(feature = "jit")]
    #[inline]
    pub(crate) fn jit_commit_flags(&mut self, value: u32) {
        debug_assert_eq!((self.eflags ^ value) & CACHE_CONTEXT_EFLAGS_VM, 0);
        if self.eflags & EFLAGS_IF == 0 && value & EFLAGS_IF != 0 {
            self.interrupt_attention = true;
        }
        self.eflags = value | 2;
        self.lazy_flags = LazyFlags::NONE;
    }

    #[inline]
    pub fn set_flag(&mut self, mask: u32, set: bool) {
        debug_assert_eq!(mask & CACHE_CONTEXT_EFLAGS_VM, 0);
        let track_interruptibility = (mask & EFLAGS_IF) != 0;
        let was_interruptible = track_interruptibility && self.maskable_interrupts_enabled();
        if (mask & EFLAGS_STATUS_MASK) != 0 {
            self.materialize_flags();
        }
        if set {
            self.eflags |= mask;
        } else {
            self.eflags &= !mask;
        }
        self.eflags |= 0x0000_0002;
        if track_interruptibility && !was_interruptible && self.maskable_interrupts_enabled() {
            self.interrupt_attention = true;
        }
    }

    /// Pass pending arithmetic through the Wasm ABI without computing flags
    /// which its first instruction may overwrite.
    #[cfg(all(feature = "wasm-jit", any(test, target_arch = "wasm32")))]
    #[inline]
    pub(crate) fn jit_arithmetic(&self) -> (u32, u32, u32, u32) {
        (
            self.eflags | 2,
            self.lazy_flags.lhs,
            self.lazy_flags.result,
            self.lazy_flags.kind,
        )
    }

    /// Import the same compact lazy record used by the shared interpreter.
    /// Generated code has already width-masked lhs/result and cannot change VM.
    /// No operand-width switch or temporary materialization is needed here.
    #[cfg(all(feature = "wasm-jit", any(test, target_arch = "wasm32")))]
    #[inline]
    pub(crate) fn jit_commit_arithmetic(&mut self, flags: u32, lhs: u32, result: u32, kind: u32) {
        debug_assert_eq!((self.eflags ^ flags) & CACHE_CONTEXT_EFLAGS_VM, 0);
        debug_assert!(
            kind == 0
                || (matches!(kind >> 8, 7 | 15 | 31)
                    && matches!(kind & 31, 1..=6 | 18 | 20)
                    && kind & !0x1f5f == 0)
        );
        if self.eflags & EFLAGS_IF == 0 && flags & EFLAGS_IF != 0 {
            self.interrupt_attention = true;
        }
        self.eflags = flags | 2;
        self.lazy_flags = LazyFlags { lhs, result, kind };
    }

    #[inline(always)]
    pub fn maskable_interrupts_enabled(&self) -> bool {
        self.flag(EFLAGS_IF) && !self.interrupt_shadow
    }

    #[inline(always)]
    pub fn interrupt_shadow(&self) -> bool {
        self.interrupt_shadow
    }

    #[inline(always)]
    pub fn restore_interrupt_shadow(&mut self, inhibited: bool) {
        self.interrupt_shadow = inhibited;
        self.interrupt_attention = false;
    }

    #[inline(always)]
    pub(crate) fn begin_instruction(&mut self) {
        self.interrupt_attention = self.interrupt_shadow && (self.eflags & EFLAGS_IF) != 0;
        self.interrupt_shadow = false;
    }

    #[inline(always)]
    pub(crate) fn inhibit_maskable_interrupts(&mut self) {
        self.interrupt_shadow = true;
        self.interrupt_attention = false;
    }

    #[inline(always)]
    pub(crate) fn was_interruptible_before_instruction(&self) -> bool {
        !self.interrupt_attention && self.maskable_interrupts_enabled()
    }

    /// The x86 semantic caller chooses the commit point (after a successful
    /// faultable write for memory ALU operations). Chip status calculation is
    /// eager; Cpu remains the sole authoritative EFLAGS owner. No handler ABI
    /// or live hardware flag value crosses a Rust call.
    #[inline(always)]
    fn install_flags(&mut self, flags: LazyFlags) {
        #[cfg(not(all(feature = "r86chip-native", target_arch = "riscv32")))]
        {
            self.lazy_flags = flags;
        }
        #[cfg(all(feature = "r86chip-native", target_arch = "riscv32"))]
        {
            self.eflags = flags.chip_eflags(self.eflags | 2);
        }
    }

    #[inline(always)]
    pub fn set_logic_flags8(&mut self, result: u8) {
        self.install_flags(LazyFlags::new(LazyFlags::LOGIC, 8, 0, result as u32, 0));
    }

    #[inline(always)]
    pub fn set_logic_flags16(&mut self, result: u16) {
        self.install_flags(LazyFlags::new(LazyFlags::LOGIC, 16, 0, result as u32, 0));
    }

    #[inline(always)]
    pub fn set_logic_flags32(&mut self, result: u32) {
        self.install_flags(LazyFlags::new(LazyFlags::LOGIC, 32, 0, result as u32, 0));
    }

    #[inline(always)]
    pub fn set_add_flags8(&mut self, lhs: u8, _rhs: u8, result: u8) {
        self.install_flags(LazyFlags::new(
            LazyFlags::ADD,
            8,
            lhs as u32,
            result as u32,
            0,
        ));
    }

    #[inline(always)]
    pub fn set_add_flags16(&mut self, lhs: u16, _rhs: u16, result: u16) {
        self.install_flags(LazyFlags::new(
            LazyFlags::ADD,
            16,
            lhs as u32,
            result as u32,
            0,
        ));
    }

    #[inline(always)]
    pub fn set_add_flags32(&mut self, lhs: u32, _rhs: u32, result: u32) {
        self.install_flags(LazyFlags::new(
            LazyFlags::ADD,
            32,
            lhs as u32,
            result as u32,
            0,
        ));
    }

    #[inline(always)]
    pub fn set_adc_flags8(&mut self, lhs: u8, _rhs: u8, carry: u8, result: u8) {
        self.install_flags(LazyFlags::new(
            LazyFlags::ADC,
            8,
            lhs as u32,
            result as u32,
            carry as u32,
        ));
    }

    #[inline(always)]
    pub fn set_adc_flags16(&mut self, lhs: u16, _rhs: u16, carry: u16, result: u16) {
        self.install_flags(LazyFlags::new(
            LazyFlags::ADC,
            16,
            lhs as u32,
            result as u32,
            carry as u32,
        ));
    }

    #[inline(always)]
    pub fn set_adc_flags32(&mut self, lhs: u32, _rhs: u32, carry: u32, result: u32) {
        self.install_flags(LazyFlags::new(
            LazyFlags::ADC,
            32,
            lhs as u32,
            result as u32,
            carry as u32,
        ));
    }

    #[inline(always)]
    pub fn set_sub_flags8(&mut self, lhs: u8, _rhs: u8, result: u8) {
        self.install_flags(LazyFlags::new(
            LazyFlags::SUB,
            8,
            lhs as u32,
            result as u32,
            0,
        ));
    }

    #[inline(always)]
    pub fn set_sub_flags16(&mut self, lhs: u16, _rhs: u16, result: u16) {
        self.install_flags(LazyFlags::new(
            LazyFlags::SUB,
            16,
            lhs as u32,
            result as u32,
            0,
        ));
    }

    #[inline(always)]
    pub fn set_sub_flags32(&mut self, lhs: u32, _rhs: u32, result: u32) {
        self.install_flags(LazyFlags::new(
            LazyFlags::SUB,
            32,
            lhs as u32,
            result as u32,
            0,
        ));
    }

    #[inline(always)]
    pub fn set_sbb_flags8(&mut self, lhs: u8, _rhs: u8, carry: u8, result: u8) {
        self.install_flags(LazyFlags::new(
            LazyFlags::SBB,
            8,
            lhs as u32,
            result as u32,
            carry as u32,
        ));
    }

    #[inline(always)]
    pub fn set_sbb_flags16(&mut self, lhs: u16, _rhs: u16, carry: u16, result: u16) {
        self.install_flags(LazyFlags::new(
            LazyFlags::SBB,
            16,
            lhs as u32,
            result as u32,
            carry as u32,
        ));
    }

    #[inline(always)]
    pub fn set_sbb_flags32(&mut self, lhs: u32, _rhs: u32, carry: u32, result: u32) {
        self.install_flags(LazyFlags::new(
            LazyFlags::SBB,
            32,
            lhs as u32,
            result as u32,
            carry as u32,
        ));
    }

    #[inline(always)]
    pub fn set_shift_flags8(&mut self, result: u8, carry: bool, overflow: bool) {
        self.install_flags(LazyFlags::new(
            LazyFlags::SHIFT,
            8,
            overflow as u32,
            result as u32,
            carry as u32,
        ));
    }

    #[inline(always)]
    pub fn set_shift_flags16(&mut self, result: u16, carry: bool, overflow: bool) {
        self.install_flags(LazyFlags::new(
            LazyFlags::SHIFT,
            16,
            overflow as u32,
            result as u32,
            carry as u32,
        ));
    }

    #[inline(always)]
    pub fn set_shift_flags32(&mut self, result: u32, carry: bool, overflow: bool) {
        self.install_flags(LazyFlags::new(
            LazyFlags::SHIFT,
            32,
            overflow as u32,
            result as u32,
            carry as u32,
        ));
    }

    #[inline(always)]
    pub fn set_inc_flags8(&mut self, lhs: u8, result: u8) {
        // Only carry survives; the next arithmetic record owns other status bits.
        self.eflags = (self.eflags & !EFLAGS_CF) | u32::from(self.flag(EFLAGS_CF));
        self.install_flags(LazyFlags::new(
            LazyFlags::ADD | LazyFlags::KEEP_CF,
            8,
            lhs as u32,
            result as u32,
            0,
        ));
    }

    #[inline(always)]
    pub fn set_inc_flags16(&mut self, lhs: u16, result: u16) {
        // Only carry survives; the next arithmetic record owns other status bits.
        self.eflags = (self.eflags & !EFLAGS_CF) | u32::from(self.flag(EFLAGS_CF));
        self.install_flags(LazyFlags::new(
            LazyFlags::ADD | LazyFlags::KEEP_CF,
            16,
            lhs as u32,
            result as u32,
            0,
        ));
    }

    #[inline(always)]
    pub fn set_inc_flags32(&mut self, lhs: u32, result: u32) {
        // Only carry survives; the next arithmetic record owns other status bits.
        self.eflags = (self.eflags & !EFLAGS_CF) | u32::from(self.flag(EFLAGS_CF));
        self.install_flags(LazyFlags::new(
            LazyFlags::ADD | LazyFlags::KEEP_CF,
            32,
            lhs as u32,
            result as u32,
            0,
        ));
    }

    #[inline(always)]
    pub fn set_dec_flags8(&mut self, lhs: u8, result: u8) {
        // Only carry survives; the next arithmetic record owns other status bits.
        self.eflags = (self.eflags & !EFLAGS_CF) | u32::from(self.flag(EFLAGS_CF));
        self.install_flags(LazyFlags::new(
            LazyFlags::SUB | LazyFlags::KEEP_CF,
            8,
            lhs as u32,
            result as u32,
            0,
        ));
    }

    #[inline(always)]
    pub fn set_dec_flags16(&mut self, lhs: u16, result: u16) {
        // Only carry survives; the next arithmetic record owns other status bits.
        self.eflags = (self.eflags & !EFLAGS_CF) | u32::from(self.flag(EFLAGS_CF));
        self.install_flags(LazyFlags::new(
            LazyFlags::SUB | LazyFlags::KEEP_CF,
            16,
            lhs as u32,
            result as u32,
            0,
        ));
    }

    #[inline(always)]
    pub fn set_dec_flags32(&mut self, lhs: u32, result: u32) {
        // Only carry survives; the next arithmetic record owns other status bits.
        self.eflags = (self.eflags & !EFLAGS_CF) | u32::from(self.flag(EFLAGS_CF));
        self.install_flags(LazyFlags::new(
            LazyFlags::SUB | LazyFlags::KEEP_CF,
            32,
            lhs as u32,
            result as u32,
            0,
        ));
    }
}

impl Default for Cpu {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn flat_data_segment(cache: SegmentCache, write: bool) -> bool {
    let access = (cache.flags & 0x00ff) as u8;
    // A full-limit expand-down segment is empty, not flat RAM, so it stays on
    // the checked segment path (v86 cpu.rs get_seg models no expand-down bounds).
    let data = (access & 0x1c) == 0x10;
    let writable = (access & 0x02) != 0;
    cache.base == 0
        && cache.limit == u32::MAX
        && (cache.selector & !3) != 0
        && data
        && (!write || writable)
}

#[inline(always)]
#[cfg(not(all(feature = "r86chip-native", target_arch = "riscv32")))]
fn parity8(value: u8) -> bool {
    let folded = value ^ (value >> 4);
    ((0x9669u16 >> (folded & 0x0f)) & 1) != 0
}

#[inline]
#[cfg(not(all(feature = "r86chip-native", target_arch = "riscv32")))]
fn flag_bit(mask: u32, set: bool) -> u32 {
    mask & 0u32.wrapping_sub(set as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "r86chip")]
    #[test]
    fn chip_tsc_observations_survive_partial_batches_writes_and_counter_wrap() {
        let mut cpu = Cpu::new();
        cpu.set_tsc(u64::MAX - 1);
        cpu.begin_tsc_batch(20);
        assert_eq!(cpu.tsc(), u64::MAX - 1); // Planning does not charge time.
        cpu.set_tsc_remaining(19);
        assert_eq!(cpu.tsc(), u64::MAX);
        cpu.set_tsc_remaining(18);
        assert_eq!(cpu.tsc(), 0);

        // A write during execution is immediately visible. Neither the next
        // attempt nor an early exit may restore the old counter's high bits.
        cpu.set_tsc(0x1234_5678_ffff_ffff);
        assert_eq!(cpu.tsc(), 0x1234_5678_ffff_ffff);
        cpu.set_tsc_remaining(17);
        assert_eq!(cpu.tsc(), 0x1234_5679_0000_0000);
        let snapshot = cpu.clone();
        cpu.advance_tsc(19); // E.g. a halted fast-forward between batches.
        assert_eq!(snapshot.tsc(), 0x1234_5679_0000_0000);
        cpu.begin_tsc_batch(8); // Old batch exited with 17 unexecuted attempts.
        assert_eq!(cpu.tsc(), 0x1234_5679_0000_0013);
        cpu.set_tsc_remaining(7);
        assert_eq!(cpu.tsc(), 0x1234_5679_0000_0014);

        // Reset and a subsequent fresh batch must not include stale progress.
        cpu.reset_bios();
        assert_eq!(cpu.tsc(), 0);
        cpu.begin_tsc_batch(1);
        cpu.set_tsc_remaining(0);
        assert_eq!(cpu.tsc(), 1);
    }

    #[cfg(feature = "r86chip")]
    #[test]
    fn chip_tsc_reanchors_at_u32_chunk_boundary_without_losing_high_bits() {
        // Exercise the >u32::MAX budget boundary without executing billions of
        // guest NOPs. Only the public timestamp observations are asserted.
        let mut cpu = Cpu::new();
        cpu.set_tsc(u64::MAX - 2);
        cpu.begin_tsc_batch(u32::MAX);
        assert_eq!(cpu.tsc(), u64::MAX - 2);
        cpu.set_tsc_remaining(0);
        assert_eq!(cpu.tsc(), u64::from(u32::MAX) - 3);
        cpu.begin_tsc_batch(17);
        assert_eq!(cpu.tsc(), u64::from(u32::MAX) - 3);
        cpu.set_tsc_remaining(0);
        assert_eq!(cpu.tsc(), u64::from(u32::MAX) + 14);
    }

    fn flag(cpu: &Cpu, mask: u32) -> bool {
        cpu.flag(mask)
    }

    fn status_flags() -> u32 {
        EFLAGS_CF | EFLAGS_PF | EFLAGS_AF | EFLAGS_ZF | EFLAGS_SF | EFLAGS_OF
    }

    #[test]
    fn fetch_cache_range_invalidation_covers_middle_pages() {
        let mut cpu = Cpu::new();

        cpu.fill_fetch_cache(LinearAddr(0x0000), PhysAddr(0x0000));
        cpu.fill_fetch_cache(LinearAddr(0x1000), PhysAddr(0x1000));
        cpu.fill_fetch_cache(LinearAddr(0x2000), PhysAddr(0x2000));
        cpu.fill_fetch_cache(LinearAddr(0x3000), PhysAddr(0x3000));

        cpu.invalidate_fetch_cache_for_phys_range(PhysAddr(0x1800), 0x1800);

        assert_eq!(
            cpu.fetch_cache_phys(LinearAddr(0x0000)),
            Some(PhysAddr(0x0000))
        );
        assert_eq!(cpu.fetch_cache_phys(LinearAddr(0x1000)), None);
        assert_eq!(cpu.fetch_cache_phys(LinearAddr(0x2000)), None);
        assert_eq!(
            cpu.fetch_cache_phys(LinearAddr(0x3000)),
            Some(PhysAddr(0x3000))
        );
    }

    #[test]
    fn fetch_cache_keeps_opaque_backing_token_alignment() {
        let mut cpu = Cpu::new();
        let linear = LinearAddr(0x1234);
        let phys = PhysAddr(0x5234);
        let backing = BackingToken(0x1000_0077);

        cpu.fill_fetch_cache_with_backing(linear, phys, backing, false);

        let hit = cpu.fetch_cache_lookup(LinearAddr(0x13ab)).unwrap();
        assert_eq!(hit.phys, PhysAddr(0x53ab));
        assert_eq!(hit.backing, BackingToken(0x1000_01ee));
    }

    #[test]
    fn fetch_cache_invalidation_rejects_old_mapping() {
        let mut cache = FetchCache::new();
        let linear = LinearAddr(0x1234);
        cache.fill(linear, PhysAddr(0x5234), BackingToken(0x1000_0077), false);
        assert_eq!(cache.lookup(linear).unwrap().phys, PhysAddr(0x5234));

        cache.invalidate();

        assert_eq!(cache.lookup(linear), None);
    }

    #[test]
    fn fetch_cache_replacement_preserves_invalidation_of_current_mapping() {
        let mut cache = FetchCache::new();
        let first_linear = LinearAddr(0x1234);
        let colliding_linear = LinearAddr(0x1234 + ((FETCH_CACHE_SLOTS as u32) << 12));
        let first_phys = PhysAddr(0x5234);
        let second_phys = PhysAddr(0x9234);

        cache.fill(first_linear, first_phys, BackingToken(0x10_0234), false);
        cache.fill(
            colliding_linear,
            second_phys,
            BackingToken(0x20_0234),
            false,
        );

        assert_eq!(cache.lookup(first_linear), None);
        assert!(cache.lookup(colliding_linear).is_some());
        assert!(!cache.invalidate_for_phys_range(PhysAddr(0x5000), 1));
        assert!(cache.lookup(colliding_linear).is_some());
        assert!(cache.invalidate_for_phys_range(PhysAddr(0x9000), 1));
        assert_eq!(cache.lookup(colliding_linear), None);
    }

    #[test]
    fn vm86_flag_change_invalidates_translation_context() {
        let mut cpu = Cpu::new();
        let linear = LinearAddr(0x1234);
        cpu.fill_fetch_cache(linear, PhysAddr(0x5234));
        cpu.fill_cached_read_translation(linear, BackingToken(0x9234), false);

        cpu.replace_eflags(cpu.eflags | CACHE_CONTEXT_EFLAGS_VM);

        assert_eq!(cpu.fetch_cache_lookup(linear), None);
        assert_eq!(cpu.cached_read_translation(linear, false), None);
    }

    #[test]
    fn fetch_cursor_reuses_non_consuming_window_for_loops() {
        let mut cursor = FetchCursor::new();
        cursor.fill_after(0x1200, BackingToken(0x8000), 1, 0x100);

        assert_eq!(cursor.take(0x1201, 4), Some(BackingToken(0x8001)));
        assert_eq!(cursor.take(0x1200, 2), Some(BackingToken(0x8000)));
        assert_eq!(cursor.take(0x12ff, 1), Some(BackingToken(0x80ff)));
        assert_eq!(cursor.take(0x12ff, 2), None);
        assert_eq!(cursor.take(0x11ff, 1), None);
        assert_eq!(cursor.take_u8(0x1200), Some(BackingToken(0x8000)));
        assert_eq!(cursor.take_u8(0x11ff), None);
        assert_eq!(cursor.take_u16(0x12fe), Some(BackingToken(0x80fe)));
        assert_eq!(cursor.take_u16(0x12ff), None);
        assert_eq!(cursor.take_u32(0x12fc), Some(BackingToken(0x80fc)));
        assert_eq!(cursor.take_u32(0x12fd), None);

        cursor.fill_after(0xffff_fffe, BackingToken(0x9000), 1, 2);
        assert_eq!(cursor.take(0xffff_ffff, 1), Some(BackingToken(0x9001)));
        assert_eq!(cursor.take(0, 1), None);
        assert_eq!(cursor.take_u8(0), None);
        assert_eq!(cursor.take_u16(0), None);
        assert_eq!(cursor.take_u32(0), None);
    }

    #[test]
    fn protected_flat_segments_allow_data_access_and_32bit_stack() {
        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(0, 0);

        assert!(cpu.flat_segment_access(SegmentReg::Ds, false));
        assert!(cpu.flat_segment_access(SegmentReg::Ds, true));
        assert!(!cpu.flat_segment_access(SegmentReg::Cs, false));
        assert!(cpu.flat_32bit_stack());
    }

    #[test]
    fn flat_segment_masks_track_read_only_and_nonflat_updates() {
        let mut cpu = Cpu::new();
        cpu.set_seg_cache(SegmentReg::Ds, SegmentCache::flat(0x18, 0xc090));
        assert!(cpu.flat_segment_access(SegmentReg::Ds, false));
        assert!(!cpu.flat_segment_access(SegmentReg::Ds, true));

        cpu.set_seg_cache(SegmentReg::Ds, SegmentCache::new(0x18, 1, u32::MAX, 0xc092));
        assert!(!cpu.flat_segment_access(SegmentReg::Ds, false));
        assert!(!cpu.flat_segment_access(SegmentReg::Ds, true));
    }

    #[test]
    fn flat_32bit_stack_metadata_tracks_stack_cache_updates() {
        let mut cpu = Cpu::new();
        cpu.set_seg_cache(SegmentReg::Ss, SegmentCache::flat(0x18, 0xc092));
        assert!(cpu.flat_32bit_stack());

        cpu.set_seg_cache(SegmentReg::Ss, SegmentCache::flat(0x18, 0x0092));
        assert!(!cpu.flat_32bit_stack());

        cpu.set_seg_cache(SegmentReg::Ss, SegmentCache::new(0x18, 1, u32::MAX, 0xc092));
        assert!(!cpu.flat_32bit_stack());
    }

    #[test]
    fn linear_translation_cache_invalidation_rejects_old_mapping() {
        let mut cache = LinearTranslationCache::new();
        let linear = LinearAddr(0x1234_5678);
        let phys = PhysAddr(0x000a_b678);

        let backing = BackingToken(phys.0);
        cache.fill(linear, backing, false, false);
        assert_eq!(cache.lookup(linear, false, false), Some(backing));

        cache.invalidate();

        assert_eq!(cache.lookup(linear, false, false), None);
    }

    #[test]
    fn linear_translation_write_permissions_follow_entry_identity() {
        let mut cache = LinearTranslationCache::new();
        let linear = LinearAddr(0x3456);
        let backing = BackingToken(0x7456);

        cache.fill(linear, backing, false, false);
        assert_eq!(cache.lookup(linear, false, true), None);

        cache.fill(linear, backing, false, true);
        assert_eq!(cache.lookup(linear, false, true), Some(backing));
        cache.fill(linear, backing, false, false);
        assert_eq!(cache.lookup(linear, false, true), Some(backing));

        cache.invalidate();
        cache.fill(linear, backing, false, false);
        assert_eq!(cache.lookup(linear, false, true), None);
    }

    #[test]
    fn invlpg_keeps_unrelated_read_write_and_execute_translations() {
        let mut cpu = Cpu::new();
        for page in [0x1000, 0x402000] {
            let address = LinearAddr(page);
            cpu.fill_cached_write_translation(address, BackingToken(u64::from(page)), false);
            cpu.fill_fetch_cache(address, PhysAddr(u64::from(page)));
        }
        cpu.invalidate_translation_page(LinearAddr(0x1fff));
        assert_eq!(
            cpu.cached_current_read_translation(LinearAddr(0x1000)),
            None
        );
        assert_eq!(
            cpu.cached_current_write_translation(LinearAddr(0x1000)),
            None
        );
        assert!(cpu.fetch_cache_lookup(LinearAddr(0x1000)).is_none());
        assert_eq!(
            cpu.cached_current_write_translation(LinearAddr(0x402123)),
            Some(BackingToken(0x402123))
        );
        assert_eq!(
            cpu.fetch_cache_lookup(LinearAddr(0x402123)).unwrap().phys,
            PhysAddr(0x402123)
        );
        // Publishing a later read-only mapping in the retired slot cannot
        // inherit the write permission of the old cached mapping.
        cpu.fill_cached_read_translation(LinearAddr(0x1000), BackingToken(0x7000), false);
        assert_eq!(
            cpu.cached_current_write_translation(LinearAddr(0x1000)),
            None
        );
    }

    #[test]
    fn invlpg_expires_all_cached_pieces_of_a_large_translation() {
        let mut cpu = Cpu::new();
        for page in [0x1000, 0x202000, 0x3ff000, 0x403000] {
            let address = LinearAddr(page);
            cpu.fill_cached_write_translation(address, BackingToken(u64::from(page)), false);
            cpu.fill_fetch_cache(address, PhysAddr(u64::from(page)));
        }
        cpu.note_large_translation(LinearAddr(0x202000));
        // This particular 4 KiB piece was never cached. It still names the
        // same large translation as all three cached pieces below 4 MiB.
        cpu.invalidate_translation_page(LinearAddr(0x204000));
        for page in [0x1000, 0x202000, 0x3ff000] {
            assert_eq!(cpu.cached_current_write_translation(LinearAddr(page)), None);
            assert!(cpu.fetch_cache_lookup(LinearAddr(page)).is_none());
        }
        assert_eq!(
            cpu.cached_current_write_translation(LinearAddr(0x403000)),
            Some(BackingToken(0x403000))
        );
        assert!(cpu.fetch_cache_lookup(LinearAddr(0x403000)).is_some());
    }

    #[cfg(feature = "jit")]
    #[test]
    fn multiple_unconsumed_invlpgs_expire_native_context_with_empty_fetch_cache() {
        let mut cpu = Cpu::new();
        let epoch = cpu.jit_mapping_epoch;
        cpu.invalidate_translation_page(LinearAddr(0x1234));
        cpu.invalidate_translation_page(LinearAddr(0x1abc));
        assert_eq!(cpu.jit_mapping_epoch, epoch);
        assert_eq!(cpu.jit_invalidated_range, Some((0x1000, 0xffff_f000)));
        cpu.invalidate_translation_page(LinearAddr(0x4567));
        assert_ne!(cpu.jit_mapping_epoch, epoch);
        assert_eq!(cpu.jit_invalidated_range, None);
    }

    #[cfg(all(feature = "jit", target_os = "linux", target_arch = "aarch64"))]
    #[test]
    fn jit_code_promotion_revokes_physical_aliases_only() {
        let mut cache = LinearTranslationCache::new();
        // RAM need not begin on a host page boundary. Both virtual pages alias
        // the same backing page, while the third holds unrelated writable data.
        let backing = 0x1234_5010;
        for (linear, page) in [
            (0x1000, backing),
            (0x4000, backing),
            (0x2000, backing + 4096),
        ] {
            cache.fill(LinearAddr(linear), BackingToken(page), false, true);
        }
        cache.revoke_writable_aliases(backing);
        for linear in [0x1000, 0x4000] {
            assert_eq!(cache.lookup_current_write(LinearAddr(linear)), None);
            assert_eq!(
                cache.lookup_current_read(LinearAddr(linear)),
                Some(BackingToken(backing))
            );
            assert_eq!(
                cache.entries[LinearTranslationCache::slot(LinearAddr(linear))].write_tag,
                0
            );
        }
        assert_eq!(
            cache.lookup_current_write(LinearAddr(0x2000)),
            Some(BackingToken(backing + 4096))
        );
        assert_eq!(
            cache.entries[LinearTranslationCache::slot(LinearAddr(0x2000))].write_tag,
            cache.entries[LinearTranslationCache::slot(LinearAddr(0x2000))].tag
        );
    }

    #[test]
    fn linear_translation_generation_wrap_clears_old_entries() {
        let mut cache = LinearTranslationCache::new();
        let linear = LinearAddr(0x2000);
        let phys = PhysAddr(0x5000);
        cache.state = LINEAR_TRANSLATION_GENERATION_MASK;
        cache.fill(linear, BackingToken(phys.0), false, false);

        cache.invalidate();

        assert_eq!(cache.lookup(linear, false, false), None);
        cache.fill(linear, BackingToken(0x9000), false, false);
        assert_eq!(
            cache.lookup(linear, false, false),
            Some(BackingToken(0x9000))
        );
    }

    #[cfg(feature = "jit")]
    #[test]
    fn repeated_context_changes_never_revive_read_or_write_permissions() {
        for user in [false, true] {
            let mut cache = LinearTranslationCache::new();
            cache.set_current_user(user);
            let linear = LinearAddr(0xffff_f123);
            let old = BackingToken(0x8123);
            let new = BackingToken(0x9123);
            cache.fill(linear, old, user, true);
            for _ in 0..4096 {
                cache.invalidate_non_global();
                assert_eq!(cache.lookup_current_read(linear), None);
                assert_eq!(cache.lookup_current_write(linear), None);
            }
            cache.fill(linear, new, user, false);
            assert_eq!(cache.lookup_current_read(linear), Some(new));
            assert_eq!(cache.lookup_current_write(linear), None);
            cache.set_current_user(!user);
            assert_eq!(cache.lookup_current_read(linear), None);
            assert_eq!(cache.lookup_current_write(linear), None);
        }
    }

    #[test]
    fn current_read_tag_tracks_code_privilege_without_stale_hits() {
        let mut cache = LinearTranslationCache::new();
        let linear = LinearAddr(0x2345);
        let supervisor = BackingToken(0x7345);
        let user = BackingToken(0x9345);

        cache.fill(linear, supervisor, false, false);
        assert_eq!(cache.lookup_current_read(linear), Some(supervisor));

        cache.set_current_user(true);
        assert_eq!(cache.lookup_current_read(linear), None);
        cache.set_current_user(false);
        assert_eq!(cache.lookup_current_read(linear), Some(supervisor));
        cache.set_current_user(true);
        cache.fill(linear, user, true, false);
        assert_eq!(cache.lookup_current_read(linear), Some(user));

        cache.set_current_user(false);
        assert_eq!(cache.lookup_current_read(linear), None);
    }

    #[test]
    fn code_segment_reload_keeps_linear_mappings_but_rechecks_fetch_window() {
        let mut cpu = Cpu::new();
        let code = SegmentCache::flat(0x18, SEGMENT_FLAG_DEFAULT_BIG);
        cpu.set_seg_cache(SegmentReg::Cs, code);
        let linear = LinearAddr(0x3000);
        let phys = PhysAddr(0x7000);
        let backing = BackingToken(phys.0);
        cpu.fill_fetch_cache(linear, phys);
        cpu.fill_cached_read_translation(linear, backing, false);
        assert_eq!(cpu.fetch_cache_phys(linear), Some(phys));
        assert_eq!(cpu.cached_read_translation(linear, false), Some(backing));

        cpu.fill_fetch_cursor_after(0x3000, backing, 1, 16);
        cpu.set_seg_cache(SegmentReg::Cs, code);

        assert_eq!(cpu.fetch_cursor_backing_u8(0x3001), None);
        assert_eq!(cpu.fetch_cache_phys(linear), Some(phys));
        assert_eq!(cpu.cached_read_translation(linear, false), Some(backing));

        cpu.set_seg_cache(
            SegmentReg::Cs,
            SegmentCache {
                base: 0x100,
                ..code
            },
        );

        assert_eq!(cpu.fetch_cache_phys(linear), None);
        assert_eq!(cpu.cached_read_translation(linear, false), Some(backing));
    }

    #[test]
    fn restoring_user_code_segment_cannot_use_supervisor_translations() {
        let mut cpu = Cpu::new();
        cpu.cr0 |= CR0_PE; // CS.RPL is the CPL only in protected mode.
        let linear = LinearAddr(0x3000);
        let phys = PhysAddr(0x7000);
        cpu.fill_fetch_cache(linear, phys);
        cpu.fill_cached_write_translation(linear, BackingToken(phys.0), false);
        cpu.set_seg_cache(
            SegmentReg::Cs,
            SegmentCache {
                selector: 3,
                ..cpu.seg(SegmentReg::Cs)
            },
        );

        assert_eq!(cpu.fetch_cache_phys(linear), None);
        assert_eq!(cpu.cached_current_read_translation(linear), None);
        assert_eq!(cpu.cached_current_write_translation(linear), None);
        // Explicit supervisor accesses (e.g. descriptor loads) keep their
        // separately tagged permission; ordinary CPL3 accesses cannot use it.
        assert_eq!(
            cpu.cached_read_translation(linear, false),
            Some(BackingToken(phys.0))
        );
        cpu.fill_cached_read_translation(linear, BackingToken(phys.0), true);
        assert_eq!(
            cpu.cached_current_read_translation(linear),
            Some(BackingToken(phys.0))
        );
        assert_eq!(cpu.cached_current_write_translation(linear), None);
    }

    #[test]
    fn arithmetic_flags_agree_before_and_after_materialization() {
        let mut cpu = Cpu::new();
        cpu.eflags = EFLAGS_OF | EFLAGS_DF | 0x0000_0002;

        cpu.set_add_flags8(0xff, 1, 0);

        assert!(cpu.flag(EFLAGS_CF));
        assert!(cpu.flag(EFLAGS_AF));
        assert!(cpu.flag(EFLAGS_ZF));
        assert!(cpu.flag(EFLAGS_PF));
        assert!(!cpu.flag(EFLAGS_SF));
        assert!(!cpu.flag(EFLAGS_OF));
        let effective = cpu.effective_eflags();

        cpu.materialize_flags();

        assert_eq!(cpu.eflags, effective);
        assert_ne!(cpu.eflags & EFLAGS_DF, 0);
    }

    #[test]
    fn byte_adc_and_sbb_lazy_flags_match_exhaustive_reference() {
        let mut cpu = Cpu::new();
        for lhs in 0u16..=u8::MAX as u16 {
            for rhs in 0u16..=u8::MAX as u16 {
                for carry in 0u16..=1 {
                    let adc_wide = lhs + rhs + carry;
                    let adc = adc_wide as u8;
                    cpu.set_adc_flags8(lhs as u8, rhs as u8, carry as u8, adc);
                    let adc_expected = flag_bit(EFLAGS_CF, adc_wide > u8::MAX as u16)
                        | flag_bit(EFLAGS_PF, parity8(adc))
                        | flag_bit(EFLAGS_AF, (lhs & 0xf) + (rhs & 0xf) + carry > 0xf)
                        | flag_bit(EFLAGS_ZF, adc == 0)
                        | flag_bit(EFLAGS_SF, adc & 0x80 != 0)
                        | flag_bit(EFLAGS_OF, (!(lhs ^ rhs) & (lhs ^ adc as u16) & 0x80) != 0);
                    assert_eq!(cpu.effective_eflags() & EFLAGS_STATUS_MASK, adc_expected);

                    let sbb = (lhs as u8)
                        .wrapping_sub(rhs as u8)
                        .wrapping_sub(carry as u8);
                    cpu.set_sbb_flags8(lhs as u8, rhs as u8, carry as u8, sbb);
                    let sbb_expected = flag_bit(EFLAGS_CF, lhs < rhs + carry)
                        | flag_bit(EFLAGS_PF, parity8(sbb))
                        | flag_bit(EFLAGS_AF, (lhs & 0xf) < (rhs & 0xf) + carry)
                        | flag_bit(EFLAGS_ZF, sbb == 0)
                        | flag_bit(EFLAGS_SF, sbb & 0x80 != 0)
                        | flag_bit(EFLAGS_OF, ((lhs ^ rhs) & (lhs ^ sbb as u16) & 0x80) != 0);
                    assert_eq!(cpu.effective_eflags() & EFLAGS_STATUS_MASK, sbb_expected);
                }
            }
        }
    }

    #[test]
    fn wide_arithmetic_flags_match_signed_and_unsigned_boundaries() {
        let mut cpu = Cpu::new();
        for bits in [16, 32] {
            let modulus = 1i64 << bits;
            let sign = modulus / 2;
            let mask = modulus - 1;
            let values = [0, 1, 15, 16, sign - 1, sign, mask - 1, mask];
            let signed = |v| if v < sign { v } else { v - modulus };
            for lhs in values {
                for rhs in values {
                    for carry in [0, 1] {
                        for subtract in [false, true] {
                            let (wide, signed_wide, auxiliary) = if subtract {
                                (
                                    lhs - rhs - carry,
                                    signed(lhs) - signed(rhs) - carry,
                                    (lhs & 15) < (rhs & 15) + carry,
                                )
                            } else {
                                (
                                    lhs + rhs + carry,
                                    signed(lhs) + signed(rhs) + carry,
                                    (lhs & 15) + (rhs & 15) + carry >= 16,
                                )
                            };
                            let result = (wide & mask) as u32;
                            match (bits, subtract) {
                                (16, false) => cpu.set_adc_flags16(
                                    lhs as u16,
                                    rhs as u16,
                                    carry as u16,
                                    result as u16,
                                ),
                                (16, true) => cpu.set_sbb_flags16(
                                    lhs as u16,
                                    rhs as u16,
                                    carry as u16,
                                    result as u16,
                                ),
                                (32, false) => cpu.set_adc_flags32(
                                    lhs as u32,
                                    rhs as u32,
                                    carry as u32,
                                    result,
                                ),
                                (32, true) => cpu.set_sbb_flags32(
                                    lhs as u32,
                                    rhs as u32,
                                    carry as u32,
                                    result,
                                ),
                                _ => unreachable!(),
                            }
                            let expected = flag_bit(EFLAGS_CF, wide < 0 || wide >= modulus)
                                | flag_bit(EFLAGS_PF, (result as u8).count_ones() & 1 == 0)
                                | flag_bit(EFLAGS_AF, auxiliary)
                                | flag_bit(EFLAGS_ZF, result == 0)
                                | flag_bit(EFLAGS_SF, i64::from(result) >= sign)
                                | flag_bit(EFLAGS_OF, signed_wide < -sign || signed_wide >= sign);
                            assert_eq!(cpu.effective_eflags() & EFLAGS_STATUS_MASK, expected);
                            for flag in [
                                EFLAGS_CF, EFLAGS_PF, EFLAGS_AF, EFLAGS_ZF, EFLAGS_SF, EFLAGS_OF,
                            ] {
                                assert_eq!(cpu.flag(flag), expected & flag != 0);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn shift_lazy_flags_preserve_explicit_carry_and_overflow() {
        let mut cpu = Cpu::new();
        for result in 0u16..=u8::MAX as u16 {
            for carry in [false, true] {
                for overflow in [false, true] {
                    cpu.set_shift_flags8(result as u8, carry, overflow);
                    let expected = flag_bit(EFLAGS_CF, carry)
                        | flag_bit(EFLAGS_PF, parity8(result as u8))
                        | flag_bit(EFLAGS_ZF, result == 0)
                        | flag_bit(EFLAGS_SF, result & 0x80 != 0)
                        | flag_bit(EFLAGS_OF, overflow);
                    assert_eq!(cpu.effective_eflags() & EFLAGS_STATUS_MASK, expected);
                }
            }
        }
    }

    #[test]
    fn replacing_eflags_discards_pending_arithmetic() {
        let mut cpu = Cpu::new();
        cpu.set_sub_flags32(0, 1, u32::MAX);

        cpu.replace_eflags(EFLAGS_DF | EFLAGS_ZF);

        assert_eq!(cpu.effective_eflags(), EFLAGS_DF | EFLAGS_ZF | 0x0000_0002);
    }

    #[test]
    fn inc_and_dec_preserve_carry_and_update_other_status_flags() {
        let mut cpu = Cpu::new();
        cpu.set_add_flags8(0xff, 1, 0);
        cpu.set_inc_flags8(0x7f, 0x80);

        assert!(cpu.flag(EFLAGS_CF));
        assert!(cpu.flag(EFLAGS_AF));
        assert!(cpu.flag(EFLAGS_SF));
        assert!(cpu.flag(EFLAGS_OF));
        assert!(!cpu.flag(EFLAGS_ZF));

        cpu.set_logic_flags32(1);
        cpu.set_dec_flags32(0x8000_0000, 0x7fff_ffff);

        assert!(!cpu.flag(EFLAGS_CF));
        assert!(cpu.flag(EFLAGS_AF));
        assert!(!cpu.flag(EFLAGS_SF));
        assert!(cpu.flag(EFLAGS_OF));
        assert!(!cpu.flag(EFLAGS_ZF));
    }

    #[test]
    fn logic_flag_helpers_clear_auxiliary_like_reference_logic_ops() {
        let mut cpu = Cpu::new();
        cpu.eflags = status_flags() | 0x0000_0002;

        cpu.set_logic_flags8(0x80);
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_OF));
        assert!(!flag(&cpu, EFLAGS_AF));
        assert!(!flag(&cpu, EFLAGS_ZF));
        assert!(flag(&cpu, EFLAGS_SF));
        assert!(!flag(&cpu, EFLAGS_PF));
        assert!(flag(&cpu, 0x0000_0002));

        cpu.eflags = status_flags() | 0x0000_0002;
        cpu.set_logic_flags16(0x0000);
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_OF));
        assert!(!flag(&cpu, EFLAGS_AF));
        assert!(flag(&cpu, EFLAGS_ZF));
        assert!(!flag(&cpu, EFLAGS_SF));
        assert!(flag(&cpu, EFLAGS_PF));
        assert!(flag(&cpu, 0x0000_0002));

        cpu.eflags = status_flags() | 0x0000_0002;
        cpu.set_logic_flags32(0x8000_0001);
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_OF));
        assert!(!flag(&cpu, EFLAGS_AF));
        assert!(!flag(&cpu, EFLAGS_ZF));
        assert!(flag(&cpu, EFLAGS_SF));
        assert!(!flag(&cpu, EFLAGS_PF));
        assert!(flag(&cpu, 0x0000_0002));
    }

    #[test]
    fn add_flag_helpers_set_auxiliary_carry_for_nibble_carry() {
        let mut cpu = Cpu::new();

        cpu.set_add_flags8(0x0f, 0x01, 0x10);
        assert!(flag(&cpu, EFLAGS_AF));
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_ZF));
        assert!(!flag(&cpu, EFLAGS_SF));
        assert!(!flag(&cpu, EFLAGS_OF));

        cpu.set_add_flags16(0x00ff, 0x0001, 0x0100);
        assert!(flag(&cpu, EFLAGS_AF));
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_ZF));
        assert!(!flag(&cpu, EFLAGS_SF));
        assert!(!flag(&cpu, EFLAGS_OF));

        cpu.set_add_flags32(0x7fff_ffff, 0x0000_0001, 0x8000_0000);
        assert!(flag(&cpu, EFLAGS_AF));
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_ZF));
        assert!(flag(&cpu, EFLAGS_SF));
        assert!(flag(&cpu, EFLAGS_OF));
    }

    #[test]
    fn add_flag_helpers_clear_auxiliary_carry_without_low_nibble_carry() {
        let mut cpu = Cpu::new();
        cpu.eflags = EFLAGS_AF | EFLAGS_CF | EFLAGS_OF | 0x0000_0002;

        cpu.set_add_flags8(0x01, 0x02, 0x03);
        assert!(!flag(&cpu, EFLAGS_AF));
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_OF));

        cpu.eflags = EFLAGS_AF | EFLAGS_CF | EFLAGS_OF | 0x0000_0002;
        cpu.set_add_flags16(0x0101, 0x0202, 0x0303);
        assert!(!flag(&cpu, EFLAGS_AF));
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_OF));

        cpu.eflags = EFLAGS_AF | EFLAGS_CF | EFLAGS_OF | 0x0000_0002;
        cpu.set_add_flags32(0x0101_0101, 0x0202_0202, 0x0303_0303);
        assert!(!flag(&cpu, EFLAGS_AF));
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_OF));
    }

    #[test]
    fn sub_flag_helpers_set_auxiliary_carry_for_low_nibble_borrow() {
        let mut cpu = Cpu::new();

        cpu.set_sub_flags8(0x10, 0x01, 0x0f);
        assert!(flag(&cpu, EFLAGS_AF));
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_ZF));
        assert!(!flag(&cpu, EFLAGS_SF));
        assert!(!flag(&cpu, EFLAGS_OF));

        cpu.set_sub_flags16(0x0100, 0x0001, 0x00ff);
        assert!(flag(&cpu, EFLAGS_AF));
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_ZF));
        assert!(!flag(&cpu, EFLAGS_SF));
        assert!(!flag(&cpu, EFLAGS_OF));

        cpu.set_sub_flags32(0x8000_0000, 0x0000_0001, 0x7fff_ffff);
        assert!(flag(&cpu, EFLAGS_AF));
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_ZF));
        assert!(!flag(&cpu, EFLAGS_SF));
        assert!(flag(&cpu, EFLAGS_OF));
    }

    #[test]
    fn sub_flag_helpers_clear_auxiliary_carry_without_low_nibble_borrow() {
        let mut cpu = Cpu::new();
        cpu.eflags = EFLAGS_AF | EFLAGS_CF | EFLAGS_OF | 0x0000_0002;

        cpu.set_sub_flags8(0x03, 0x01, 0x02);
        assert!(!flag(&cpu, EFLAGS_AF));
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_OF));

        cpu.eflags = EFLAGS_AF | EFLAGS_CF | EFLAGS_OF | 0x0000_0002;
        cpu.set_sub_flags16(0x0303, 0x0101, 0x0202);
        assert!(!flag(&cpu, EFLAGS_AF));
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_OF));

        cpu.eflags = EFLAGS_AF | EFLAGS_CF | EFLAGS_OF | 0x0000_0002;
        cpu.set_sub_flags32(0x0303_0303, 0x0101_0101, 0x0202_0202);
        assert!(!flag(&cpu, EFLAGS_AF));
        assert!(!flag(&cpu, EFLAGS_CF));
        assert!(!flag(&cpu, EFLAGS_OF));
    }
}
