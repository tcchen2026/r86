//! Thumb-2 block dispatch for the shared Machine.
//!
//! A block is keyed by its EIP, the backing byte from the current execute
//! translation and the segment facts it was compiled under. It may run
//! while that translation exists and its CodePages version is unchanged; the
//! interpreter's fetch path owns every page walk and fetch fault. After a
//! version change the page is watched again, which revokes writable aliases,
//! and the block is kept if its own source bytes are unchanged: code and data
//! often share pages in firmware and drivers. Blocks live until the arena is
//! reset. The block index is a fixed table ([`super::store`]); a full set
//! drops one record, which only forgets that block.
//!
//! Only control-transfer targets and instructions after an interpreter handoff
//! are entries; straight-line cold code is interpreted up to the next transfer.
//! A direct-mapped counter admits an entry on its sixteenth visit: with a
//! 4 MiB arena, compiling lukewarm boot code thrashes the cache (measured on
//! the TinyCore boot: admission at three visits compiled each block ~8 times).
//!
//! PM32 blocks chain through [eip, code] pairs, one plane per privilege.
//! User PM16 has a separate [linear, code] plane and checks the incoming IP
//! and live CS limit at entry. An entry is
//! inserted only after the full lookup above succeeds. Native code cannot
//! change mappings, CPL, CS, segment facts or watched code, so the table stays
//! valid for a whole invocation. PM32 fact/base changes clear only its planes;
//! PM16 needs no flat facts and its dispatcher adds the live CS base. Global
//! mapping changes clear all planes; the runtime drops non-global entries
//! after a non-global flush, and an INVLPG range's or a retired page's entries.
//!
//! Each block reserves its length from the invocation budget, which ends at
//! the next machine event; a precise exit hands its instruction to the
//! interpreter.
//!
//! A chain that returns at a dispatch miss with budget left resumes from the
//! same frame when the target already has a valid record. Native code cannot
//! change mappings, CPL, CS, segment facts or watched code, so the context
//! `sync` checked still holds. Every code address handed to native code has
//! bit 0 set (Thumb state).

use super::arena::Arena;
use super::lower::{
    self, CODE16, EXIT_ATTENTION, EXIT_BOUNDARY, EXIT_INTERPRET, EXIT_SHADOW, Frame, STACK_FACT,
    STACK32_FACT, TABLE_BITS,
};
use super::store::{Record, Store};
use crate::machine::HostBus;
use crate::x86::cpu::{CR0_PE, SEGMENT_FLAG_DEFAULT_BIG, SegmentReg};
use crate::x86::jit::decode;
use crate::x86::{BackingToken, BatchResult, BatchStop, Bus, Cpu, Interpreter, LinearAddr};
#[cfg(feature = "jit-profile")]
use alloc::collections::BTreeMap;
use alloc::{boxed::Box, format, string::String, vec::Vec};
#[cfg(feature = "jit-profile")]
use profile::{Iteration, NativeReturn, TableCause, Target};

#[cfg(feature = "jit-profile")]
#[path = "profile_code16.rs"]
mod code16;

// Diagnostic state and all its callers disappear without jit-profile.
#[cfg(feature = "jit-profile")]
mod profile {
    use super::{Cpu, SegmentReg};
    use crate::x86::cpu::{CR0_PE, CR0_PG, CR0_WP, SegmentCache};
    use alloc::{collections::BTreeMap, string::String, vec::Vec};
    use core::fmt::Write;

    // Ineligible causes are exclusive, in the existing guard's check order.
    pub enum Iteration {
        Shadow,
        Halted,
        Code16,
        CsLimit,
        TfVm,
        Sequential,
        Native,
        Interpret,
        ColdNew,
        ColdCounting,
    }
    pub enum NativeReturn {
        InterpretGuard,
        BudgetExhausted,
        DispatchResolved,
        NextBlockOverLimit,
        UnsupportedEntry,
        FetchMiss,
        InvalidatedCode,
        TargetUncompiled,
        Attention,
        Shadow,
        Other,
    }
    pub enum TableCause {
        ArenaReset,
        CodeEpoch,
        CodePage,
        Cr3Changed,
        Cr3NonGlobal,
        Cr0OrCs,
        Invlpg,
        FactsChanged,
        GlobalEpochChanged,
        OtherContext,
        InitialSync,
    }
    #[derive(Default)]
    pub enum Target {
        #[default]
        FetchMiss,
        Uncompiled,
        Invalidated,
        Unsupported,
        Native,
    }

    #[derive(Clone, Copy)]
    pub struct CpuContext {
        cr0_mapping: u32,
        cr3: u32,
        cs: SegmentCache,
        facts: u32,
        global: u64,
    }
    impl CpuContext {
        pub fn capture(cpu: &Cpu, facts: u32, global: u64) -> Self {
            Self {
                cr0_mapping: cpu.cr0 & (CR0_PE | CR0_PG | CR0_WP),
                cr3: cpu.cr3,
                cs: cpu.seg(SegmentReg::Cs),
                facts,
                global,
            }
        }
    }

    #[derive(Default)]
    pub struct Counters {
        pub code16: super::code16::Counters,
        pub ip_checks: u64,
        pub pm16_native_steps: u64,
        pub iterations: [u64; 10],
        pub native_returns: [u64; 11],
        pub native_steps: [u64; 8],
        pub invoke_limits: [u64; 5],
        pub table_clears: [u64; 11],
        pub table_retirements: [u64; 11],
        pub entry_rejected: u64,
        pub attention_limits: u64,
        pub attention_ends: u64,
        pub returned_steps: u64,
        pub control_attention_returns: u64,
        pub shadow_continues: u64,
        pub attention_continues: u64,
        pub address16_unclassified: u64,
        pub mov_nonflat: [u64; 7],
        pub invocation_start: u64,
        pub checked_segments: BTreeMap<(u16, u32, u32, bool, u8), [u64; 11]>,
        pub checked_pages: BTreeMap<u32, u64>,
        pub target: Target,
        pub context: Option<CpuContext>,
    }
    impl Counters {
        pub fn mov_segment(&mut self, cpu: &Cpu, key: u8) {
            let segment = if key & 7 == SegmentReg::Ds as u8 + 1 {
                SegmentReg::Ds
            } else {
                SegmentReg::Ss
            };
            let cache = cpu.seg(segment);
            let access = cache.flags & 0xff;
            // Rejecting descriptor kinds take precedence over geometry, so
            // base/limit counts do not include unusable or read-only writes.
            let cause = if cache.selector & !3 == 0 {
                0
            } else if access & 0x18 == 0x18 {
                1
            } else if access & 0x1c == 0x14 {
                2
            } else if key & 8 != 0 && access & 2 == 0 {
                3
            } else if cache.base != 0 {
                4
            } else if cache.limit != u32::MAX {
                5
            } else {
                6
            };
            self.mov_nonflat[cause] += 1;
        }

        pub fn limit(&mut self, limit: u64) {
            let bucket = match limit {
                0 => 0,
                1 => 1,
                2..=15 => 2,
                16..=255 => 3,
                _ => 4,
            };
            self.invoke_limits[bucket] += 1;
        }

        // One sample for each actual native entry, including a resolved
        // dispatch miss which resumes inside the same Rust invoke call.
        pub fn steps(&mut self, steps: u32) {
            let bucket = match steps {
                0 => 0,
                1 => 1,
                2..=3 => 2,
                4..=7 => 3,
                8..=15 => 4,
                16..=31 => 5,
                32..=63 => 6,
                _ => 7,
            };
            self.native_steps[bucket] += 1;
            self.returned_steps += u64::from(steps);
        }

        pub fn unresolved(&mut self) {
            let reason = match self.target {
                Target::Native => NativeReturn::NextBlockOverLimit,
                Target::FetchMiss => NativeReturn::FetchMiss,
                Target::Uncompiled => NativeReturn::TargetUncompiled,
                Target::Invalidated => NativeReturn::InvalidatedCode,
                Target::Unsupported => NativeReturn::UnsupportedEntry,
            };
            self.native_returns[reason as usize] += 1;
        }

        // Attribution is by observations at sync, not an instruction trace.
        // CR0/CS and CR3 changes take precedence. Among the remaining clears,
        // changed facts take precedence over a simultaneous global epoch change.
        pub fn context_cause(&self, next: CpuContext) -> TableCause {
            match self.context {
                None => TableCause::InitialSync,
                Some(old) if old.cr0_mapping != next.cr0_mapping || old.cs != next.cs => {
                    TableCause::Cr0OrCs
                }
                Some(old) if old.cr3 != next.cr3 => TableCause::Cr3Changed,
                Some(old) if old.facts != next.facts => TableCause::FactsChanged,
                Some(old) if old.global != next.global => TableCause::GlobalEpochChanged,
                Some(_) => TableCause::OtherContext,
            }
        }

        pub fn report(&self, out: &mut String) {
            fn row(out: &mut String, name: &str, names: &[&str], counts: &[u64]) {
                let _ = write!(
                    out,
                    "t32 profile {name}: total={}",
                    counts.iter().sum::<u64>()
                );
                for (label, count) in names.iter().zip(counts) {
                    let _ = write!(out, " {label}={count}");
                }
                out.push('\n');
            }
            row(
                out,
                "iterations",
                &[
                    "shadow",
                    "halted",
                    "code16",
                    "cs_limit",
                    "tf_vm",
                    "sequential",
                    "native",
                    "interpret",
                    "cold_new",
                    "cold_counting",
                ],
                &self.iterations,
            );
            row(
                out,
                "native_returns",
                &[
                    "interpret_guard",
                    "budget_exhausted",
                    "dispatch_resolved",
                    "next_block_over_limit",
                    "unsupported_entry",
                    "fetch_miss",
                    "invalidated_code",
                    "target_uncompiled",
                    "attention",
                    "shadow",
                    "other",
                ],
                &self.native_returns,
            );
            row(
                out,
                "native_steps",
                &["0", "1", "2-3", "4-7", "8-15", "16-31", "32-63", "64+"],
                &self.native_steps,
            );
            row(
                out,
                "invoke_limits",
                &["0", "1", "2-15", "16-255", "256+"],
                &self.invoke_limits,
            );
            let causes = [
                "arena_reset",
                "code_epoch",
                "code_page",
                "cr3_changed",
                "cr3_nonglobal",
                "cr0_or_cs",
                "invlpg",
                "facts_changed",
                "global_epoch_changed",
                "other_context",
                "initial_sync",
            ];
            row(out, "table_clears", &causes, &self.table_clears);
            row(out, "table_retirements", &causes, &self.table_retirements);
            row(
                out,
                "mov_nonflat",
                &[
                    "null",
                    "code",
                    "expand_down",
                    "read_only",
                    "base",
                    "limit",
                    "other",
                ],
                &self.mov_nonflat,
            );
            let mut segments: Vec<_> = self.checked_segments.iter().collect();
            segments.sort_by_key(|(_, counts)| core::cmp::Reverse(counts[0]));
            // Keep cumulative diagnostic rows complete so two snapshots can
            // be subtracted before ranking a workload phase on the host.
            for ((selector, base, limit, big, cpl), counts) in segments {
                let _ = writeln!(
                    out,
                    "t32 checked segment: selector={selector:04x} base={base:08x} limit={limit:08x} big={big} cpl={cpl} count={} windows={:?}",
                    counts[0],
                    &counts[1..]
                );
            }
            let mut pages: Vec<_> = self.checked_pages.iter().collect();
            pages.sort_by_key(|(_, count)| core::cmp::Reverse(**count));
            for (page, count) in pages {
                let _ = writeln!(out, "t32 checked page: eip={page:08x} count={count}");
            }
            let _ = writeln!(
                out,
                "t32 profile invoke: entry_rejected={} attention_limits={} attention_ends={} returned_steps={}",
                self.entry_rejected,
                self.attention_limits,
                self.attention_ends,
                self.returned_steps
            );
            let _ = writeln!(
                out,
                "t32 profile controls: attention_returns={} shadow_continues={} attention_continues={}",
                self.control_attention_returns, self.shadow_continues, self.attention_continues,
            );
            let _ = writeln!(
                out,
                "t32 profile entry_decode: address16_unclassified={}",
                self.address16_unclassified
            );
        }
    }
}

/// One diagnostic invocation; native code cannot change cached segments or CPL.
#[cfg(feature = "jit-profile")]
struct CheckedInvocation {
    counters: *mut profile::Counters,
    segments: [crate::x86::cpu::SegmentCache; 5],
    cpl: u8,
    start: u64,
    budget: u32,
}

/// Called only by diagnostic generated code with its invocation-owned context.
#[cfg(feature = "jit-profile")]
pub(super) unsafe extern "C" fn profile_checked(
    context: usize,
    segment: u32,
    eip: u32,
    remaining: u32,
) {
    // SAFETY: invoke keeps the context and counters alive until native return;
    // generated code preserves the AAPCS registers and never calls concurrently.
    let invocation = unsafe { &mut *(context as *mut CheckedInvocation) };
    let counters = unsafe { &mut *invocation.counters };
    let cache = invocation.segments[segment as usize];
    let key = (
        cache.selector,
        cache.base,
        cache.limit,
        cache.flags & crate::x86::cpu::SEGMENT_FLAG_DEFAULT_BIG != 0,
        invocation.cpl,
    );
    let counts = counters.checked_segments.entry(key).or_default();
    counts[0] += 1;
    let step = invocation.start + u64::from(invocation.budget - remaining);
    if let Some(count) = counts.get_mut(1 + (step / 25_000_000) as usize) {
        *count += 1;
    }
    *counters.checked_pages.entry(eip & !0xfff).or_default() += 1;
}

const HEAT: usize = 4096;
const HOT: u8 = 16;
/// Records in the block index: four-way sets. A 4 MiB arena holds about ten
/// thousand blocks. On the TinyCore boot 16384 records replaced 21,324 live
/// records in full sets and 32768 replaced 3,216.
const BLOCKS: usize = 32768;
const BLOCK_INSTRUCTIONS: usize = 32;
const WINDOW: usize = 480;
const TF: u32 = 1 << 8;
const VM: u32 = 1 << 17;
/// DS/ES/SS flat read and write proofs, plus the 32-bit stack proof.
const FACTS: u32 = 0b1101 | 0b1101 << 8 | STACK_FACT | STACK32_FACT;
const PLANE: usize = 1 << TABLE_BITS;

#[derive(Debug, Default)]
pub struct Stats {
    pub native_steps: u64,
    pub interpreted_steps: u64,
    pub invocations: u64,
    pub memory_exits: u64,
    pub compiled_blocks: u64,
    pub unsupported_entries: u64,
    pub invalidations: u64,
    pub revalidations: u64,
    pub table_clears: u64,
    pub table_retirements: u64,
    pub arena_clears: u64,
    /// Dispatch misses resumed without leaving `invoke`.
    pub resolved: u64,
    /// Live records replaced in a full set of the block index.
    pub store_evictions: u64,
    #[cfg(all(feature = "jit-profile", target_os = "linux"))]
    pub compile_nanoseconds: u128,
}

/// What the PM32 dispatch planes were filled under.
#[derive(Clone, Copy, Eq, PartialEq)]
struct Context {
    global: u64,
    facts: u32,
    cs_base: u32,
}

const NO_SLOT: u16 = u16::MAX;
const NO_PAGE: u32 = u32::MAX;

/// One active dispatch slot's backing-page list. `version` also marks
/// membership in `filled`: page retirement leaves that membership intact,
/// so republishing an empty slot cannot append a duplicate filled index.
#[derive(Clone, Copy)]
struct PageSlot {
    linear: u32, // Page plus bit 0 for a global translation.
    version: *const u64,
    ram_page: u32,
    previous: u16,
    next: u16,
}

impl PageSlot {
    const EMPTY: Self = Self {
        linear: 0,
        version: core::ptr::null(),
        ram_page: NO_PAGE,
        previous: NO_SLOT,
        next: NO_SLOT,
    };

    fn unlink(slots: &mut [Self], heads: &mut [u16], slot: usize) {
        let old = slots[slot];
        if old.ram_page == NO_PAGE {
            return;
        }
        if old.previous == NO_SLOT {
            heads[old.ram_page as usize] = old.next;
        } else {
            slots[usize::from(old.previous)].next = old.next;
        }
        if old.next != NO_SLOT {
            slots[usize::from(old.next)].previous = old.previous;
        }
        slots[slot].ram_page = NO_PAGE;
    }
}

pub struct Engine {
    arena: Arena,
    /// Shared stubs, as Thumb addresses (bit 0 set).
    entry: usize,
    dispatch: usize,
    entry16: usize,
    dispatch16: usize,
    miss: u32,
    shared_bytes: usize,
    store: Store,
    /// Visits of not yet admitted entries: (EIP, count), direct-mapped.
    heat: Box<[(u32, u8)]>,
    /// PM32 supervisor/user [eip, code], then PM16 user [linear, code].
    table: Box<[[u32; 2]]>,
    /// Table indices filled since the last clear.
    filled: Vec<u16>,
    filled16: Vec<u16>,
    pages: Box<[PageSlot]>,
    /// Intrusive lists of active slots, indexed by backing RAM page. All
    /// three planes share these lists; slot reuse unlinks the previous page.
    page_heads: Vec<u16>,
    context: Option<Context>,
    global: u64,
    mapping: u64,
    code: u64,
    pub stats: Stats,
    /// Interpreted entries by opcode; see `profile_key`.
    #[cfg(feature = "jit-profile")]
    unsupported: Box<[u64; 2048]>,
    #[cfg(feature = "jit-profile")]
    unsupported_memory: BTreeMap<(u16, u8), u64>,
    #[cfg(feature = "jit-profile")]
    profile: profile::Counters,
}

/// A valid native block.
#[derive(Clone, Copy)]
struct Native {
    /// Thumb address (bit 0 set).
    address: usize,
    instructions: u32,
    /// CodePages version cell of its page.
    version: *const u64,
    ram_page: usize,
}

enum Lookup {
    Native(Native),
    /// The interpreter executes this instruction; the next may be native.
    Interpret,
    /// Not yet admitted. True when this address was not being counted.
    Cold(bool),
}

/// What the block index holds for the current EIP.
enum Probe {
    Known(Lookup),
    /// No record. The execute translation names this backing byte.
    Unknown(BackingToken),
}

/// Generated entry: saves AAPCS callee registers, loads guest GPRs from the
/// frame and jumps to the block. The chain returns to the caller itself.
type Invoke = unsafe extern "C" fn(*mut Frame, usize);

impl Engine {
    pub fn new() -> Result<Self, String> {
        // The 3DS has little memory; this is the whole translation cache.
        Self::with_capacity(4 * 1024 * 1024)
    }

    pub(crate) fn with_capacity(bytes: usize) -> Result<Self, String> {
        let mut arena = Arena::new(bytes)?;
        let shared = lower::shared();
        let base = arena.publish(&shared.code).ok_or("JIT arena too small")?;
        let miss = (base + shared.miss * 2) as u32 | 1;
        Ok(Self {
            shared_bytes: arena.used_bytes(),
            arena,
            entry: base | 1,
            dispatch: (base + shared.dispatch * 2) | 1,
            entry16: (base + shared.entry16 * 2) | 1,
            dispatch16: (base + shared.dispatch16 * 2) | 1,
            miss,
            store: Store::new(BLOCKS),
            heat: alloc::vec![(0, 0); HEAT].into_boxed_slice(),
            table: alloc::vec![[0, miss]; 3 * PLANE].into_boxed_slice(),
            filled: Vec::new(),
            filled16: Vec::new(),
            pages: alloc::vec![PageSlot::EMPTY; 3 * PLANE].into_boxed_slice(),
            page_heads: Vec::new(),
            context: None,
            global: 0,
            mapping: 0,
            code: 0,
            stats: Stats::default(),
            #[cfg(feature = "jit-profile")]
            unsupported: Box::new([0; 2048]),
            #[cfg(feature = "jit-profile")]
            unsupported_memory: BTreeMap::new(),
            #[cfg(feature = "jit-profile")]
            profile: profile::Counters::default(),
        })
    }

    pub(crate) fn clear(&mut self, bus: &mut HostBus) -> Result<(), String> {
        self.store.clear();
        #[cfg(feature = "jit-profile")]
        {
            self.profile.table_clears[TableCause::ArenaReset as usize] += 1;
        }
        self.clear_table(true);
        self.arena.reset(self.shared_bytes);
        // No block can observe an old version after this point.
        bus.jit_release_code_pages();
        self.stats.arena_clears += 1;
        Ok(())
    }

    fn clear_table(&mut self, all: bool) {
        if self.filled.len() > PLANE / 8 {
            self.table[..2 * PLANE].fill([0, self.miss]);
        } else {
            for &index in &self.filled {
                self.table[usize::from(index)] = [0, self.miss];
            }
        }
        for &slot in &self.filled {
            let slot = usize::from(slot);
            PageSlot::unlink(&mut self.pages, &mut self.page_heads, slot);
            self.pages[slot] = PageSlot::EMPTY;
        }
        self.filled.clear();
        if all {
            for index in self.filled16.drain(..) {
                let slot = 2 * PLANE + usize::from(index);
                self.table[slot] = [0, self.miss];
                PageSlot::unlink(&mut self.pages, &mut self.page_heads, slot);
                self.pages[slot] = PageSlot::EMPTY;
            }
        }
        self.stats.table_clears += 1;
    }

    pub fn report(&self) -> String {
        #[allow(unused_mut)]
        let mut report = format!(
            "t32 jit: {:?}, {} blocks, code {} KiB\n",
            self.stats,
            self.store.live(),
            self.arena.used_bytes() / 1024
        );
        #[cfg(feature = "jit-profile")]
        {
            if code16::ENABLED {
                self.profile.code16.report(&mut report);
            }
            self.profile.report(&mut report);
            report += &format!(
                "t32 profile pm16: native_steps={} ip_checks={} unchanged_page_rewatches={} arena_high_water_bytes={}\n",
                self.profile.pm16_native_steps,
                self.profile.ip_checks,
                self.stats.revalidations,
                self.arena.high_water_bytes(),
            );
            let mut top: Vec<_> = (0..2048).filter(|&i| self.unsupported[i] != 0).collect();
            top.sort_by_key(|&i| core::cmp::Reverse(self.unsupported[i]));
            for i in top {
                report += &format!(
                    "interpreted entry {}: {}\n",
                    profile_name(i as u16),
                    self.unsupported[i]
                );
            }
            let mut memory: Vec<_> = self.unsupported_memory.iter().collect();
            memory.sort_by_key(|(_, count)| core::cmp::Reverse(**count));
            for (&(opcode, key), count) in memory {
                let segment = ["ES", "CS", "SS", "DS", "FS", "GS"][usize::from((key & 7) - 1)];
                let missing = ["none", "flat_read", "flat_write", "CS"][usize::from(key >> 4 & 3)];
                report += &format!(
                    "interpreted memory entry {}: segment={segment} access={} missing={missing} addr16={} count={count}\n",
                    profile_name(opcode),
                    if key & 8 != 0 { "write" } else { "read" },
                    key & 64 != 0,
                );
            }
        }
        report
    }

    fn facts(cpu: &Cpu) -> u32 {
        if cpu.code_default16() {
            return CODE16;
        }
        let (flat, flat_write) = cpu.jit_segment_masks();
        let stack32 = if cpu.seg(SegmentReg::Ss).flags & SEGMENT_FLAG_DEFAULT_BIG != 0 {
            STACK32_FACT
        } else {
            0
        };
        (flat | flat_write << 8 | stack32) & FACTS
    }

    /// Remove the table entries that `retire` selects by page and version.
    fn retire_table(&mut self, retire: impl Fn(u32, *const u64) -> bool) {
        let (table, pages, heads, miss) = (
            &mut self.table,
            &mut self.pages,
            &mut self.page_heads,
            self.miss,
        );
        self.filled.retain(|&slot| {
            let slot = usize::from(slot);
            let keep = !retire(pages[slot].linear, pages[slot].version);
            if !keep {
                table[slot] = [0, miss];
                PageSlot::unlink(pages, heads, slot);
                pages[slot] = PageSlot::EMPTY;
            }
            keep
        });
        self.filled16.retain(|&index| {
            let slot = 2 * PLANE + usize::from(index);
            let keep = !retire(pages[slot].linear, pages[slot].version);
            if !keep {
                table[slot] = [0, miss];
                PageSlot::unlink(pages, heads, slot);
                pages[slot] = PageSlot::EMPTY;
            }
            keep
        });
        self.stats.table_retirements += 1;
    }

    /// A single backing-page retirement visits only its active slots.
    fn retire_code_page(&mut self, ram_page: usize, version: *const u64) {
        let mut next = self
            .page_heads
            .get_mut(ram_page)
            .map(|head| core::mem::replace(head, NO_SLOT))
            .unwrap_or(NO_SLOT);
        while next != NO_SLOT {
            let slot = usize::from(next);
            debug_assert_eq!(self.pages[slot].version, version);
            next = self.pages[slot].next;
            self.table[slot] = [0, self.miss];
            self.pages[slot].ram_page = NO_PAGE;
        }
        self.stats.table_retirements += 1;
    }

    /// Retire every table entry whose qualification may have changed.
    /// A non-global flush (CR3 reload with PGE) keeps global code, INVLPG
    /// removes only its range, and a single retired page only its own blocks;
    /// blocks never cross a page.
    fn sync(&mut self, cpu: &mut Cpu, bus: &HostBus) {
        let context = Context {
            global: cpu.jit_global_epoch(),
            facts: Self::facts(cpu),
            cs_base: cpu.seg(SegmentReg::Cs).base,
        };
        #[cfg(feature = "jit-profile")]
        let observed = profile::CpuContext::capture(cpu, context.facts, context.global);
        let code = bus.jit_code_epoch();
        if code != self.code {
            match bus.jit_single_retired_page(self.code) {
                Some((ram_page, version)) => {
                    #[cfg(feature = "jit-profile")]
                    {
                        self.profile.table_retirements[TableCause::CodePage as usize] += 1;
                    }
                    self.retire_code_page(ram_page, version)
                }
                None => {
                    #[cfg(feature = "jit-profile")]
                    {
                        self.profile.table_clears[TableCause::CodeEpoch as usize] += 1;
                    }
                    self.clear_table(true)
                }
            }
            self.code = code;
        }
        let mapping = cpu.jit_mapping_epoch();
        let invalidated = cpu.take_jit_invalidated_range();
        let global_changed = self.global != context.global;
        let context_changed = !cpu.code_default16() && self.context != Some(context);
        if global_changed || context_changed {
            #[cfg(feature = "jit-profile")]
            {
                let cause = self.profile.context_cause(observed);
                self.profile.table_clears[cause as usize] += 1;
            }
            self.clear_table(global_changed);
        }
        if global_changed {
            self.context = None;
        }
        if !cpu.code_default16() {
            self.context = Some(context);
        }
        self.global = context.global;
        // PM16 may survive a PM32 context clear, but never a mapping change.
        if !global_changed {
            if self.mapping != mapping {
                #[cfg(feature = "jit-profile")]
                {
                    self.profile.table_retirements[TableCause::Cr3NonGlobal as usize] += 1;
                }
                self.retire_table(|page, _| page & 1 == 0);
            }
            if let Some((base, mask)) = invalidated {
                #[cfg(feature = "jit-profile")]
                {
                    self.profile.table_retirements[TableCause::Invlpg as usize] += 1;
                }
                self.retire_table(|page, _| page & mask == base);
            }
        }
        self.mapping = mapping;
        #[cfg(feature = "jit-profile")]
        {
            self.profile.context = Some(observed);
        }
    }

    fn insert(&mut self, record: Record) {
        if self.store.insert(record) {
            self.stats.store_evictions += 1;
        }
    }

    /// The record for the current EIP, revalidated against its page version.
    /// `profile` counts an interpreted entry.
    #[cfg_attr(not(feature = "jit-profile"), allow(unused_variables))]
    fn probe(&mut self, cpu: &mut Cpu, bus: &mut HostBus, profile: bool) -> Probe {
        let linear = cpu.seg(SegmentReg::Cs).base.wrapping_add(cpu.eip);
        // A miss stays an architectural fetch for the interpreter.
        let Some(hit) = cpu.fetch_cache_lookup(LinearAddr(linear)) else {
            #[cfg(feature = "jit-profile")]
            {
                self.profile.target = Target::FetchMiss;
            }
            return Probe::Known(Lookup::Interpret);
        };
        let Some(slot) = self.store.find(cpu.eip, Self::facts(cpu), hit.backing.0) else {
            #[cfg(feature = "jit-profile")]
            {
                self.profile.target = Target::Uncompiled;
            }
            return Probe::Unknown(hit.backing);
        };
        let mut record = self.store.get(slot);
        // SAFETY: CodePages owns a stable cell until clear() discards all
        // records; it changes only outside native invocations.
        if unsafe { *record.version_ptr } != record.version {
            // Promotion revokes writable aliases before the bytes are
            // compared, so they cannot change again before execution.
            let (offset, version, version_ptr) = bus.jit_code_stamp(cpu, hit.backing);
            let len = record.source_len as usize;
            // SAFETY: a native record's source is `len` bytes in the arena,
            // which is reset only together with the store.
            if record.address == 0
                || bus.jit_code_bytes(offset, len)
                    != unsafe { core::slice::from_raw_parts(record.source, len) }
            {
                self.stats.invalidations += 1;
                self.store.remove(slot);
                #[cfg(feature = "jit-profile")]
                {
                    self.profile.target = Target::Invalidated;
                }
                return Probe::Known(Lookup::Cold(false));
            }
            self.store.revalidate(slot, version, version_ptr);
            record.version_ptr = version_ptr;
            self.stats.revalidations += 1;
        }
        if record.address == 0 {
            #[cfg(feature = "jit-profile")]
            {
                self.profile.target = Target::Unsupported;
            }
            #[cfg(feature = "jit-profile")]
            if profile {
                self.unsupported[usize::from(record.opcode)] += 1;
                if record.memory & 7 != 0 {
                    *self
                        .unsupported_memory
                        .entry((record.opcode, record.memory))
                        .or_default() += 1;
                    if matches!(record.opcode & 0x3ff, 0x88..=0x8b)
                        && matches!(record.memory & 7, 3 | 4)
                        && matches!(record.memory >> 4 & 3, 1 | 2)
                    {
                        self.profile.mov_segment(cpu, record.memory);
                    }
                } else if record.memory & 64 != 0 {
                    self.profile.address16_unclassified += 1;
                }
            }
            return Probe::Known(Lookup::Interpret);
        }
        #[cfg(feature = "jit-profile")]
        {
            self.profile.target = Target::Native;
        }
        Probe::Known(Lookup::Native(Native {
            address: record.address as usize,
            instructions: record.instructions,
            version: record.version_ptr,
            ram_page: (bus.jit_ram_offset(hit.backing) >> 12) as usize,
        }))
    }

    /// Classify the current EIP, admitting or compiling it when hot.
    fn lookup(&mut self, cpu: &mut Cpu, bus: &mut HostBus) -> Result<Lookup, String> {
        let backing = match self.probe(cpu, bus, true) {
            Probe::Known(found) => return Ok(found),
            Probe::Unknown(backing) => backing,
        };
        let linear = cpu.seg(SegmentReg::Cs).base.wrapping_add(cpu.eip);
        let facts = Self::facts(cpu);
        let heat = &mut self.heat
            [(cpu.eip ^ (cpu.eip >> 12) ^ (backing.0 as u32 >> 7)) as usize & (HEAT - 1)];
        if heat.0 != cpu.eip {
            *heat = (cpu.eip, 1);
            return Ok(Lookup::Cold(true));
        }
        heat.1 += 1;
        if heat.1 < HOT {
            return Ok(Lookup::Cold(false));
        }
        *heat = (0, 0);
        #[cfg(all(feature = "jit-profile", target_os = "linux"))]
        let compilation_start = std::time::Instant::now();
        // Promotion revokes writable aliases before any native execution.
        let (offset, mut version, mut version_ptr) = bus.jit_code_stamp(cpu, backing);
        let window = (4096 - (linear as usize & 4095))
            .min(WINDOW)
            .min((u32::MAX - cpu.eip) as usize + 1);
        let bytes = bus.jit_code_bytes(offset, window);
        let decoded = if facts == CODE16 {
            decode::block_code16(bytes, cpu.eip, BLOCK_INSTRUCTIONS)
        } else {
            decode::block_extended(bytes, cpu.eip, BLOCK_INSTRUCTIONS)
        };
        let mut record = Record {
            eip: cpu.eip,
            facts,
            backing: backing.0,
            version,
            version_ptr,
            address: 0,
            instructions: 0,
            source: core::ptr::null(),
            source_len: 0,
            #[cfg(feature = "jit-profile")]
            opcode: profile_key(bytes),
            #[cfg(feature = "jit-profile")]
            memory: profile_memory(bytes, &decoded, facts),
        };
        let Some(compiled) = lower::block(&decoded, facts) else {
            self.stats.unsupported_entries += 1;
            self.insert(record);
            return Ok(Lookup::Interpret);
        };
        if !self.arena.fits(compiled.code.len(), compiled.bytes) {
            // No native code is active; every record and watch is retired.
            self.clear(bus)?;
            (_, version, version_ptr) = bus.jit_code_stamp(cpu, backing);
        }
        let source = self.arena.stash(bus.jit_code_bytes(offset, compiled.bytes));
        let (Some(source), Some(address)) = (source, self.arena.publish(&compiled.code)) else {
            return Err("JIT block larger than the arena".into());
        };
        self.stats.compiled_blocks += 1;
        #[cfg(all(feature = "jit-profile", target_os = "linux"))]
        {
            self.stats.compile_nanoseconds += compilation_start.elapsed().as_nanos();
        }
        record.version = version;
        record.version_ptr = version_ptr;
        record.address = address as u32 | 1;
        record.instructions = compiled.instructions;
        record.source = source;
        record.source_len = compiled.bytes as u32;
        self.insert(record);
        Ok(Lookup::Native(Native {
            address: address | 1,
            instructions: compiled.instructions,
            version: version_ptr,
            ram_page: offset as usize >> 12,
        }))
    }

    /// Enter the validated block for the current EIP in the dispatch table;
    /// returns the first slot of its privilege plane.
    fn publish(&mut self, cpu: &Cpu, block: Native) -> usize {
        let linear = cpu.seg(SegmentReg::Cs).base.wrapping_add(cpu.eip);
        let word = cpu.code_default16();
        let tag = if word { linear } else { cpu.eip };
        let plane = if word { 2 } else { usize::from(cpu.cpl() == 3) } * PLANE;
        let index = lower::table_slot(tag);
        let slot = plane + index;
        if self.pages[slot].version.is_null() {
            if word {
                self.filled16.push(index as u16);
            } else {
                self.filled.push(slot as u16);
            }
        }
        self.table[slot] = [tag, block.address as u32];
        // The fetch-cache entry found by the lookup is still current.
        let global = cpu.fetch_translation_is_global(LinearAddr(linear));
        if self.pages[slot].ram_page != block.ram_page as u32 {
            PageSlot::unlink(&mut self.pages, &mut self.page_heads, slot);
            if self.page_heads.len() <= block.ram_page {
                self.page_heads.resize(block.ram_page + 1, NO_SLOT);
            }
            let first = self.page_heads[block.ram_page];
            if first != NO_SLOT {
                self.pages[usize::from(first)].previous = slot as u16;
            }
            self.page_heads[block.ram_page] = slot as u16;
            self.pages[slot].previous = NO_SLOT;
            self.pages[slot].next = first;
            self.pages[slot].ram_page = block.ram_page as u32;
        }
        self.pages[slot].linear = linear & !4095 | u32::from(global);
        self.pages[slot].version = block.version;
        slot & !(PLANE - 1)
    }

    /// Run a chain starting with `block` if it fits `limit`. Returns completed
    /// instructions and the exit code. Only EXIT_INTERPRET leaves before the
    /// current instruction; attention/shadow exits have completed it.
    fn invoke(
        &mut self,
        cpu: &mut Cpu,
        bus: &mut HostBus,
        block: Native,
        limit: u64,
    ) -> Option<(u64, u32)> {
        #[cfg(feature = "jit-profile")]
        {
            self.profile.limit(limit);
        }
        if u64::from(block.instructions) > limit {
            #[cfg(feature = "jit-profile")]
            {
                self.profile.entry_rejected += 1;
            }
            return None;
        }
        let plane = self.publish(cpu, block);
        let word = cpu.code_default16();
        let entry = if word { self.entry16 } else { self.entry };
        let dispatch = if word { self.dispatch16 } else { self.dispatch };
        cpu.begin_instruction();
        cpu.segment_override = None;
        let budget = limit.min(u64::from(u32::MAX)) as u32;
        let mut frame = Frame {
            regs: cpu.regs,
            eip: cpu.eip,
            remaining: budget,
            table: self.table[plane..].as_ptr() as usize as u32,
            dispatch: dispatch as u32,
            popf_mask: crate::x86::interp::popfd_change_mask(cpu).unwrap_or(0),
            quiet: u32::from(bus.batch_interrupts_quiet()),
            selectors: [
                SegmentReg::Es,
                SegmentReg::Cs,
                SegmentReg::Ss,
                SegmentReg::Ds,
                SegmentReg::Fs,
                SegmentReg::Gs,
            ]
            .map(|seg| u32::from(cpu.seg(seg).selector)),
            ..Frame::default()
        };
        frame.set_eflags(cpu.effective_eflags());
        frame.set_segments(cpu);
        #[cfg(feature = "jit-profile")]
        let mut checked = CheckedInvocation {
            start: self.profile.invocation_start,
            counters: &mut self.profile,
            segments: lower::DATA_SEGMENTS.map(|seg| cpu.seg(seg)),
            cpl: cpu.cpl(),
            budget,
        };
        #[cfg(feature = "jit-profile")]
        if lower::PROFILE_CHECKED {
            frame.checked_profile = &mut checked as *mut CheckedInvocation as usize as u32;
            frame.checked_callback = profile_checked as *const () as usize as u32;
        }
        let mut address = block.address;
        loop {
            // A lookup below may promote a code page, which revokes write
            // rows in place; take the current view before every entry.
            let (tlb, state) = cpu.translation_cache.jit_view();
            frame.tlb = tlb as usize as u32;
            frame.tlb_state = state as u32;
            #[cfg(feature = "jit-profile")]
            let before = frame.remaining;
            // SAFETY: the entry, dispatcher and blocks were emitted for this
            // ABI and published in the live arena; table entries name only
            // blocks validated in the current context. The frame outlives the
            // call. TLB pointers are HostBus RAM tokens whose write tags
            // exclude watched code, and the translation cache is not modified
            // during the call.
            unsafe {
                let entry: Invoke = core::mem::transmute(entry);
                entry(&mut frame, address);
            }
            self.stats.invocations += 1;
            #[cfg(feature = "jit-profile")]
            {
                self.profile.steps(before - frame.remaining);
            }
            if frame.exit != EXIT_BOUNDARY || frame.remaining == 0 {
                #[cfg(feature = "jit-profile")]
                {
                    let reason = match frame.exit {
                        EXIT_INTERPRET => NativeReturn::InterpretGuard,
                        EXIT_BOUNDARY => NativeReturn::BudgetExhausted,
                        EXIT_ATTENTION => NativeReturn::Attention,
                        EXIT_SHADOW => NativeReturn::Shadow,
                        _ => NativeReturn::Other,
                    };
                    self.profile.native_returns[reason as usize] += 1;
                }
                break;
            }
            // A dispatch miss with budget left. No guest register, flag or
            // time has been committed yet; the probe reads only EIP, CS and
            // derived caches.
            cpu.eip = frame.eip;
            match self.probe(cpu, bus, false) {
                Probe::Known(Lookup::Native(next)) if next.instructions <= frame.remaining => {
                    // CPL cannot change natively: the same plane.
                    self.publish(cpu, next);
                    address = next.address;
                    self.stats.resolved += 1;
                    #[cfg(feature = "jit-profile")]
                    {
                        self.profile.native_returns[NativeReturn::DispatchResolved as usize] += 1;
                    }
                }
                _ => {
                    #[cfg(feature = "jit-profile")]
                    {
                        self.profile.unresolved();
                    }
                    break;
                }
            }
        }
        let executed = u64::from(budget - frame.remaining);
        #[cfg(feature = "jit-profile")]
        if word {
            self.profile.ip_checks += u64::from(frame.ip_checks);
            self.profile.pm16_native_steps += executed;
        }
        cpu.regs = frame.regs;
        cpu.eip = frame.eip;
        cpu.jit_commit_flags(frame.eflags());
        cpu.advance_tsc(executed);
        Some((executed, frame.exit))
    }

    pub(crate) fn run_batch(
        &mut self,
        cpu: &mut Cpu,
        bus: &mut HostBus,
        start: u64,
        budget: u64,
    ) -> Result<BatchResult, String> {
        // Code met for the first time is probably run once. It goes through
        // the interpreter's own loop, a short batch per lookup: one engine
        // iteration per instruction costs about three times the instruction.
        // Once an address is being counted, interpretation stops at each
        // transfer so that admission sees every visit.
        const COLD_BATCH: u64 = 16;
        bus.sync_ioram(cpu);
        let mut ran = 0;
        // EIP of the last instruction interpreted as cold code. Its sequential
        // successor is no block entry; interpretation continues to a transfer.
        let mut cold = None::<u32>;
        while ran < budget {
            #[cfg(feature = "jit-profile")]
            let profile_pm16 = code16::ENABLED && code16::pm16(cpu);
            #[cfg(not(feature = "jit-profile"))]
            let profile_pm16 = false;
            #[cfg(feature = "jit-profile")]
            if code16::ENABLED {
                if profile_pm16 {
                    self.profile.code16.observe(cpu);
                } else {
                    self.profile.code16.leave();
                }
            }
            let shadowed = cpu.interrupt_shadow();
            let eligible = !shadowed
                && !cpu.halted
                && if cpu.code_default16() {
                    cpu.cr0 & CR0_PE != 0
                        && cpu.cpl() == 3
                        && cpu.seg(SegmentReg::Ss).flags & SEGMENT_FLAG_DEFAULT_BIG == 0
                } else {
                    cpu.seg(SegmentReg::Cs).limit == u32::MAX
                }
                && cpu.eflags & (TF | VM) == 0;
            #[cfg(feature = "jit-profile")]
            if !eligible {
                let reason = if shadowed {
                    Iteration::Shadow
                } else if cpu.halted {
                    Iteration::Halted
                } else if cpu.code_default16() {
                    Iteration::Code16
                } else if cpu.seg(SegmentReg::Cs).limit != u32::MAX {
                    Iteration::CsLimit
                } else {
                    Iteration::TfVm
                };
                self.profile.iterations[reason as usize] += 1;
            }
            let (limit, attention) = bus
                .batch_attention_limit(start.wrapping_add(ran), budget - ran)
                .unwrap_or((1, false));
            let sequential = cold.is_some_and(|eip| cpu.eip.wrapping_sub(eip).wrapping_sub(1) < 15);
            cold = None;
            let mut first = false;
            if eligible && sequential {
                #[cfg(feature = "jit-profile")]
                {
                    self.profile.iterations[Iteration::Sequential as usize] += 1;
                }
                cold = Some(cpu.eip);
            } else if eligible {
                self.sync(cpu, bus);
                let native = match self.lookup(cpu, bus)? {
                    Lookup::Native(block) => {
                        #[cfg(feature = "jit-profile")]
                        {
                            self.profile.iterations[Iteration::Native as usize] += 1;
                            self.profile.attention_limits += u64::from(attention);
                            self.profile.invocation_start = start.wrapping_add(ran);
                        }
                        self.invoke(cpu, bus, block, limit)
                    }
                    Lookup::Interpret => {
                        #[cfg(feature = "jit-profile")]
                        {
                            self.profile.iterations[Iteration::Interpret as usize] += 1;
                        }
                        None
                    }
                    Lookup::Cold(new) => {
                        #[cfg(feature = "jit-profile")]
                        {
                            let kind = if new {
                                Iteration::ColdNew
                            } else {
                                Iteration::ColdCounting
                            };
                            self.profile.iterations[kind as usize] += 1;
                        }
                        first = new;
                        if !new {
                            cold = Some(cpu.eip);
                        }
                        None
                    }
                };
                if let Some((executed, exit)) = native {
                    ran += executed;
                    self.stats.native_steps += executed;
                    if executed != 0 {
                        bus.begin_instruction(
                            start.wrapping_add(ran).wrapping_sub(1),
                            cpu.tsc().wrapping_sub(1),
                        );
                    }
                    let control = matches!(exit, EXIT_ATTENTION | EXIT_SHADOW);
                    // Establish STI's shadow before Machine can observe IRQs.
                    // Quiet control exits can stay in this batch; the shared
                    // helper also clears jit_commit_flags' IF-rise attention.
                    if exit == EXIT_SHADOW {
                        cpu.inhibit_maskable_interrupts();
                    }
                    if control
                        && crate::x86::interp::continue_after_interrupt_shadow(
                            cpu,
                            bus,
                            executed < limit,
                        )
                    {
                        #[cfg(feature = "jit-profile")]
                        {
                            self.profile.shadow_continues += u64::from(exit == EXIT_SHADOW);
                            self.profile.attention_continues += u64::from(exit == EXIT_ATTENTION);
                        }
                        continue;
                    }
                    if control || (executed == limit && attention) {
                        #[cfg(feature = "jit-profile")]
                        {
                            self.profile.attention_ends +=
                                u64::from(executed == limit && attention);
                            self.profile.control_attention_returns += u64::from(control);
                        }
                        return Ok(BatchResult {
                            ran,
                            stop: BatchStop::Attention {
                                instruction_step: start.wrapping_add(ran).wrapping_sub(1),
                                was_interruptible: false,
                            },
                        });
                    }
                    if exit != EXIT_INTERPRET {
                        continue;
                    }
                    self.stats.memory_exits += 1;
                }
            }
            let mut batch = if first {
                Interpreter::run_batch(
                    cpu,
                    bus,
                    start.wrapping_add(ran),
                    (budget - ran).min(COLD_BATCH),
                )
            } else if eligible || shadowed || profile_pm16 {
                Interpreter::run_one(cpu, bus, start.wrapping_add(ran))
            } else {
                Interpreter::run_batch(cpu, bus, start.wrapping_add(ran), budget - ran)
            };
            if shadowed && limit > 1 && matches!(batch.stop, BatchStop::Budget) {
                // The shadow expired; let the machine observe pending interrupts.
                batch.stop = BatchStop::Attention {
                    instruction_step: start.wrapping_add(ran),
                    was_interruptible: false,
                };
            }
            ran += batch.ran;
            self.stats.interpreted_steps += batch.ran;
            #[cfg(feature = "jit-profile")]
            if profile_pm16 && !code16::pm16(cpu) {
                self.profile.code16.leave();
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

/// Segment (plus one), access, missing fact, and 67h bit of an entry operand.
#[cfg(feature = "jit-profile")]
fn profile_memory(bytes: &[u8], decoded: &decode::Block, facts: u32) -> u8 {
    let prefix_len = bytes
        .iter()
        .take(15)
        .take_while(|b| {
            matches!(
                b,
                0x26 | 0x2e | 0x36 | 0x3e | 0x64..=0x67 | 0xf0 | 0xf2 | 0xf3
            )
        })
        .count();
    let prefixes = &bytes[..prefix_len];
    let address16 = (facts == CODE16) != prefixes.contains(&0x67);
    let operand = decoded
        .instructions
        .first()
        .and_then(|i| lower::profile_memory(i.op, facts));
    // The decoder rejects 67h before producing an Op. Classify MOV's explicit
    // operand directly for A3, including the different 16-bit default segment.
    // This is diagnostic metadata only; it never admits or executes an opcode.
    let operand = operand.or_else(|| {
        let opcode = *bytes.get(prefix_len)?;
        if !address16 || !matches!(opcode, 0x88..=0x8b | 0xa0..=0xa3) {
            return None;
        }
        let mut segment = SegmentReg::Ds;
        if opcode < 0xa0 {
            let modrm = *bytes.get(prefix_len + 1)?;
            if modrm >= 0xc0 {
                return None;
            }
            if matches!(modrm & 7, 2 | 3) || (modrm & 7 == 6 && modrm & 0xc0 != 0) {
                segment = SegmentReg::Ss;
            }
        }
        for prefix in prefixes {
            segment = match prefix {
                0x26 => SegmentReg::Es,
                0x2e => SegmentReg::Cs,
                0x36 => SegmentReg::Ss,
                0x3e => SegmentReg::Ds,
                0x64 => SegmentReg::Fs,
                0x65 => SegmentReg::Gs,
                _ => segment,
            };
        }
        let write = matches!(opcode, 0x88 | 0x89 | 0xa2 | 0xa3);
        Some((
            segment,
            write,
            lower::profile_segment(segment, write, facts),
        ))
    });
    let Some((segment, write, allowed)) = operand else {
        return u8::from(address16) << 6;
    };
    let missing = if allowed {
        0
    } else if segment == SegmentReg::Cs {
        3
    } else if write {
        2
    } else {
        1
    };
    segment as u8 + 1 | (u8::from(write) << 3) | missing << 4 | u8::from(address16) << 6
}

/// Profile bucket: opcode (0x100 for 0F), common ModRM groups at 0x200,
/// and 0x400 for an operand-size prefix.
#[cfg(feature = "jit-profile")]
fn profile_key(bytes: &[u8]) -> u16 {
    let bytes = &bytes[..bytes.len().min(15)];
    let at = bytes
        .iter()
        .position(|b| {
            !matches!(
                b,
                0x26 | 0x2e | 0x36 | 0x3e | 0x64..=0x67 | 0xf0 | 0xf2 | 0xf3
            )
        })
        .unwrap_or(0);
    let narrow = if bytes[..at].contains(&0x66) {
        0x400
    } else {
        0
    };
    let byte = |i: usize| u16::from(*bytes.get(i).unwrap_or(&0));
    let (opcode, modrm) = match byte(at) {
        0x0f => (0x100 | byte(at + 1), byte(at + 2)),
        first => (first, byte(at + 1)),
    };
    let group = [
        0xc0, 0xc1, 0xd0, 0xd1, 0xd2, 0xd3, 0xf6, 0xf7, 0xfe, 0xff, 0x1ba,
    ]
    .iter()
    .position(|&op| op == opcode);
    narrow
        | match group {
            Some(group) => {
                0x200 + group as u16 * 16 + (modrm >> 3 & 7) + if modrm >= 0xc0 { 8 } else { 0 }
            }
            None => opcode,
        }
}

#[cfg(feature = "jit-profile")]
fn profile_name(key: u16) -> String {
    let prefix = if key & 0x400 != 0 { "66 " } else { "" };
    let key = key & 0x3ff;
    if key >= 0x200 {
        let group = usize::from(key - 0x200) / 16;
        let op = [
            0xc0, 0xc1, 0xd0, 0xd1, 0xd2, 0xd3, 0xf6, 0xf7, 0xfe, 0xff, 0x1ba,
        ][group];
        let form = if key & 8 != 0 { "reg" } else { "mem" };
        let name = if op >= 0x100 {
            format!("0f {:02x}", op & 0xff)
        } else {
            format!("{op:02x}")
        };
        format!("{prefix}{name} /{} {form}", key & 7)
    } else if key >= 0x100 {
        format!("{prefix}0f {:02x}", key & 0xff)
    } else {
        format!("{prefix}{key:02x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::x86::cpu::SegmentCache;

    fn publish(engine: &mut Engine, versions: &[u64], page: usize, plane: usize, ip: u32) -> usize {
        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(ip, 0);
        let (selector, flags) = match plane {
            0 => (8, 0xc09b),
            1 => (0xb, 0xc0fb),
            _ => (0xb, 0xfb),
        };
        cpu.set_seg_cache(
            SegmentReg::Cs,
            SegmentCache::new(selector, 0, u32::MAX, flags),
        );
        engine.publish(
            &cpu,
            Native {
                address: 0x1001 + 32 * page,
                instructions: 1,
                version: &versions[page],
                ram_page: page,
            },
        ) + lower::table_slot(ip)
    }

    #[test]
    fn page_retirement_covers_all_planes_and_preserves_other_pages() {
        let versions = [1, 1];
        let mut engine = Engine::with_capacity(65536).unwrap();
        let mut retired = Vec::new();
        for plane in 0..3 {
            for ip in [0x100, 0x120] {
                retired.push(publish(&mut engine, &versions, 0, plane, ip));
            }
        }
        let other = publish(&mut engine, &versions, 1, 2, 0x140);
        let kept = engine.table[other];
        for _ in 0..32 {
            engine.retire_code_page(0, &versions[0]);
            for &slot in &retired {
                assert_eq!(engine.table[slot][1], engine.miss);
            }
            assert_eq!(engine.table[other], kept);
            for plane in 0..3 {
                for ip in [0x100, 0x120] {
                    publish(&mut engine, &versions, 0, plane, ip);
                }
            }
        }
        // Page retirement can empty slots many times between context clears.
        // Republishing must not grow the filled-index allocation without bound.
        assert_eq!(
            engine.filled.len() + engine.filled16.len(),
            retired.len() + 1
        );
        engine.clear_table(false);
        assert_eq!(engine.table[other], kept);
        engine.retire_code_page(0, &versions[0]);
        for slot in retired {
            assert_eq!(engine.table[slot][1], engine.miss);
        }
        assert_eq!(engine.table[other], kept);
    }

    #[test]
    fn reused_slots_and_bulk_retirement_keep_page_lists_sound() {
        let versions = [1, 1, 1];
        let mut engine = Engine::with_capacity(65536).unwrap();
        let reused = publish(&mut engine, &versions, 0, 2, 0x100);
        let old_page = publish(&mut engine, &versions, 0, 1, 0x120);
        assert_eq!(publish(&mut engine, &versions, 1, 2, 0x100), reused);
        let kept = engine.table[reused];
        engine.retire_code_page(0, &versions[0]);
        assert_eq!(engine.table[reused], kept);
        assert_eq!(engine.table[old_page][1], engine.miss);
        let separate = publish(&mut engine, &versions, 2, 0, 0x1100);
        let separate_code = engine.table[separate];
        // The mapping-invalidation scan still unlinks every selected slot.
        engine.retire_table(|linear, _| linear & !4095 == 0);
        assert_eq!(engine.table[reused][1], engine.miss);
        assert_eq!(engine.table[separate], separate_code);
        engine.retire_code_page(1, &versions[1]);
        assert_eq!(engine.table[separate], separate_code);
        engine.clear_table(true); // Multi-page/global fallback.
        publish(&mut engine, &versions, 0, 2, 0x100);
        engine.retire_code_page(2, &versions[2]);
        assert_ne!(engine.table[reused][1], engine.miss);
        engine.retire_code_page(0, &versions[0]);
        assert_eq!(engine.table[reused][1], engine.miss);
    }
}
