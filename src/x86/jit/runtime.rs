//! AArch64 admission, invocation and lookup owner.
//! The code arena selects executable mappings at build time.
//!
//! Lookups require a valid shared execute translation and the recorded backing
//! page's content version. Guarded native blocks cannot change code or mappings
//! internally. Writable aliases are revoked on code-page promotion; scalar,
//! token, page-walk, bulk and device RAM writes notify the version owner. Loaders
//! and reset discard all derived code. No future page walk is done by lookahead.
//! A renewed watch may retain a page's compiled entries only after comparing
//! every byte used by its native and negative entries. Retain or retire the
//! whole page together: a same-page direct branch can reach any of those bodies.
//! Compare under exclusive CPU/bus ownership after execute revalidation. Renew
//! the watch and revoke writable aliases before republishing any retained link.
//! Link tags also qualify CS base and compiled segment facts with a stable
//! 12-bit context id. Context changes retain links; exhausting the 4095 ids
//! clears both privilege planes before any id is reused.
//! Protected 16-bit code requires a 16-bit stack and uses dynamic based segment
//! checks. Real mode, V86 and debug stepping remain interpreted. PM32 keeps its
//! full CS limit requirement and flat segment specializations.
//! Execute revalidation is per page: the previous per-PC tags caused about
//! 88 million context-related exits in the firmware workload. Restoring all
//! entries of a revalidated page reduced a development cold boot from 48.1 to
//! 32.5 seconds on the RPi, without changing its 1,975,715,720 guest steps.
//! These single-run diagnostics justify the structure, not final acceptance.
//!
//! Admission counts block entries, with HOT=8 on every ARM64 host. A fixed
//! 1 MiB table holds 131,072 packed tags/counts. Tags use EIP, RAM offset and
//! the page record's code-restart number. Count-only lookups install no watch.
//! Recorded code-byte changes restart heat; data-only writes retain it.
//! Collisions replace counts; internal capacity/table clears halve them and
//! external resets zero them. There is no time expiry or per-byte heat array.
//!
//! Cold interpretation ends at every decoder block boundary (including untaken
//! branches), page crossing, 32 instructions, or a precise event/fault boundary.
//! Negative entries, missing execute proofs, memory/division exits and shadows
//! use the dedicated single-instruction kernel. Ordinary ineligible batches and
//! single-step handoffs carry no cold observer. Unsupported successors receive
//! their own lookup. Compiler/publication failures are errors, never fallback.
//!
//! Build r86-run with `--features jit` for aarch64-unknown-linux-musl (using
//! RUSTFLAGS='-C linker=rust-lld' when cross compiling), then select
//! `--cpu-engine jit` or `cpu-engine = 'jit'` in TOML. Explicit CLI wins. The
//! CLI defaults to JIT on supported ARM64 Linux builds, otherwise interpretation.
//! Statistics distinguish native steps from interpreter boundaries; a successful
//! boot alone is not JIT validation.

use super::code::{CodeArena, CodeError, CodeHandle, ENTRY_TABLES, TableIndex, cache_limits};
use super::validity::{AliasSlot, CodeContent};
use super::{
    compile, decode,
    frame::Frame,
    links::{self, COLD, INTERPRET, Link, PAGE_BYTES, SLOTS},
    soft::View,
};
use crate::machine::HostBus;
#[cfg(debug_assertions)]
use crate::machine::{JitCacheCounters, JitCacheWork, JitClearCause};
use crate::x86::cpu::{FetchCacheHit, SegmentReg, TranslationRecheck};
use crate::x86::{
    BackingToken, BatchResult, BatchStop, Bus, Cpu, Interpreter, LinearAddr, PhysAddr,
};
use alloc::{boxed::Box, format, string::String, vec, vec::Vec};
use core::hash::{BuildHasherDefault, Hasher};
use std::collections::HashMap;

/// This bounded map hashes two addresses and segment facts, comparing the full
/// key. General-purpose byte hashing accounted for about 1.7% of the measured
/// firmware profile. Mix each address as a word; rotation distributes aligned
/// page bits before the next word, and the final fold supplies low bucket bits.
#[derive(Default)]
struct PageHasher(u64);

impl Hasher for PageHasher {
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 32)
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(25) ^ value).wrapping_mul(0x9e37_79b1_85eb_ca87);
    }

    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }
}

pub(crate) const HOT: u8 = 8;
const HEAT_SLOTS: usize = 131072;
const COLD_BATCH: u64 = 32;
const MAX_CONTEXTS: usize = PAGE_BYTES - 1;

// A scheduling hint, never a native-code identity. The upper 56 bits hold a
// tag; the low byte holds a saturating count. Exactly 1 MiB on every ARM64 host.
#[derive(Clone, Copy, Default)]
struct Heat(u64);

impl Heat {
    fn key(eip: u32, backing: u64, restart: u64) -> (usize, u64) {
        let mut hash = ((u64::from(eip) << 32) ^ backing) ^ restart.rotate_left(17);
        hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        hash ^= hash >> 31;
        (hash as usize & (HEAT_SLOTS - 1), hash & !255)
    }

    fn count(self) -> u8 {
        self.0 as u8
    }

    fn matches(self, tag: u64) -> bool {
        self.0 & !255 == tag
    }

    fn visit(&mut self, tag: u64) -> bool {
        let count = if self.matches(tag) {
            (self.count() + 1).min(HOT)
        } else {
            1
        };
        self.0 = tag | u64::from(count);
        count == HOT
    }

    fn decay(&mut self) {
        self.0 = (self.0 & !255) | u64::from(self.count() / 2);
    }
}

// A cold-only page needs just its negative-entry bitmap (512 B). Visit heat is
// bounded independently; promotion expands negative decisions into a native
// table and drops this allocation. No table is allocated for a counted-only page.
enum PageEntries {
    Cold(Box<[u64; PAGE_BYTES / 64]>),
    Native(TableIndex),
}

impl PageEntries {
    fn get(&self, arena: &CodeArena, offset: usize) -> u32 {
        match self {
            Self::Cold(negative) => ((negative[offset / 64] >> (offset % 64)) & 1) as u32,
            Self::Native(table) => arena.entries(*table)[offset],
        }
    }

    fn set(&mut self, arena: &mut CodeArena, offset: usize, entry: u32) {
        match self {
            Self::Cold(negative) => {
                debug_assert!(entry <= INTERPRET);
                let bit = 1u64 << (offset % 64);
                negative[offset / 64] =
                    (negative[offset / 64] & !bit) | (u64::from(entry) << (offset % 64));
            }
            Self::Native(table) => arena.entries_mut(*table)[offset] = entry,
        }
    }

    fn native(&self) -> Option<TableIndex> {
        match self {
            Self::Cold(_) => None,
            Self::Native(table) => Some(*table),
        }
    }

    fn promote(&mut self, arena: &mut CodeArena) -> TableIndex {
        match self {
            Self::Native(table) => *table,
            Self::Cold(negative) => {
                let table = arena.allocate_entries();
                for (offset, word) in arena.entries_mut(table).iter_mut().enumerate() {
                    *word = ((negative[offset / 64] >> (offset % 64)) & 1) as u32;
                }
                *self = Self::Native(table);
                table
            }
        }
    }

    fn clear(&mut self, arena: &mut CodeArena) {
        match self {
            Self::Cold(negative) => negative.fill(0),
            Self::Native(table) => arena.entries_mut(*table).fill(0),
        }
    }
}

struct CompiledPage {
    version: u64,
    version_ptr: *const u64,
    restart: u64,
    content: Box<CodeContent>,
    // Once promoted, live links address a stable arena table for every x86 byte.
    // Zero is unseen, one is interpreted, 2..=COLD are reserved; larger
    // values are signed byte offsets from the table's RX address. Code precedes
    // metadata in the same bounded mapping, so native displacements are negative
    // and cannot collide with sentinels. Cold entries use ordinary semantics.
    entries: PageEntries,
    // Native-PC metadata retains its associated emitted body address.
    #[cfg(target_os = "linux")]
    faults: Vec<(usize, Box<[super::memory::FaultSite]>)>,
    pending_edges: Vec<(usize, u16)>, // RX branch site and target byte offset.
    #[cfg(feature = "jit-profile")]
    opcodes: Box<[u16]>,
}

impl CompiledPage {
    fn new(version: u64, version_ptr: *const u64, arena: &mut CodeArena) -> Self {
        // Full pools (large hosts) keep a table per visited page, so pages
        // without native code still dispatch through links.
        let entries = if arena.table_capacity() < ENTRY_TABLES {
            PageEntries::Cold(Box::new([0; PAGE_BYTES / 64]))
        } else {
            PageEntries::Native(arena.allocate_entries())
        };
        Self {
            version,
            version_ptr,
            restart: 0,
            content: CodeContent::new(),
            entries,
            #[cfg(target_os = "linux")]
            faults: Vec::new(),
            pending_edges: Vec::new(),
            #[cfg(feature = "jit-profile")]
            opcodes: vec![0; PAGE_BYTES].into_boxed_slice(),
        }
    }

    fn invalidate(&mut self, version: u64, arena: &mut CodeArena) {
        self.version = version;
        self.entries.clear(arena);
        #[cfg(target_os = "linux")]
        self.faults.clear();
        self.pending_edges.clear();
        self.content.clear();
        self.restart = self
            .restart
            .checked_add(1)
            .expect("JIT page restart exhausted");
    }
}

#[derive(Clone, Copy)]
struct NativeBlock {
    body: usize,
}

impl NativeBlock {
    fn is_single_instruction(&self) -> bool {
        // SAFETY: lookup returned an aligned instruction in this live arena,
        // after checking the current execute mapping and content. Publication
        // and reclamation cannot occur between this lookup and invocation.
        // Only shadow entries read it; ordinary native dispatch is unchanged.
        unsafe { *(self.body as *const u32) == compile::ONE_INSTRUCTION_ENTRY }
    }
}

enum Lookup {
    Native(NativeBlock),
    Cold,
    Interpret,
}

fn relative_entry(entries: *const u32, displacement: u32) -> usize {
    (entries as usize).wrapping_add_signed(displacement as i32 as isize)
}

struct Entry {
    handle: CodeHandle,
    #[cfg(target_os = "linux")]
    base: usize,
    dispatch: usize,
    call_dispatch: usize,
    finish: usize,
    recheck: usize,
}

/// This helper borrows the invocation's sole CPU/HostBus owner without reading
/// GPRs, EIP, arithmetic flags or time, which are still resident in native code.
/// The probe changes only derived translations and framebuffer dirty metadata,
/// never architectural state or guest memory. Its returned framebuffer pointer
/// is confined to native execution and never enters Cpu's RAM/fetch caches.
unsafe extern "C" fn refill_data_translation(view: *mut View, page_tag: u32, write: u32) -> u64 {
    let linear = crate::x86::addr::LinearAddr(page_tag & !4095);
    // SAFETY: the invocation owns both objects and its host adapter installed
    // these pointers. No Rust reference is used concurrently by native code.
    let view = unsafe { &mut *view };
    let token = unsafe {
        crate::x86::mmu::try_resident_data_translation(
            &mut *view.cpu,
            &mut *view.bus.cast::<HostBus>(),
            linear,
            write != 0,
        )
    };
    let bias = token.map_or(0, |token| token.0.wrapping_sub(u64::from(linear.0)));
    view.remember_refill(page_tag, write != 0, bias);
    bias
}

/// Change a data segment without reading resident GPRs/flags/time or performing
/// a faultable access. Refresh its explicit native view; changed compiled
/// qualifications force exit before another block can run.
pub(super) unsafe extern "C" fn load_cached_segment<B: Bus>(
    frame: *mut Frame,
    segment: u32,
    selector: u32,
) -> u32 {
    let segment = crate::x86::cpu::SegmentReg::from_modrm_reg(segment as u8);
    // SAFETY: the invocation owns the frame, view, CPU and bus exclusively.
    // Generated callers end their block, preserve the fixed mapping state and
    // reload permissions; return 2 exits before following any specialized link.
    unsafe {
        let frame = &mut *frame;
        let view = &mut *frame.memory.cast::<View>();
        let cpu = &mut *view.cpu;
        if !crate::x86::interp::try_load_cached_segment(
            cpu,
            &mut *view.bus.cast::<B>(),
            segment,
            selector as u16,
        ) {
            return 0;
        }
        frame.selectors[segment as usize] = selector as u16;
        if view.refresh_segments(cpu) { 1 } else { 2 }
    }
}

/// Operands and CR0 were qualified by generated code. This helper cannot
/// interpret, access guest memory/devices or inspect stale GPRs/flags/time.
/// Stores call it only after their native access.
pub(super) unsafe extern "C" fn execute_mmx(view: *mut View, control: u32, value: u64) {
    // SAFETY: the invocation exclusively owns this CPU. Its scalar MMX array
    // pointer is read only outside this helper, with no concurrent Rust borrow.
    let fpu = unsafe { &mut (*(*view).cpu).fpu };
    let opcode = control as u8;
    let reg = (control >> 8) as u8;
    match opcode {
        0x77 => fpu.emms(),
        0x7e | 0x7f => fpu.prepare_mmx(),
        0x6e | 0x6f => fpu.set_mmx(reg, value),
        _ => fpu.set_mmx(
            reg,
            crate::x86::interp::mmx_binary_result(opcode, fpu.mmx(reg), value),
        ),
    }
}

#[derive(Clone, Copy, Default)]
struct CodeStamp {
    version_ptr: *const u64,
    version: u64,
    phys_page: u64,
    backing_page: u64,
}

#[derive(Clone, Default, Debug)]
pub struct Stats {
    pub native_steps: u64,
    pub interpreted_steps: u64,
    /// Batch-entry causes: ineligible, cold, after native exit, other fallback.
    /// Already-halted polls are excluded.
    #[cfg(debug_assertions)]
    pub interpreted_causes: [u64; 4],
    #[cfg(feature = "jit-profile")]
    pub eligible_interpreted_steps: u64,
    #[cfg(feature = "jit-profile")]
    pub heat_replacements: u64,
    #[cfg(feature = "jit-profile")]
    pub admissions: u64,
    #[cfg(feature = "jit-profile")]
    pub heat_replacements_ge4: u64,
    #[cfg(any(test, feature = "jit-profile"))]
    pub cold_batches: u64,
    pub compiled_blocks: u64,
    /// New admitted page records, including records recreated after a clear.
    #[cfg(debug_assertions)]
    pub recorded_pages: u64,
    pub content_changes: u64,
    #[cfg(any(test, debug_assertions, feature = "jit-profile"))]
    pub content_reuses: u64,
    /// All clears, including external resets; the two causes below are subsets.
    pub arena_clears: u64,
    pub capacity_clears: u64,
    pub table_clears: u64,
    pub code_bytes: u64,
    pub linked_edges: u64,
    #[cfg(any(test, feature = "jit-profile"))]
    pub global_link_retags: u64,
    #[cfg(any(test, feature = "jit-profile"))]
    pub context_clears: u64,
    #[cfg(feature = "jit-profile")]
    pub native_pm16_steps: u64,
    #[cfg(feature = "jit-profile")]
    pub interpreted_modes: [u64; 5],
    /// Native returns: far transfer, string, segment boundary, CS limit,
    /// memory, other (including dispatch/admission and budget/attention).
    #[cfg(feature = "jit-profile")]
    pub exit_reasons: [u64; 6],
    #[cfg(any(test, feature = "jit-profile"))]
    pub native_memory_reads: u64,
    #[cfg(any(test, feature = "jit-profile"))]
    pub native_memory_writes: u64,
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
    pub native_segment_loads: u64,
    #[cfg(any(test, feature = "jit-profile"))]
    pub partial_repeats: u64,
    #[cfg(any(test, feature = "jit-profile"))]
    pub division_exits: u64,
    pub memory_exits: u64,
    pub native_invocations: u64,
    #[cfg(feature = "jit-profile")]
    pub compile_nanoseconds: u128,
    #[cfg(feature = "jit-profile")]
    pub budget_exits: u64,
    #[cfg(feature = "jit-profile")]
    pub empty_invocations: u64,
    #[cfg(feature = "jit-profile")]
    pub dispatch_misses: u64,
    #[cfg(feature = "jit-profile")]
    pub dispatch_collisions: u64,
    #[cfg(feature = "jit-profile")]
    pub dispatch_context_changes: u64,
    #[cfg(feature = "jit-profile")]
    pub mapping_changes: u64,
    #[cfg(feature = "jit-profile")]
    pub same_cr3_mapping_changes: u64,
    #[cfg(feature = "jit-profile")]
    pub invlpg_ranges: u64,
    #[cfg(feature = "jit-profile")]
    pub dispatch_interpreter: u64,
    #[cfg(feature = "jit-profile")]
    pub dispatch_uncompiled: u64,
    /// Other (stack/string), ES..GS, split access, TLB tag, writable token.
    #[cfg(feature = "jit-profile")]
    pub memory_exit_reasons: [u64; 10],
    /// Empty row, old mapping generation, index collision, access privilege.
    #[cfg(feature = "jit-profile")]
    pub memory_tag_misses: [u64; 4],
    /// Full nonzero base, limited nonzero base, limited zero base, other.
    #[cfg(feature = "jit-profile")]
    pub segment_exit_shapes: [u64; 4],
    #[cfg(feature = "jit-profile")]
    pub write_misses_flushing_context: u64,
}

#[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExitReason {
    MemoryGuard,
    Division,
    CsLimit,
    Attention,
    Budget,
    DispatchMiss,
    FarTransfer,
    String,
    SegmentBoundary,
    Other,
}

#[cfg(debug_assertions)]
impl ExitReason {
    pub const NAMES: [&'static str; 10] = [
        "memory/guard",
        "division",
        "CS limit",
        "attention/shadow boundary",
        "budget",
        "dispatch miss",
        "far transfer",
        "string",
        "segment boundary",
        "other",
    ];

    fn boundary(kind: u32, at_limit: bool, attention: bool) -> Option<Self> {
        match kind {
            1 | 3 => Some(Self::MemoryGuard),
            4 => Some(Self::Division),
            7 => Some(Self::CsLimit),
            5 | 6 => Some(Self::Attention),
            _ if at_limit && attention => Some(Self::Attention),
            2 => Some(Self::Budget),
            _ if at_limit => Some(Self::Budget),
            _ => None,
        }
    }
}

/// One active session; exact reason totals and at most 4096 retained sites.
/// Sites identify the committed return/resume CS:EIP, not code identities.
#[cfg(debug_assertions)]
#[derive(Default)]
pub struct ExitSites {
    pub reasons: [u64; 10],
    pub sites: HashMap<(u16, u32, ExitReason), u64>,
}

#[cfg(debug_assertions)]
impl ExitSites {
    /// W95A full site counting exceeded the debug-session overhead budget.
    /// Reason totals remain exact; site counts are unscaled 1-in-8 samples.
    pub const SAMPLE_INTERVAL: u64 = 8;

    fn record(&mut self, cs: u16, eip: u32, reason: ExitReason, invocation: u64) {
        self.reasons[reason as usize] += 1;
        if invocation % Self::SAMPLE_INTERVAL != 0 {
            return;
        }
        let key = (cs, eip, reason);
        if let Some(count) = self.sites.get_mut(&key) {
            *count += 1;
        } else if self.sites.len() < 4096 {
            self.sites.insert(key, 1);
        }
    }
}

pub struct Engine {
    arena: CodeArena,
    entry: Entry,
    // Key: EIP at the linear page's start, canonical physical backing page,
    // and flat DS/ES/SS facts.
    // This also handles nonzero/unaligned CS bases and different RAM aliases.
    pages: HashMap<(u64, u64), CompiledPage, BuildHasherDefault<PageHasher>>,
    // Both privilege planes have fixed bounds. Keep that ownership invariant
    // in the types so a masked page index needs no dynamic slice-length check.
    links: Box<[Link; 2 * SLOTS]>,
    // Reverse RAM-page hint; stale slots are checked against their live stamp.
    // Multiple linear/privilege aliases use the existing full scan.
    link_aliases: Vec<AliasSlot>,
    user_links: bool,
    // Content stamps are read only after a code write, outside native execution.
    // Keeping them out of Link halves the generated dispatch table's footprint.
    stamps: Box<[CodeStamp; 2 * SLOTS]>,
    rechecks: Box<[TranslationRecheck; 2 * SLOTS]>,
    returns: Box<links::ReturnStack>,
    // Low word: CS base; high word: compiled segment facts. Compare once on
    // the usual unchanged path; hash the full pair only on a context switch.
    context: u64,
    context_id: u16,
    contexts: HashMap<u64, u16, BuildHasherDefault<PageHasher>>,
    heat: Box<[Heat]>,
    code_epoch: u64,
    mapping_epoch: Option<u64>,
    #[cfg(feature = "jit-profile")]
    previous_cr3: u32,
    global_epoch: u64,
    global_links: [u64; 2 * SLOTS / 64],
    link_epoch: u32,
    #[cfg(feature = "jit-profile")]
    unsupported: Box<[u64; 576]>,
    pub stats: Stats,
    #[cfg(debug_assertions)]
    pub(crate) profile: Option<ExitSites>,
    #[cfg(debug_assertions)]
    clear_causes: [u64; 4],
    #[cfg(debug_assertions)]
    last_clear: JitCacheWork,
}

impl Engine {
    pub fn new() -> Result<Self, String> {
        let (bytes, tables) = cache_limits();
        Self::with_limits(bytes, tables)
    }

    #[cfg(test)]
    pub(crate) fn with_capacity(bytes: usize) -> Result<Self, String> {
        Self::with_limits(bytes, ENTRY_TABLES)
    }

    pub(crate) fn with_limits(bytes: usize, tables: usize) -> Result<Self, String> {
        let mut arena = CodeArena::with_limits(bytes, tables)
            .map_err(|error| format!("JIT code mapping: {error:?}"))?;
        let entry = Self::publish_entry(&mut arena)?;
        Ok(Self {
            arena,
            entry,
            pages: HashMap::default(),
            links: vec![Link::EMPTY; 2 * SLOTS]
                .into_boxed_slice()
                .try_into()
                .ok()
                .expect("fixed link count"),
            link_aliases: Vec::new(),
            user_links: false,
            stamps: vec![CodeStamp::default(); 2 * SLOTS]
                .into_boxed_slice()
                .try_into()
                .ok()
                .expect("fixed stamp count"),
            rechecks: vec![TranslationRecheck::EMPTY; 2 * SLOTS]
                .into_boxed_slice()
                .try_into()
                .ok()
                .expect("fixed recheck count"),
            returns: Box::new(links::ReturnStack::new()),
            context: 0,
            context_id: 1,
            contexts: HashMap::from_iter([(0, 1)]),
            heat: vec![Heat::default(); HEAT_SLOTS].into_boxed_slice(),
            code_epoch: 0,
            mapping_epoch: None,
            #[cfg(feature = "jit-profile")]
            previous_cr3: 0,
            global_epoch: 0,
            global_links: [0; 2 * SLOTS / 64],
            link_epoch: 1,
            #[cfg(feature = "jit-profile")]
            unsupported: Box::new([0; 576]),
            stats: Stats::default(),
            #[cfg(debug_assertions)]
            profile: None,
            #[cfg(debug_assertions)]
            clear_causes: [0; 4],
            #[cfg(debug_assertions)]
            last_clear: JitCacheWork::default(),
        })
    }

    fn publish_entry(arena: &mut CodeArena) -> Result<Entry, String> {
        let code = compile::entry().map_err(|error| format!("JIT entry emission: {error:?}"))?;
        let handle = arena
            .publish(&code.words)
            .map_err(|error| format!("JIT entry publication: {error:?}"))?;
        let dispatch = arena
            .address(handle, code.dispatch_offset)
            .map_err(|error| format!("JIT dispatch entry: {error:?}"))?;
        let finish = arena
            .address(handle, code.return_offset)
            .map_err(|error| format!("JIT return entry: {error:?}"))?;
        Ok(Entry {
            handle,
            #[cfg(target_os = "linux")]
            base: arena.address(handle, 0).expect("fresh shared entry"),
            dispatch,
            call_dispatch: arena
                .address(handle, code.call_dispatch_offset)
                .expect("fresh call dispatch entry"),
            finish,
            recheck: arena
                .address(handle, code.recheck_offset)
                .expect("fresh recheck entry"),
        })
    }

    pub(crate) fn clear(&mut self, bus: &mut HostBus) -> Result<(), String> {
        self.heat.fill(Heat::default());
        self.reclaim(
            bus,
            #[cfg(debug_assertions)]
            JitClearCause::External,
        )
    }

    #[cfg(debug_assertions)]
    pub(crate) fn cache_counters(&self) -> JitCacheCounters {
        JitCacheCounters {
            causes: self.clear_causes,
            last_clear: self.last_clear,
            current: JitCacheWork {
                steps: self.stats.native_steps + self.stats.interpreted_steps,
                compiled_blocks: self.stats.compiled_blocks,
                code_bytes: self.stats.code_bytes,
                recorded_pages: self.stats.recorded_pages,
            },
        }
    }

    fn reclaim(
        &mut self,
        bus: &mut HostBus,
        #[cfg(debug_assertions)] cause: JitClearCause,
    ) -> Result<(), String> {
        // Only the rare clear path records work. Frontends poll these plain
        // counters; no logging or host-clock work enters execution batches.
        #[cfg(debug_assertions)]
        {
            self.clear_causes[cause as usize] += 1;
            self.last_clear = self.cache_counters().current;
        }
        self.links.fill(Link::EMPTY);
        self.link_aliases.fill(AliasSlot::default());
        self.global_links.fill(0);
        // Rewritten pages lose their nonzero restart identity here. Their
        // next visit replaces the old half-count; unchanged pages retain it.
        self.pages.clear();
        for heat in &mut self.heat {
            heat.decay();
        }
        self.arena.clear();
        // No compiled code or published link can observe an old version now.
        // Reclaim write watches with their code, including capacity evictions;
        // retaining them forever penalizes code pages later reused as data.
        bus.jit_release_code_pages();
        self.entry = Self::publish_entry(&mut self.arena)?;
        self.stats.arena_clears += 1;
        Ok(())
    }

    pub fn report(&self) -> String {
        // Diagnostic output is requested explicitly and collected after native
        // execution, so cycle sampling can use the production emitted code.
        #[cfg(target_os = "linux")]
        if let Some(path) = std::env::var_os("R86_JIT_DUMP") {
            self.arena
                .dump(
                    std::path::Path::new(&path),
                    &[
                        (0, "jit_entry"),
                        (self.entry.dispatch - self.entry.base, "jit_dispatch"),
                        (
                            self.entry.call_dispatch - self.entry.base,
                            "jit_call_dispatch",
                        ),
                        (self.entry.finish - self.entry.base, "jit_return"),
                        (self.entry.recheck - self.entry.base, "jit_recheck"),
                        (self.entry.handle.byte_len(), "jit_blocks"),
                    ],
                )
                .expect("write requested R86_JIT_DUMP diagnostic");
        }
        let result = format!(
            "jit: {:?}\njit cache: code_used={} tables_used={} code_capacity={} table_capacity={}\n",
            self.stats,
            self.arena.used_bytes(),
            self.arena.tables_used(),
            self.arena.capacity(),
            self.arena.table_capacity(),
        );
        #[cfg(target_os = "macos")]
        let result = result + &self.arena.protection_report();
        #[cfg(feature = "jit-profile")]
        {
            let mut ranked: Vec<_> = self
                .unsupported
                .iter()
                .copied()
                .enumerate()
                .filter(|(_, count)| *count != 0)
                .collect();
            ranked.sort_unstable_by_key(|&(_, count)| core::cmp::Reverse(count));
            let mut result = result;
            for (opcode, count) in ranked.into_iter().take(24) {
                let opcode = if opcode < 256 {
                    format!("{opcode:02x}")
                } else if opcode < 512 {
                    format!("0f{:02x}", opcode - 256)
                } else {
                    let detail = opcode - 512;
                    let name = ["d3", "f7", "0fba", "8e"][detail / 16];
                    let form = if detail & 8 != 0 { "reg" } else { "mem" };
                    format!("{name}/{:x} {form}", detail & 7)
                };
                result.push_str(&format!("jit.unsupported {opcode}: {count}\n"));
            }
            result
        }
        #[cfg(not(feature = "jit-profile"))]
        result
    }

    fn intern_context(&mut self, context: u64) -> u16 {
        if let Some(&id) = self.contexts.get(&context) {
            return id;
        }
        if self.contexts.len() == MAX_CONTEXTS {
            // Native execution has returned. No live link may name a reused
            // id, including a global link in the other privilege plane.
            self.links.fill(Link::EMPTY);
            self.link_aliases.fill(AliasSlot::default());
            self.global_links.fill(0);
            self.contexts.clear();
            #[cfg(any(test, feature = "jit-profile"))]
            {
                self.stats.context_clears += 1;
            }
        }
        let id = (self.contexts.len() + 1) as u16;
        self.contexts.insert(context, id);
        id
    }

    fn link_context(&self) -> u64 {
        (u64::from(self.link_epoch) << 32) | u64::from(self.context_id)
    }

    #[cfg(any(debug_assertions, feature = "jit-profile"))]
    fn profile_exit(cpu: &mut Cpu, bus: &HostBus, kind: u32) -> usize {
        match kind {
            7 => return 3,
            1 | 3 => return 4,
            0 => {}
            _ => return 5,
        }
        // Observe only already resident bytes. No page walk, A/D update or
        // device read is allowed for an exit classifier; unknown stays other.
        let linear = LinearAddr(cpu.seg(SegmentReg::Cs).base.wrapping_add(cpu.eip));
        let Some(hit) = cpu.fetch_cache_lookup(linear) else {
            return 5;
        };
        let bytes = bus.jit_code_bytes(
            bus.jit_ram_offset(hit.backing),
            (4096 - (linear.0 & 4095) as usize).min(15),
        );
        let Some(at) = bytes.iter().position(|b| {
            !matches!(
                b,
                0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0xf0 | 0xf2 | 0xf3
            )
        }) else {
            return 5;
        };
        match bytes[at] {
            0x9a | 0xea | 0xca | 0xcb => 0,
            0xff if bytes
                .get(at + 1)
                .is_some_and(|b| matches!((b >> 3) & 7, 3 | 5)) =>
            {
                0
            }
            0x6c..=0x6f | 0xa4..=0xa7 | 0xaa..=0xaf => 1,
            0x07 | 0x17 | 0x1f | 0x8e | 0xc4 | 0xc5 => 2,
            0x0f if bytes
                .get(at + 1)
                .is_some_and(|b| matches!(b, 0xa1 | 0xa9 | 0xb2 | 0xb4 | 0xb5)) =>
            {
                2
            }
            _ => 5,
        }
    }

    fn sync_mapping(&mut self, cpu: &mut Cpu) {
        // Execute rechecks certify mappings, not compiled assumptions. Keep
        // both in the tag so returning to an old context can reuse its links.
        let context =
            u64::from(cpu.seg(SegmentReg::Cs).base) | (u64::from(View::block_facts(cpu)) << 32);
        if self.context != context {
            self.context = context;
            self.context_id = self.intern_context(context);
        }
        // Paging distinguishes CPL 3 from supervisor access. Retain both
        // certified execute translations across calls/interrupts; a kernel
        // link must never certify that the same page is user-executable.
        self.user_links = cpu.cpl() == 3;
        let epoch = cpu.jit_mapping_epoch();
        let invalidated = cpu.take_jit_invalidated_range();
        #[cfg(feature = "jit-profile")]
        {
            self.stats.invlpg_ranges += u64::from(invalidated.is_some());
        }
        if let Some((base, mask)) = invalidated {
            if mask == 0xffff_f000 {
                let slot = links::slot(base);
                for slot in [slot, slot + SLOTS] {
                    let link = &mut self.links[slot];
                    if link.tag as u32 & mask == base {
                        *link = Link::EMPTY;
                    }
                }
            } else {
                for link in self.links.iter_mut() {
                    if link.tag as u32 & mask == base {
                        *link = Link::EMPTY;
                    }
                }
            }
        }
        if self.mapping_epoch == Some(epoch) {
            return;
        }
        self.mapping_epoch = Some(epoch);
        #[cfg(feature = "jit-profile")]
        {
            self.stats.mapping_changes += 1;
            self.stats.same_cr3_mapping_changes += u64::from(cpu.cr3 == self.previous_cr3);
            self.previous_cr3 = cpu.cr3;
        }
        let old_epoch = self.link_epoch;
        self.link_epoch = self.link_epoch.wrapping_add(1);
        if self.link_epoch == 0 {
            self.links.fill(Link::EMPTY);
            self.link_aliases.fill(AliasSlot::default());
            self.link_epoch = 1;
        }
        let global_epoch = cpu.jit_global_epoch();
        if self.global_epoch == global_epoch {
            for (word, bits) in self.global_links.iter_mut().enumerate() {
                let mut old_bits = *bits;
                *bits = 0;
                while old_bits != 0 {
                    let bit = old_bits.trailing_zeros() as usize;
                    old_bits &= old_bits - 1;
                    let link = &mut self.links[word * 64 + bit];
                    if link.tag >> 32 == u64::from(old_epoch) {
                        // Preserve the linear page and its context id, even
                        // when that context is not the current one.
                        link.tag = (u64::from(self.link_epoch) << 32) | (link.tag & 0xffff_ffff);
                        *bits |= 1 << bit;
                        #[cfg(any(test, feature = "jit-profile"))]
                        {
                            self.stats.global_link_retags += 1;
                        }
                    }
                }
            }
        } else {
            self.global_epoch = global_epoch;
            self.global_links.fill(0);
        }
    }

    fn link(
        &mut self,
        linear: u32,
        ram_page: usize,
        version: u64,
        version_ptr: *const u64,
        entries: *const u32,
        global: bool,
        recheck: TranslationRecheck,
        fetch: FetchCacheHit,
    ) {
        let slot = usize::from(self.user_links) * SLOTS + links::slot(linear);
        if self.link_aliases.len() <= ram_page {
            self.link_aliases.resize(ram_page + 1, AliasSlot::default());
        }
        self.link_aliases[ram_page].record(slot);
        self.links[slot] = Link {
            tag: self.link_context() | u64::from(linear & !4095),
            entries,
        };
        self.stamps[slot] = CodeStamp {
            version,
            version_ptr,
            phys_page: fetch.phys.0 & !4095,
            backing_page: fetch.backing.0.wrapping_sub(u64::from(linear & 4095)),
        };
        self.rechecks[slot] = recheck;
        self.global_links[slot >> 6] = (self.global_links[slot >> 6] & !(1 << (slot & 63)))
            | (u64::from(global) << (slot & 63));
    }

    fn sync_code(&mut self, bus: &HostBus) {
        let epoch = bus.jit_code_epoch();
        if self.code_epoch == epoch {
            return;
        }
        let single = bus.jit_single_retired_page(self.code_epoch);
        self.code_epoch = epoch;
        if let Some((ram_page, version_ptr)) = single {
            self.retire_code_page(ram_page, version_ptr);
            return;
        }
        for (link, stamp) in self.links.iter_mut().zip(self.stamps.iter()) {
            if link.tag != 0 {
                // SAFETY: a published nonzero tag owns a stable CodePages cell.
                // Reclamation/reset clears links before replacing its owner.
                // Native writes cannot touch watched pages; all other writes
                // notify that owner and advance this epoch before re-entry.
                if unsafe { *stamp.version_ptr } != stamp.version {
                    *link = Link::EMPTY;
                }
            }
        }
    }

    fn retire_code_page(&mut self, ram_page: usize, version_ptr: *const u64) {
        // SAFETY: CodePages just supplied its stable, live version cell.
        // One retirement since our last scan touched only this backing
        // page. All linear/privilege aliases share that cell; compare its
        // current version once instead of loading every published version.
        let version = unsafe { *version_ptr };
        let aliases = self
            .link_aliases
            .get_mut(ram_page)
            .map(core::mem::take)
            .unwrap_or_default();
        if let Some(slot) = aliases.single() {
            if let Some(slot) = slot {
                let stamp = &self.stamps[slot];
                if stamp.version_ptr == version_ptr && stamp.version != version {
                    self.links[slot] = Link::EMPTY;
                }
            }
            return;
        }
        for (link, stamp) in self.links.iter_mut().zip(self.stamps.iter()) {
            if stamp.version_ptr == version_ptr && stamp.version != version {
                *link = Link::EMPTY;
            }
        }
    }

    fn prepare_interpreter_fetch(&self, cpu: &mut Cpu) {
        if cpu.fetch_cursor_backing_u8(cpu.eip).is_some() {
            return;
        }
        let linear = LinearAddr((self.context as u32).wrapping_add(cpu.eip));
        let slot = usize::from(self.user_links) * SLOTS + links::slot(linear.0);
        if self.links[slot].tag != self.link_context() | u64::from(linear.0 & !4095) {
            return;
        }
        // lookup retired invalid mappings/content before entering this chain;
        // native rechecks may only renew the same mapping. No native operation
        // can change code, segment state or paging controls. Reuse that proof
        // at an interpreter boundary instead of walking this execute page again.
        let offset = u64::from(linear.0 & 4095);
        let stamp = self.stamps[slot];
        let backing = BackingToken(stamp.backing_page.wrapping_add(offset));
        if cpu.fetch_cache_lookup(linear).is_none() {
            let global = self.global_links[slot >> 6] & (1 << (slot & 63)) != 0;
            cpu.restore_fetch_translation(
                linear,
                FetchCacheHit {
                    phys: PhysAddr(stamp.phys_page + offset),
                    backing,
                },
                global,
                self.rechecks[slot],
            );
        }
        let Some(remaining) = cpu.seg(SegmentReg::Cs).limit.checked_sub(cpu.eip) else {
            return;
        };
        let window = (PAGE_BYTES as u64 - offset).min(u64::from(remaining) + 1);
        cpu.fill_fetch_cursor_after(cpu.eip, backing, 1, window as u32);
    }

    fn lookup(&mut self, cpu: &mut Cpu, bus: &mut HostBus) -> Result<Lookup, String> {
        self.sync_mapping(cpu);
        self.sync_code(bus);
        let linear = LinearAddr(cpu.seg(SegmentReg::Cs).base.wrapping_add(cpu.eip));
        let offset = (linear.0 & 4095) as usize;
        let link = self.links[usize::from(self.user_links) * SLOTS + links::slot(linear.0)];
        if link.tag == self.link_context() | u64::from(linear.0 & !4095) {
            // SAFETY: matching page tag owns a PAGE_BYTES-entry allocation;
            // sync_code retired any changed contents before this lookup.
            // clear drops the allocation only after invalidating all links.
            let entry = unsafe { *link.entries.add(offset) };
            if entry > COLD {
                return Ok(Lookup::Native(NativeBlock {
                    body: relative_entry(link.entries, entry),
                }));
            }
            // Opcode diagnostics intentionally revisit the negative cache.
            #[cfg(not(feature = "jit-profile"))]
            if entry == INTERPRET {
                return Ok(Lookup::Interpret);
            }
        }
        bus.instruction_address(linear.0);
        // Reuse the shared resident-page proof for this instruction's fetch.
        // It neither fills a future page nor performs A-bit/device/fault work;
        // those cases still enter the interpreter at this exact instruction.
        let Some(hit) = cpu
            .fetch_cache_lookup(linear)
            .or_else(|| crate::x86::mmu::try_resident_fetch_translation(cpu, bus, linear))
        else {
            return Ok(Lookup::Interpret);
        };
        let global = cpu.fetch_translation_is_global(linear);
        let recheck = cpu.fetch_recheck(linear);
        let window = (PAGE_BYTES - offset).min(480);
        let window = window.min((u32::MAX - cpu.eip) as usize + 1);
        let backing = bus.jit_ram_offset(hit.backing);
        // Preserve the full EIP origin and segment qualifications in one word.
        // Lookup hashes and compares two u64s instead of three scalar fields;
        // no qualification bits are discarded, including unaligned CS bases.
        let key = (
            u64::from(cpu.eip.wrapping_sub(offset as u32)) | (self.context & !0xffff_ffff),
            backing - offset as u64,
        );
        let mut restart = 0;
        if let Some(page) = self.pages.get_mut(&key) {
            // SAFETY: the CodePages owner outlives all page records; cache
            // reclamation drops records before releasing its stable cells.
            let observed = unsafe { *page.version_ptr };
            if page.version != observed || observed & 1 == 0 {
                if page
                    .content
                    .matches(bus.jit_code_bytes(key.1, page.content.end))
                {
                    page.version = observed;
                    #[cfg(any(test, debug_assertions, feature = "jit-profile"))]
                    {
                        self.stats.content_reuses += 1;
                    }
                } else {
                    page.invalidate(observed, &mut self.arena);
                    self.stats.content_changes += 1;
                }
            }
            restart = page.restart;
            let entry = page.entries.get(&self.arena, offset);
            if entry == INTERPRET || entry > COLD {
                // Only reuse/publication renews the watch. An even version is
                // compared again on a later count-only lookup: ordinary writes
                // to an unwatched page do not change that version cell.
                let (_, version, version_ptr) = bus.jit_code_stamp(cpu, hit.backing);
                page.version = version;
                #[cfg(feature = "jit-profile")]
                if entry == INTERPRET {
                    self.unsupported[usize::from(page.opcodes[offset])] += 1;
                }
                if let Some(table) = page.entries.native() {
                    let entries = self.arena.entries_pointer(table);
                    self.link(
                        linear.0,
                        (key.1 >> 12) as usize,
                        version,
                        version_ptr,
                        entries,
                        global,
                        recheck,
                        hit,
                    );
                    if entry > COLD {
                        return Ok(Lookup::Native(NativeBlock {
                            body: relative_entry(entries, entry),
                        }));
                    }
                }
                return Ok(Lookup::Interpret);
            }
        }
        // Source versions are execute proofs, not heat identities. A data
        // write cannot cool entries; only changed recorded code restarts them.
        // Counting an unrecorded page neither watches it nor revokes aliases.
        let (slot, tag) = Heat::key(cpu.eip, backing, restart);
        #[cfg(feature = "jit-profile")]
        if !self.heat[slot].matches(tag) {
            self.stats.heat_replacements += u64::from(self.heat[slot].count() != 0);
            self.stats.heat_replacements_ge4 += u64::from(self.heat[slot].count() >= 4);
        }
        if !self.heat[slot].visit(tag) {
            return Ok(Lookup::Cold);
        }
        #[cfg(feature = "jit-profile")]
        {
            self.stats.admissions += 1;
        }
        let (_, mut version, mut version_ptr) = bus.jit_code_stamp(cpu, hit.backing);
        // Counted-only pages consume no entry table or CompiledPage. Bound the
        // admitted page metadata as before, outside any active native stack.
        let new_page = !self.pages.contains_key(&key);
        if new_page
            && (self.pages.len() >= ENTRY_TABLES
                || (self.arena.table_capacity() == ENTRY_TABLES
                    && self.arena.tables_used() == self.arena.table_capacity()))
        {
            self.stats.table_clears += 1;
            #[cfg(debug_assertions)]
            let cause = if self.pages.len() >= ENTRY_TABLES {
                JitClearCause::PageRecords
            } else {
                JitClearCause::EntryTable
            };
            self.reclaim(
                bus,
                #[cfg(debug_assertions)]
                cause,
            )?;
            (_, version, version_ptr) = bus.jit_code_stamp(cpu, hit.backing);
        }
        #[cfg(all(feature = "jit-profile", target_os = "linux"))]
        let compilation_start = std::time::Instant::now();
        #[cfg(feature = "jit-profile")]
        let opcode = {
            let bytes = bus.jit_code_bytes(backing, window);
            let offset = bytes
                .iter()
                .take(15)
                .position(|b| {
                    !matches!(
                        b,
                        0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0xf0 | 0xf2 | 0xf3
                    )
                })
                .unwrap_or(0);
            let first = bytes[offset];
            let opcode = if first == 0x0f {
                256 + usize::from(*bytes.get(offset + 1).unwrap_or(&0))
            } else {
                usize::from(first)
            };
            if let Some(group) = [0xd3, 0xf7, 0x1ba, 0x8e]
                .iter()
                .position(|&op| op == opcode)
            {
                let modrm = *bytes
                    .get(offset + if first == 0x0f { 2 } else { 1 })
                    .unwrap_or(&0);
                512 + group * 16 + usize::from((modrm >> 3) & 7) + if modrm >= 0xc0 { 8 } else { 0 }
            } else {
                opcode
            }
        };
        // A shadow requires one complete instruction before the next attention
        // boundary. Compile that entry with the existing one-instruction ABI;
        // otherwise a longer block would be rejected on every shadowed visit.
        let count = if cpu.interrupt_shadow() { 1 } else { 32 };
        let bytes = bus.jit_code_bytes(backing, window);
        let decoded = if cpu.code_default16() {
            decode::block_code16(bytes, cpu.eip, count)
        } else {
            decode::block_extended(bytes, cpu.eip, count)
        };
        let (native, _faults, edges) = if decoded.instructions.is_empty() {
            (None, Vec::new(), Vec::new())
        } else {
            let mut code = compile::block(
                &decoded,
                Some(links::Page {
                    eip_start: key.0 as u32,
                }),
                (self.context >> 32) as u32,
            )
            .map_err(|error| format!("JIT compiler at {:#x}: {error:?}", cpu.eip))?;
            if self.arena.tables_used() == self.arena.table_capacity()
                && self
                    .pages
                    .get(&key)
                    .is_none_or(|page| page.entries.native().is_none())
            {
                self.stats.table_clears += 1;
                self.reclaim(
                    bus,
                    #[cfg(debug_assertions)]
                    JitClearCause::EntryTable,
                )?;
                (_, version, version_ptr) = bus.jit_code_stamp(cpu, hit.backing);
            }
            let entry = match self.arena.publish_block(&mut code) {
                Err(CodeError::Capacity) => {
                    self.stats.capacity_clears += 1;
                    // No active native stack exists here; reclaiming an arena
                    // invalidates all handles, lookup entries and pending edges.
                    // Emitted blocks contain no old-arena/table addresses.
                    self.reclaim(
                        bus,
                        #[cfg(debug_assertions)]
                        JitClearCause::CodeArea,
                    )?;
                    (_, version, version_ptr) = bus.jit_code_stamp(cpu, hit.backing);
                    self.arena.publish_block(&mut code)
                }
                result => result,
            }
            .map_err(|error| format!("JIT code publication: {error:?}"))?;
            self.stats.compiled_blocks += 1;
            self.stats.code_bytes += (code.words.len() * 4) as u64;
            let body = self
                .arena
                .address(entry, 0)
                .map_err(|error| format!("JIT linked entry: {error:?}"))?;
            (Some(NativeBlock { body }), code.faults, code.edges)
        };
        let page = self.pages.entry(key).or_insert_with(|| {
            #[cfg(debug_assertions)]
            {
                self.stats.recorded_pages += 1;
            }
            CompiledPage::new(version, version_ptr, &mut self.arena)
        });
        page.version = version;
        {
            let content = &mut page.content;
            // A negative decision depends on at most one architectural
            // instruction. Keeping its first 15 bytes unchanged cannot make an
            // overlong or unsupported instruction eligible. A page/EIP boundary
            // may shorten it.
            let used = if native.is_some() {
                decoded.byte_len
            } else {
                window.min(15)
            };
            content.record(offset, bus.jit_code_bytes(backing, used));
        }
        if let Some(block) = native {
            let table = page.entries.promote(&mut self.arena);
            let entries = self.arena.entries_pointer(table);
            let entry = i32::try_from(block.body as isize - entries as isize)
                .expect("bounded relative native entry") as u32;
            self.arena.entries_mut(table)[offset] = entry;
            #[cfg(target_os = "linux")]
            page.faults.push((block.body, _faults.into_boxed_slice()));
            page.pending_edges.extend(
                edges
                    .into_iter()
                    .map(|edge| (block.body + edge.native_offset as usize, edge.target_offset)),
            );
        } else {
            page.entries.set(&mut self.arena, offset, INTERPRET);
        }
        if let Some(table) = page.entries.native() {
            let entries = self.arena.entries_pointer(table);
            // Every entry on this page has the same mapping/content validity.
            // A target is published once per version, so a resolved edge needs
            // no backlink/unpatch machinery: invalidation retires its source too.
            for index in (0..page.pending_edges.len()).rev() {
                let (source, offset) = page.pending_edges[index];
                let target = self.arena.entries(table)[usize::from(offset)];
                if target > COLD {
                    let patched = self
                        .arena
                        .patch_branch(source, relative_entry(entries, target));
                    page.pending_edges.swap_remove(index);
                    self.stats.linked_edges += u64::from(patched);
                } else if target == INTERPRET {
                    // This version can never gain native code for that opcode.
                    page.pending_edges.swap_remove(index);
                }
            }
        }
        #[cfg(feature = "jit-profile")]
        {
            page.opcodes[offset] = opcode as u16;
            #[cfg(target_os = "linux")]
            {
                self.stats.compile_nanoseconds += compilation_start.elapsed().as_nanos();
            }
            if native.is_none() {
                self.unsupported[opcode] += 1;
            }
        }
        if let Some(table) = page.entries.native() {
            let entries = self.arena.entries_pointer(table);
            self.link(
                linear.0,
                (key.1 >> 12) as usize,
                version,
                version_ptr,
                entries,
                global,
                recheck,
                hit,
            );
        }
        Ok(native.map_or(Lookup::Interpret, Lookup::Native))
    }

    #[cfg(debug_assertions)]
    fn session_exit_reason(
        &self,
        cpu: &mut Cpu,
        bus: &HostBus,
        frame: &Frame,
        limit: u64,
        attention: bool,
    ) -> ExitReason {
        if let Some(reason) =
            ExitReason::boundary(frame.exit_kind, frame.completed == limit, attention)
        {
            return reason;
        }
        if frame.exit_kind == 0 {
            let linear = (self.context as u32).wrapping_add(cpu.eip);
            let link = self.links[usize::from(self.user_links) * SLOTS + links::slot(linear)];
            // SAFETY: the matching tag certifies a live entry table; native
            // execution cannot change its mapping or watched code.
            if link.tag != frame.link_context | u64::from(linear & !4095)
                || unsafe { *link.entries.add((linear & 4095) as usize) } != INTERPRET
            {
                return ExitReason::DispatchMiss;
            }
            return match Self::profile_exit(cpu, bus, 0) {
                0 => ExitReason::FarTransfer,
                1 => ExitReason::String,
                2 => ExitReason::SegmentBoundary,
                _ => ExitReason::Other,
            };
        }
        ExitReason::Other
    }

    pub(crate) fn run_batch(
        &mut self,
        cpu: &mut Cpu,
        bus: &mut HostBus,
        start: u64,
        budget: u64,
    ) -> Result<BatchResult, String> {
        bus.sync_ioram(cpu);
        bus.jit_prepare_code_pages();
        let mut ran = 0;
        while ran < budget {
            let mut cold = false;
            let mut repeat = None;
            #[cfg(debug_assertions)]
            let was_halted = cpu.halted;
            #[cfg(debug_assertions)]
            let mut after_native = false;
            #[cfg(feature = "jit-profile")]
            let mut write_token_miss = false;
            // Real mode, V86 and TF remain interpreted. PM16 requires SS.B=0;
            // PM32 retains its full CS limit rule and flat specializations.
            // Taking the normal interpreter batch also avoids a slow
            // per-instruction real-mode path during BIOS startup. The next
            // machine boundary retries JIT.
            let shadowed = cpu.interrupt_shadow();
            let eligible = cpu.jit_code_eligible();
            let plan = bus.batch_attention_limit(start.wrapping_add(ran), budget - ran);
            let (limit, attention) = plan.unwrap_or((1, false));
            let native_limit = if shadowed { 1 } else { limit };
            let native_attention = attention || shadowed;
            if eligible {
                let entry = self.lookup(cpu, bus)?;
                cold = matches!(entry, Lookup::Cold);
                let native = match entry {
                    Lookup::Native(block) if !shadowed || block.is_single_instruction() => {
                        Some(block)
                    }
                    _ => None,
                };
                if let Some(NativeBlock { body }) = native {
                    cpu.begin_instruction();
                    cpu.segment_override = None;
                    let mut frame = Frame::from_cpu(cpu, native_limit);
                    frame.native_entry = body;
                    frame.dispatch_entry = self.entry.dispatch;
                    frame.call_dispatch_entry = self.entry.call_dispatch;
                    frame.return_entry = self.entry.finish;
                    frame.recheck_entry = self.entry.recheck;
                    let link_base = usize::from(self.user_links) * SLOTS;
                    frame.links = self.links[link_base..].as_mut_ptr();
                    frame.link_rechecks = self.rechecks[link_base..].as_ptr();
                    frame.link_context = self.link_context();
                    frame.call_base = &mut *self.returns;
                    let mut memory = View::new(cpu, bus.jit_rechecks_enabled());
                    memory.bus = (bus as *mut HostBus).cast();
                    memory.refill = refill_data_translation as *const () as usize;
                    memory.load_segment = load_cached_segment::<HostBus> as *const () as usize;
                    memory.mmx = execute_mmx as *const () as usize;
                    frame.memory = (&mut memory as *mut View).cast();
                    // SAFETY: entry was emitted from these validated bytes;
                    // the exclusive frame and memory view outlive execution.
                    // HostBus issues stable RAM/linear-framebuffer pointers;
                    // guards establish segment, paging and backing permission.
                    // The refill helper borrows this same owner to update only
                    // translations/dirty metadata while GPRs/flags stay native.
                    // Device reconfiguration and code publication require exit.
                    unsafe { self.arena.invoke(self.entry.handle, &mut frame) }
                        .map_err(|error| format!("JIT execution: {error:?}"))?;
                    assert!(
                        frame.completed <= native_limit,
                        "JIT chain exceeded its budget"
                    );
                    frame.commit(cpu);
                    ran += frame.completed;
                    self.stats.native_steps += frame.completed;
                    #[cfg(any(test, feature = "jit-profile"))]
                    {
                        self.stats.native_memory_reads += frame.memory_reads;
                        self.stats.native_memory_writes += frame.memory_writes;
                        self.stats.rechecked_data += frame.rechecked_data;
                        self.stats.rechecked_code += frame.rechecked_code;
                        self.stats.refilled_data += frame.refilled_data;
                        self.stats.cached_data_refills += frame.cached_data_refills;
                        self.stats.cached_returns += frame.cached_returns;
                        self.stats.call_overflows += frame.call_overflows;
                        self.stats.native_segment_loads += frame.loaded_segments;
                    }
                    self.stats.native_invocations += 1;
                    #[cfg(debug_assertions)]
                    if self.profile.is_some() {
                        let reason = self.session_exit_reason(
                            cpu,
                            bus,
                            &frame,
                            native_limit,
                            native_attention,
                        );
                        self.profile.as_mut().unwrap().record(
                            cpu.seg(SegmentReg::Cs).selector,
                            cpu.eip,
                            reason,
                            self.stats.native_invocations,
                        );
                    }
                    #[cfg(feature = "jit-profile")]
                    {
                        if cpu.code_default16() {
                            self.stats.native_pm16_steps += frame.completed;
                        }
                        self.stats.exit_reasons[Self::profile_exit(cpu, bus, frame.exit_kind)] += 1;
                        self.stats.empty_invocations += u64::from(frame.completed == 0);
                        self.stats.budget_exits += u64::from(frame.exit_kind == 2);
                        if frame.exit_kind == 0 {
                            let linear = (self.context as u32).wrapping_add(cpu.eip);
                            let link = self.links
                                [usize::from(self.user_links) * SLOTS + links::slot(linear)];
                            if link.tag != frame.link_context | u64::from(linear & !4095) {
                                self.stats.dispatch_misses += 1;
                                if link.tag & !0xffff_f000_u64 == frame.link_context {
                                    self.stats.dispatch_collisions += 1;
                                } else if link.tag != 0 {
                                    self.stats.dispatch_context_changes += 1;
                                }
                            } else {
                                // SAFETY: a matching tag certifies the same live
                                // allocation as lookup; native cannot modify code.
                                if unsafe { *link.entries.add((linear & 4095) as usize) }
                                    == INTERPRET
                                {
                                    self.stats.dispatch_interpreter += 1;
                                } else {
                                    self.stats.dispatch_uncompiled += 1;
                                }
                            }
                        }
                    }
                    if frame.completed != 0 {
                        bus.begin_instruction(
                            start.wrapping_add(ran).wrapping_sub(1),
                            cpu.tsc().wrapping_sub(1),
                        );
                    }
                    if matches!(frame.exit_kind, 5 | 6)
                        || (frame.completed == native_limit && native_attention)
                    {
                        // STI's new shadow belongs to this control exit. Keep
                        // its test off ordinary native returns, and establish
                        // inhibition before Machine can inspect pending IRQs.
                        if frame.exit_kind == 6 {
                            cpu.inhibit_maskable_interrupts();
                            if crate::x86::interp::continue_after_interrupt_shadow(
                                cpu,
                                bus,
                                frame.completed < native_limit,
                            ) {
                                continue;
                            }
                        }
                        return Ok(BatchResult {
                            ran,
                            stop: BatchStop::Attention {
                                instruction_step: start.wrapping_add(ran).wrapping_sub(1),
                                was_interruptible: false,
                            },
                        });
                    }
                    if frame.completed == native_limit {
                        continue;
                    }
                    if shadowed {
                        // No complete instruction retired. Restore inhibition
                        // before the interpreter begins the faulting access or
                        // resumes REP's already committed elements. Its begin
                        // boundary must still observe the original shadow.
                        debug_assert_eq!(frame.completed, 0);
                        cpu.restore_interrupt_shadow(true);
                    }
                    if frame.exit_kind == 0 {
                        assert!(frame.completed != 0, "JIT normal exit lost progress");
                        let linear = (self.context as u32).wrapping_add(cpu.eip);
                        let link =
                            self.links[usize::from(self.user_links) * SLOTS + links::slot(linear)];
                        // Native code cannot change mappings or watched code.
                        // A still-certified negative entry needs interpretation,
                        // including unresolved same-page edges which bypass the
                        // native dispatcher. Cold/stale entries need full lookup.
                        if link.tag != frame.link_context | u64::from(linear & !4095)
                            // SAFETY: the matching tag certifies a live page table,
                            // just as in lookup; the offset is within that table.
                            || unsafe { *link.entries.add((linear & 4095) as usize) } != INTERPRET
                        {
                            continue;
                        }
                        #[cfg(feature = "jit-profile")]
                        self.lookup(cpu, bus)?; // Opcode counts.
                    }
                    assert!(
                        matches!(frame.exit_kind, 0..=4 | 7),
                        "unknown JIT exit kind"
                    );
                    #[cfg(debug_assertions)]
                    {
                        after_native = true;
                    }
                    #[cfg(any(test, feature = "jit-profile"))]
                    {
                        self.stats.division_exits += u64::from(frame.exit_kind == 4);
                    }
                    self.stats.memory_exits += u64::from(matches!(frame.exit_kind, 1 | 3));
                    #[cfg(feature = "jit-profile")]
                    if matches!(frame.exit_kind, 1 | 3) {
                        write_token_miss = frame.memory_exit_reason == 9;
                        self.stats.memory_exit_reasons[frame.memory_exit_reason as usize] += 1;
                        if frame.memory_exit_reason == 8 {
                            let old = frame.memory_cached_tag;
                            let new = frame.memory_expected_tag;
                            let kind = if old == 0 {
                                0
                            } else if (old ^ new) & 0xffe != 0 {
                                1
                            } else if (old ^ new) & 0xffff_f000 != 0 {
                                2
                            } else {
                                3
                            };
                            self.stats.memory_tag_misses[kind] += 1;
                        }
                        if (1..=6).contains(&frame.memory_exit_reason) {
                            let segment = cpu.seg(
                                [
                                    SegmentReg::Es,
                                    SegmentReg::Cs,
                                    SegmentReg::Ss,
                                    SegmentReg::Ds,
                                    SegmentReg::Fs,
                                    SegmentReg::Gs,
                                ][frame.memory_exit_reason as usize - 1],
                            );
                            let shape = match (segment.base != 0, segment.limit != u32::MAX) {
                                (true, false) => 0,
                                (true, true) => 1,
                                (false, true) => 2,
                                (false, false) => 3,
                            };
                            self.stats.segment_exit_shapes[shape] += 1;
                        }
                    }
                    // Ordinary misses precede the instruction. REP instead
                    // resumes committed elements with the same total unit
                    // limit. Neither exit has consumed this instruction's tick.
                    if frame.exit_kind == 3 {
                        #[cfg(any(test, feature = "jit-profile"))]
                        {
                            self.stats.partial_repeats += u64::from(frame.repeat.completed != 0);
                        }
                        repeat = Some(frame.repeat);
                    }
                }
            }
            #[cfg(feature = "jit-profile")]
            let epoch_before = cpu.jit_mapping_epoch();
            #[cfg(feature = "jit-profile")]
            let eligible_before = cpu.jit_eligible_interpreted;
            #[cfg(feature = "jit-profile")]
            let modes_before = cpu.jit_interpreted_modes;
            let mut batch = if let Some(repeat) = repeat {
                #[cfg(feature = "jit-profile")]
                {
                    cpu.jit_profile_interpreted();
                }
                Interpreter::resume_repeat(cpu, bus, start.wrapping_add(ran), &repeat)
            } else if eligible {
                self.prepare_interpreter_fetch(cpu);
                if cold && !shadowed {
                    #[cfg(any(test, feature = "jit-profile"))]
                    {
                        self.stats.cold_batches += 1;
                    }
                    Interpreter::run_cold(
                        cpu,
                        bus,
                        start.wrapping_add(ran),
                        (budget - ran).min(COLD_BATCH),
                    )
                } else {
                    Interpreter::run_one(cpu, bus, start.wrapping_add(ran))
                }
            } else if cpu.interrupt_shadow() {
                // The ordinary batch truncates this state to one instruction.
                // Reuse its single-instruction kernel, retaining the forced
                // attention at shadow expiry, including an exact caller limit.
                // This also applies when the instruction clears IF; faults,
                // halt and explicit attention keep their own precise result.
                Interpreter::run_one(cpu, bus, start.wrapping_add(ran))
            } else {
                Interpreter::run_batch(cpu, bus, start.wrapping_add(ran), budget - ran)
            };
            if shadowed && matches!(batch.stop, BatchStop::Budget) {
                batch.stop = BatchStop::Attention {
                    instruction_step: start.wrapping_add(ran),
                    was_interruptible: false,
                };
            }
            ran += batch.ran;
            #[cfg(feature = "jit-profile")]
            {
                self.stats.write_misses_flushing_context +=
                    u64::from(write_token_miss && cpu.jit_mapping_epoch() != epoch_before);
            }
            self.stats.interpreted_steps += batch.ran;
            #[cfg(debug_assertions)]
            if !was_halted {
                let cause = if !eligible {
                    0
                } else if after_native {
                    2
                } else if cold {
                    1
                } else {
                    3
                };
                self.stats.interpreted_causes[cause] += batch.ran;
            }
            #[cfg(feature = "jit-profile")]
            {
                self.stats.eligible_interpreted_steps +=
                    cpu.jit_eligible_interpreted.wrapping_sub(eligible_before);
                for (index, before) in modes_before.into_iter().enumerate() {
                    self.stats.interpreted_modes[index] +=
                        cpu.jit_interpreted_modes[index].wrapping_sub(before);
                }
            }
            if !matches!(batch.stop, BatchStop::Budget) {
                return Ok(BatchResult {
                    ran,
                    stop: batch.stop,
                });
            }
        }
        Ok(BatchResult {
            ran,
            stop: BatchStop::Budget,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(debug_assertions)]
    #[test]
    fn session_profile_map_cap_preserves_totals_and_existing_sites() {
        let mut profile = ExitSites::default();
        for event in 1..=4100 * 8 {
            profile.record(
                0x13f,
                ((event - 1) / 8) as u32,
                ExitReason::MemoryGuard,
                event,
            );
        }
        for event in 4100 * 8 + 1..=4101 * 8 {
            profile.record(0x13f, 0, ExitReason::MemoryGuard, event);
        }
        for event in 4101 * 8 + 1..=4102 * 8 {
            profile.record(0x13f, 0, ExitReason::Budget, event); // Different key, beyond cap.
        }
        assert_eq!(profile.sites.len(), 4096);
        assert_eq!(profile.sites[&(0x13f, 0, ExitReason::MemoryGuard)], 2);
        assert_eq!(profile.reasons[ExitReason::MemoryGuard as usize], 4101 * 8);
        assert_eq!(profile.reasons[ExitReason::Budget as usize], 8);
        assert_eq!(
            profile.reasons.iter().sum::<u64>() - profile.sites.values().sum::<u64>(),
            4102 * 8 - 4097
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn session_profile_reason_priority() {
        use ExitReason::*;
        for (kind, reason) in [
            (1, MemoryGuard),
            (3, MemoryGuard),
            (4, Division),
            (7, CsLimit),
            (5, Attention),
            (6, Attention),
        ] {
            assert_eq!(ExitReason::boundary(kind, true, true), Some(reason));
            assert_eq!(ExitReason::boundary(kind, false, false), Some(reason));
        }
        assert_eq!(ExitReason::boundary(2, true, true), Some(Attention));
        assert_eq!(ExitReason::boundary(0, true, true), Some(Attention));
        assert_eq!(ExitReason::boundary(2, false, false), Some(Budget));
        assert_eq!(ExitReason::boundary(0, true, false), Some(Budget));
        assert_eq!(ExitReason::boundary(0, false, false), None);
    }

    #[test]
    fn jit_link_alias_hints_retire_both_planes_and_preserve_reused_slots() {
        let mut engine = Engine::with_limits(64 * 1024, 1).unwrap();
        let mut pages = super::super::validity::CodePages::new(8192);
        let (version, _) = pages.watch(0);
        let (other_version, _) = pages.watch(1);
        let ptr = pages.version_ptr(0);
        let other = pages.version_ptr(1);
        let entries = [0u32; PAGE_BYTES];
        let publish = |engine: &mut Engine, page, ptr, version| {
            engine.link(
                0x1000,
                page,
                version,
                ptr,
                entries.as_ptr(),
                false,
                TranslationRecheck::EMPTY,
                FetchCacheHit {
                    phys: PhysAddr((page * PAGE_BYTES) as u64),
                    backing: BackingToken((page * PAGE_BYTES) as u64),
                },
            );
        };
        publish(&mut engine, 0, ptr, version);
        engine.user_links = true;
        publish(&mut engine, 0, ptr, version);
        pages.write(0, 1);
        engine.retire_code_page(0, ptr);
        assert_eq!(engine.links[1].tag, 0);
        assert_eq!(engine.links[1 + SLOTS].tag, 0);

        let (version, _) = pages.watch(0);
        publish(&mut engine, 0, ptr, version);
        publish(&mut engine, 1, other, other_version); // Same slot now owns another page.
        let retained = engine.links[1 + SLOTS].tag;
        pages.write(0, 1);
        engine.retire_code_page(0, ptr);
        assert_eq!(engine.links[1 + SLOTS].tag, retained);
        assert_ne!(retained, 0);
        assert_eq!(engine.stamps[1 + SLOTS].version_ptr, other);
    }

    fn context_probe() -> (Engine, Cpu, *const u32) {
        context_probe_mode(false)
    }

    fn context_probe_mode(code16: bool) -> (Engine, Cpu, *const u32) {
        let mut engine = Engine::with_limits(64 * 1024, 1).unwrap();
        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(0x100, 0);
        if code16 {
            for seg in [SegmentReg::Cs, SegmentReg::Ss] {
                let mut cache = cpu.seg(seg);
                cache.flags &= !crate::x86::cpu::SEGMENT_FLAG_DEFAULT_BIG;
                cpu.set_seg_cache(seg, cache);
            }
        }
        engine.sync_mapping(&mut cpu);
        let table = engine.arena.allocate_entries();
        let entries = engine.arena.entries_pointer(table);
        let decoded = if code16 {
            decode::block_code16(&[0x40], 0x100, 1)
        } else {
            decode::block_extended(&[0x40], 0x100, 1)
        }; // INC AX/EAX
        let mut code = compile::block(&decoded, None, View::block_facts(&cpu)).unwrap();
        let handle = engine.arena.publish_block(&mut code).unwrap();
        let body = engine.arena.address(handle, 0).unwrap();
        engine.arena.entries_mut(table)[0x100] = (body as isize - entries as isize) as i32 as u32;
        engine.links[0] = Link {
            tag: engine.link_context(),
            entries,
        };
        (engine, cpu, entries)
    }

    // Enter the generated dispatcher directly: a hit here cannot be rescued
    // by Rust lookup, admission or interpretation. One INC then returns.
    fn dispatch_probe(engine: &mut Engine, cpu: &mut Cpu) -> Frame {
        let mut view = View::new(cpu, false);
        let mut frame = Frame::from_cpu(cpu, 1);
        frame.memory = core::ptr::from_mut(&mut view).cast();
        frame.native_entry = engine.entry.dispatch;
        frame.dispatch_entry = engine.entry.finish;
        frame.return_entry = engine.entry.finish;
        frame.recheck_entry = engine.entry.recheck;
        frame.links = engine.links.as_mut_ptr();
        frame.link_rechecks = engine.rechecks.as_ptr();
        frame.link_context = engine.link_context();
        // SAFETY: the frame, view, links and native table stay live throughout
        // this invocation; the only published body is the one-step probe.
        unsafe {
            engine
                .arena
                .invoke(engine.entry.handle, &mut frame)
                .unwrap()
        };
        frame
    }

    #[test]
    fn native_pm16_linked_body_checks_changed_cs_limit_and_mode_eligibility() {
        let (mut engine, mut cpu, _) = context_probe_mode(true);
        let epoch = cpu.jit_mapping_epoch();
        let tag = engine.links[0].tag;
        assert!(cpu.jit_code_eligible());
        assert_eq!(dispatch_probe(&mut engine, &mut cpu).completed, 1);
        let mut cs = cpu.seg(SegmentReg::Cs);
        cs.limit = 0xff;
        cpu.set_seg_cache(SegmentReg::Cs, cs);
        engine.sync_mapping(&mut cpu);
        assert_eq!(cpu.jit_mapping_epoch(), epoch);
        assert_eq!(engine.links[0].tag, tag);
        let frame = dispatch_probe(&mut engine, &mut cpu);
        assert_eq!((frame.completed, frame.eip, frame.exit_kind), (0, 0x100, 7));
        cs.limit = 0x100;
        cpu.set_seg_cache(SegmentReg::Cs, cs);
        engine.sync_mapping(&mut cpu);
        assert_eq!(dispatch_probe(&mut engine, &mut cpu).completed, 1);
        let mut ss = cpu.seg(SegmentReg::Ss);
        ss.flags |= crate::x86::cpu::SEGMENT_FLAG_DEFAULT_BIG;
        cpu.set_seg_cache(SegmentReg::Ss, ss);
        assert!(!cpu.jit_code_eligible());
        ss.flags &= !crate::x86::cpu::SEGMENT_FLAG_DEFAULT_BIG;
        cpu.set_seg_cache(SegmentReg::Ss, ss);
        cpu.cr0 &= !crate::x86::cpu::CR0_PE;
        assert!(!cpu.jit_code_eligible());
        cpu.cr0 |= crate::x86::cpu::CR0_PE;
        cpu.replace_eflags(1 << 17);
        assert!(!cpu.jit_code_eligible());
    }

    #[test]
    fn context_links_reject_changed_base_or_facts_and_reenter_without_lookup() {
        let (mut engine, mut cpu, _) = context_probe();
        let cs = cpu.seg(SegmentReg::Cs);
        let ds = cpu.seg(SegmentReg::Ds);
        let tag = engine.links[0].tag;
        let epoch = cpu.jit_mapping_epoch();
        let hit = dispatch_probe(&mut engine, &mut cpu);
        assert_eq!((hit.completed, hit.eip, hit.regs[0]), (1, 0x101, 1));

        // Same linear page and byte, different CS-relative instruction.
        let mut based_cs = cs;
        based_cs.base = 0x80;
        cpu.set_seg_cache(SegmentReg::Cs, based_cs);
        cpu.eip = 0x80;
        engine.sync_mapping(&mut cpu);
        assert_eq!(cpu.jit_mapping_epoch(), epoch);
        assert_eq!(engine.links[0].tag, tag);
        let miss = dispatch_probe(&mut engine, &mut cpu);
        assert_eq!(
            (miss.completed, miss.eip, miss.regs),
            (0, cpu.eip, cpu.regs)
        );
        cpu.set_seg_cache(SegmentReg::Cs, cs);
        cpu.eip = 0x100;
        engine.sync_mapping(&mut cpu);
        assert_eq!(engine.link_context(), tag);
        assert_eq!(dispatch_probe(&mut engine, &mut cpu).completed, 1);

        let mut limited_ds = ds;
        limited_ds.limit = 0x7fff;
        cpu.set_seg_cache(SegmentReg::Ds, limited_ds);
        engine.sync_mapping(&mut cpu);
        assert_ne!(engine.link_context(), tag);
        assert_eq!(engine.links[0].tag, tag);
        let miss = dispatch_probe(&mut engine, &mut cpu);
        assert_eq!(
            (miss.completed, miss.eip, miss.regs),
            (0, cpu.eip, cpu.regs)
        );
        cpu.set_seg_cache(SegmentReg::Ds, ds);
        engine.sync_mapping(&mut cpu);
        assert_eq!(engine.link_context(), tag);
        assert_eq!(dispatch_probe(&mut engine, &mut cpu).completed, 1);

        // CS limits stay dynamic; changing D selects another context. Neither
        // load expires the mapping epoch or discards the retained old link.
        let mut changed_cs = cs;
        changed_cs.limit = 0xffff;
        cpu.set_seg_cache(SegmentReg::Cs, changed_cs);
        assert_eq!(cpu.jit_mapping_epoch(), epoch);
        engine.sync_mapping(&mut cpu);
        assert_eq!(engine.links[0].tag, tag);
        assert!(!cpu.jit_code_eligible()); // A limited PM32 CS cannot enter.
        changed_cs.flags ^= crate::x86::cpu::SEGMENT_FLAG_DEFAULT_BIG;
        cpu.set_seg_cache(SegmentReg::Cs, changed_cs);
        assert_eq!(cpu.jit_mapping_epoch(), epoch);
        engine.sync_mapping(&mut cpu);
        assert_eq!(engine.links[0].tag, tag);
        assert_ne!(engine.link_context(), tag);
        assert_eq!(dispatch_probe(&mut engine, &mut cpu).completed, 0);
        assert!(!cpu.jit_code_eligible()); // PM16 on SS.B=1 is deferred.
    }

    #[test]
    fn context_overflow_clears_both_planes_before_reusing_an_id() {
        let (mut engine, mut cpu, entries) = context_probe();
        let cs = cpu.seg(SegmentReg::Cs);
        let original_tag = engine.links[0].tag;
        engine.links[SLOTS] = engine.links[0];
        engine.global_links[0] = 1;
        engine.global_links[SLOTS / 64] = 1;
        // The initial (0, 0) context and this flat context already occupy ids.
        for base in 1..(MAX_CONTEXTS - 1) as u32 {
            let mut changed = cs;
            changed.base = base;
            cpu.set_seg_cache(SegmentReg::Cs, changed);
            engine.sync_mapping(&mut cpu);
        }
        assert_eq!(engine.contexts.len(), MAX_CONTEXTS);
        assert_eq!(engine.links[0].tag, original_tag);
        assert_eq!(engine.stats.context_clears, 0);
        let mut changed = cs;
        changed.base = MAX_CONTEXTS as u32;
        cpu.set_seg_cache(SegmentReg::Cs, changed);
        engine.sync_mapping(&mut cpu);
        assert_eq!(engine.context_id, 1);
        assert_eq!(engine.stats.context_clears, 1);
        assert!(
            engine
                .links
                .iter()
                .all(|link| link.tag == 0 && link.entries.is_null())
        );
        assert!(engine.global_links.iter().all(|bits| *bits == 0));
        assert_eq!(dispatch_probe(&mut engine, &mut cpu).completed, 0);

        // Returning now reuses the old numerical id, but cannot reach the
        // former link until the retained page table is explicitly published.
        cpu.set_seg_cache(SegmentReg::Cs, cs);
        engine.sync_mapping(&mut cpu);
        assert_eq!(engine.link_context(), original_tag);
        assert_eq!(dispatch_probe(&mut engine, &mut cpu).completed, 0);
        engine.links[0] = Link {
            tag: engine.link_context(),
            entries,
        };
        assert_eq!(dispatch_probe(&mut engine, &mut cpu).completed, 1);
    }

    #[test]
    fn global_retag_keeps_each_context_id_and_epoch_wrap_expires_all_links() {
        let (mut engine, mut cpu, entries) = context_probe();
        let ds = cpu.seg(SegmentReg::Ds);
        cpu.cr4 |= crate::x86::cpu::CR4_PGE;
        let old_tag = engine.links[0].tag;
        let mut changed_ds = ds;
        changed_ds.base = 0x1000;
        cpu.set_seg_cache(SegmentReg::Ds, changed_ds);
        engine.sync_mapping(&mut cpu);
        let other_tag = engine.link_context() | 0x1000;
        assert_ne!(other_tag & 4095, old_tag & 4095);
        engine.links[SLOTS + 1] = Link {
            tag: other_tag,
            entries,
        };
        engine.links[2] = Link {
            tag: engine.link_context() | 0x2000,
            entries,
        };
        let nonglobal = engine.links[2].tag;
        engine.global_links[0] = 1;
        engine.global_links[SLOTS / 64] = 2;

        cpu.invalidate_non_global_translations();
        engine.sync_mapping(&mut cpu);
        let epoch = u64::from(engine.link_epoch) << 32;
        assert_eq!(engine.links[0].tag, epoch | (old_tag & 0xffff_ffff));
        assert_eq!(
            engine.links[SLOTS + 1].tag,
            epoch | (other_tag & 0xffff_ffff)
        );
        assert_eq!(engine.links[2].tag, nonglobal);
        assert_eq!(engine.stats.global_link_retags, 2);
        cpu.set_seg_cache(SegmentReg::Ds, ds);
        engine.sync_mapping(&mut cpu);
        assert_eq!(dispatch_probe(&mut engine, &mut cpu).completed, 1);

        let id = engine.context_id;
        engine.link_epoch = u32::MAX;
        cpu.invalidate_non_global_translations();
        engine.sync_mapping(&mut cpu);
        assert_eq!(engine.link_epoch, 1);
        assert_eq!(engine.context_id, id);
        assert!(engine.links.iter().all(|link| link.tag == 0));
        assert_eq!(dispatch_probe(&mut engine, &mut cpu).completed, 0);
    }

    #[test]
    fn heat_word_admits_at_threshold_and_restarts_on_tag_replacement() {
        assert_eq!(core::mem::size_of::<Heat>() * HEAT_SLOTS, 1024 * 1024);
        let (_, tag) = Heat::key(0x1234, 0x5678, 0);
        let (_, rewritten) = Heat::key(0x1234, 0x5678, 1);
        let mut heat = Heat::default();
        for visit in 1..=HOT {
            assert_eq!(heat.visit(tag), visit == HOT);
        }
        assert!(heat.visit(tag));
        assert!(!heat.visit(rewritten));
        for visit in 2..=HOT {
            assert_eq!(heat.visit(rewritten), visit == HOT);
        }
        assert!(!heat.visit(tag));
    }

    #[test]
    fn compact_page_promotion_preserves_negative_entries() {
        let mut arena = CodeArena::with_limits(64 * 1024, 1).unwrap();
        let mut page = CompiledPage::new(1, core::ptr::null(), &mut arena);
        for offset in 0..PAGE_BYTES {
            page.entries.set(&mut arena, offset, offset as u32 & 1);
        }
        assert_eq!(arena.tables_used(), 0);
        assert!(page.entries.native().is_none());
        let table = page.entries.promote(&mut arena);
        assert_eq!(arena.tables_used(), 1);
        let pointer = arena.entries_pointer(table);
        for offset in 0..PAGE_BYTES {
            // Both aliases expose the same negative entries.
            let expected = offset as u32 & 1;
            assert_eq!(page.entries.get(&arena, offset), expected);
            assert_eq!(unsafe { pointer.add(offset).read() }, expected);
        }
        page.invalidate(2, &mut arena);
        assert!(arena.entries(table).iter().all(|&entry| entry == 0));
        assert_eq!(arena.tables_used(), 1);
        let mut cold = CompiledPage::new(1, core::ptr::null(), &mut arena);
        cold.entries.set(&mut arena, 4095, INTERPRET);
        cold.invalidate(2, &mut arena);
        assert_eq!(cold.entries.get(&arena, 4095), 0);
        assert_eq!(cold.content.end, 0);
        assert_eq!(cold.restart, 1);
        assert_eq!(arena.tables_used(), 1);
    }
}
