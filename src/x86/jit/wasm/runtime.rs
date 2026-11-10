//! Cold admission, independently owned regions, deferred publication and CLOCK.
//!
//! A code page can own several immutable modules of bounded functions. Adding a newly hot entry
//! does not recompile its neighbours. Page entries encode a module slot and a
//! local block, never a permanent identity for recycled code. Charged bytes
//! include metadata, compiler scratch and one pending module. V8 allocations
//! are opaque: accounting and host code limits are distinct from measured RSS.

use super::{
    compile,
    frame::{
        ENTRY_BITS, ENTRY_MASK, ENTRY_SEGMENTS, Frame, GROUP_BITS, GROUP_FUNCTIONS, LINK_PLANES,
        LINK_SLOTS, Link, REGION_SLOTS,
    },
};
use crate::machine::HostBus;
use crate::x86::cpu::{LINEAR_TRANSLATION_CACHE_BITS, LinearTranslationCacheEntry};
use crate::x86::jit::validity::CodeContent;
use crate::x86::{
    BackingToken, BatchResult, BatchStop, Bus, Cpu, Interpreter, LinearAddr, PhysAddr, SegmentReg,
};
use alloc::{boxed::Box, collections::BTreeMap, format, string::String, vec, vec::Vec};

const PAGE_CHARGE: usize = 20 * 1024;
const SCRATCH_CHARGE: usize = 32 * 1024 * 1024;
const MAX_PAGES: usize = 1024;
const PAGE_LOOKUP_SLOTS: usize = 1024;
const HEAT_SLOTS: usize = 16384;
const COLD_ENTRY_SLOTS: usize = 4096;
const RETIRED_ROOT_SLOTS: usize = 4096;
const MAX_MODULE_BYTES: usize = 64 * 1024 + 1024;
// Admission measures guest work, not host time or core polling frequency.
// Give a newly hot entry one short observation window to gather its paths.
// A burst can earn admission sooner; idle heat expires without a sweep.
const ADMISSION_DELAY: u64 = 64 * 1024;
const HEAT_IDLE_LIMIT: u64 = 8 * ADMISSION_DELAY;
const HEAT_COUNT_BITS: u32 = 20;
const HEAT_COUNT_MASK: u64 = (1 << HEAT_COUNT_BITS) - 1;
const HEAT_TIME_BITS: u32 = 64 - HEAT_COUNT_BITS;
const HEAT_TIME_MASK: u64 = u64::MAX >> HEAT_COUNT_BITS;

#[link(wasm_import_module = "browser")]
unsafe extern "C" {
    fn jit_compile(slot: u32, request: u32, bytes: *const u8, len: u32) -> u32;
    fn jit_retire(slot: u32);
}

#[derive(Default, Debug)]
pub struct Stats {
    pub native_steps: u64,
    pub interpreted_steps: u64,
    pub invocations: u64,
    pub compiled: u64,
    pub published: u64,
    pub discarded: u64,
    pub evicted: u64,
    pub invalidated: u64,
    pub evicted_unused: u64,
    pub content_reuses: u64,
    pub memory_exits: u64,
    pub semantic_exits: u64,
    pub cached_segment_loads: u64,
    pub repeat_exits: u64,
    pub partial_repeat_elements: u64,
    pub zero_step_exits: u64,
    pub short_budget_exits: u64,
    pub module_bytes: u64,
    pub split_functions: u64,
    pub generated_functions: u64,
    pub deferred_edges: u64,
    pub capacity_evictions: u64,
    pub recent_capacity_recompiles: u64,
    pub same_version_recompiles: u64,
    pub retirement_history_collisions: u64,
    pub delay_candidates: u64,
    /// Candidates lost to idle expiration or a heat-table tag collision.
    pub delay_expired: u64,
    pub admitted_after_delay: u64,
    pub admitted_burst: u64,
    pub admitted_reuse: u64,
    pub ineligible_steps: u64,
    pub charged_bytes: usize,
    pub peak_charged_bytes: usize,
    // Sparse host-wall samples distinguish generated execution (including V8
    // tiering stalls) from shared interpretation. Absent from normal builds.
    #[cfg(feature = "jit-profile")]
    pub sampled_native_ms: f64,
    #[cfg(feature = "jit-profile")]
    pub sampled_interpreter_ms: f64,
    #[cfg(feature = "jit-profile")]
    pub compiler_ms: f64,
    #[cfg(feature = "jit-profile")]
    pub exits: [u64; 8],
    /// Failed probe, ordinary RAM, or an uncached linear-framebuffer pointer.
    #[cfg(feature = "jit-profile")]
    pub resident_refills: [u64; 3],
    /// Empty row, privilege-only mismatch, missing write proof, different
    /// linear page in the slot, or old generation. Classify before refill;
    /// this distinguishes address conflicts from privilege ping-pong.
    #[cfg(feature = "jit-profile")]
    pub data_miss_causes: [u64; 5],
    #[cfg(feature = "jit-profile")]
    pub sampled_uncached_interpreter_ms: f64,
    /// Distinct foreign pages with untranslated mature/warm roots at an
    /// accepted submission. Capped at seven companions; diagnostic opportunity
    /// counts, not execute proofs or predicted speedups from packing.
    #[cfg(feature = "jit-profile")]
    pub mature_companions: [u64; GROUP_FUNCTIONS],
    #[cfg(feature = "jit-profile")]
    pub warm_companions: [u64; GROUP_FUNCTIONS],
}

struct Page {
    key: (u32, u32), // backing RAM page offset, CS-relative page origin
    version: u64,
    version_ptr: *const u64,
    recent: bool,
    entries: Box<[u32]>, // (function slot + 1) << ENTRY_BITS | local block
    regions: Vec<usize>,
}

/// Cache ownership lookup, including absent pages. This is not an execute or
/// source-version proof: callers still qualify the current translation/code.
/// Creation/removal updates the affected key before a page slot can be reused.
#[derive(Clone, Copy, Default)]
struct PageLookup {
    key: u64,
    page: u16, // slot + 1, or zero when this key has no admitted page
    valid: bool,
}

/// Cold interpreter handoff metadata for a published execute link. Keep it
/// outside the generated link row: native chains never use physical addresses.
/// Low physical bit records the checked translation's global-page attribute.
#[derive(Clone, Copy, Default)]
struct FetchProof {
    phys_global: u64,
    backing: u64,
}
#[derive(Clone, Copy, Default)]
struct Heat {
    tag: u64,
    // One hot load/store for exact guest-work time and visit count. Twenty
    // count bits exceed four times every accepted u16 admission threshold.
    // Cold candidate timestamps live separately, keeping ordinary rows 16 B.
    seen_count: u64,
    #[cfg(feature = "jit-profile")]
    origin: u32,
    #[cfg(feature = "jit-profile")]
    last_seen_interpreted: u64,
}

impl Heat {
    #[inline]
    fn count(&self) -> u32 {
        (self.seen_count & HEAT_COUNT_MASK) as u32
    }

    #[inline]
    fn age(&self, step: u64) -> u64 {
        step.wrapping_sub(self.seen_count >> HEAT_COUNT_BITS) & HEAT_TIME_MASK
    }

    #[inline]
    fn observe(&mut self, step: u64, count: u32) {
        self.seen_count = (step << HEAT_COUNT_BITS) | u64::from(count).min(HEAT_COUNT_MASK);
    }

    #[inline]
    fn clear_count(&mut self) {
        self.seen_count &= !HEAT_COUNT_MASK;
    }
}

#[derive(Clone, Copy, Default)]
// PC in the high word; the low two bytes hold remaining/skipped visits.
// One carry changes both counters, so a cold branch needs one memory update.
struct ColdEntry(u64);

/// Scheduling hints only. An entry recently found cold can stay in the shared
/// interpreter for seven further control-flow visits before full dispatch.
/// Skipped visits contribute to heat when that entry is next qualified. A
/// collision loses only observations; stale hints only affect admission timing.
/// No hint can execute generated code or replace a fetch/permission check.
pub(crate) struct ColdEntries([ColdEntry; COLD_ENTRY_SLOTS]);

impl ColdEntries {
    fn new() -> Self {
        Self([ColdEntry::default(); COLD_ENTRY_SLOTS])
    }

    #[inline]
    fn entry(&mut self, pc: u32) -> &mut ColdEntry {
        &mut self.0[(pc.wrapping_mul(0x9e3779b9) >> 20) as usize]
    }

    #[inline]
    pub(crate) fn should_dispatch(&mut self, pc: u32) -> bool {
        let entry = self.entry(pc);
        if (entry.0 >> 32) as u32 != pc || entry.0 & 255 == 0 {
            return true;
        }
        // Remaining is nonzero and remaining + skipped is always seven.
        // Adding 255 decrements its low byte and carries into skipped.
        entry.0 += 255;
        false
    }

    fn observe(&mut self, pc: u32) -> u32 {
        let entry = self.entry(pc);
        let visits = 1 + if (entry.0 >> 32) as u32 == pc {
            ((entry.0 >> 8) & 255) as u32
        } else {
            0
        };
        *entry = ColdEntry((u64::from(pc) << 32) | 7);
        visits
    }

    fn forget(&mut self, pc: u32) {
        let entry = self.entry(pc);
        if (entry.0 >> 32) as u32 == pc {
            *entry = ColdEntry::default();
        }
    }
}

/// Bounded metadata only: no source bytes, Wasm code, or function references.
/// It recognizes capacity recompilation without counting host tier promotion
/// or adding instrumentation to generated execution. Collisions can lose
/// history, so reported hits are a lower bound, never a count of all repeats.
#[derive(Clone, Copy, Default)]
struct RetiredRoot {
    key: u64,
    version: u64,
    used: bool,
    valid: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum RetireReason {
    Capacity,
    SourceChanged,
    Reset,
}

struct Region {
    #[cfg(feature = "jit-profile")]
    trace_request: u32,
    page: usize,
    function: u32,
    charge: usize,
    entries: Vec<(u16, u16)>,
    content: Box<CodeContent>,
    function_starts: Vec<u16>,
}
struct Pending {
    slot: usize,
    request: u32,
    region: Region,
}

/// A post-interpreter framebuffer proof, valid until interpretation or the
/// next Machine return. It only occupies a TLB slot during a generated call;
/// restoration precedes all Rust MMU/cache/device operations. The ordinary
/// interpreter must never observe a VRAM pointer in its RAM-only cache.
struct TransientPage {
    index: usize,
    row: LinearTranslationCacheEntry,
}

pub struct Engine {
    // Stable explicit ABI storage. Generated modules can specialize its address
    // and prove internal stores are inside the imported memory's minimum size.
    frame: Box<Frame>,
    transient: Option<TransientPage>,
    heat: Box<[Heat; HEAT_SLOTS]>,
    candidate_at: Box<[u64; HEAT_SLOTS]>,
    heat_epoch: u64,
    cold_entries: Box<ColdEntries>,
    retired_roots: Box<[RetiredRoot; RETIRED_ROOT_SLOTS]>,
    pages: BTreeMap<(u32, u32), usize>,
    page_lookup: Box<[PageLookup; PAGE_LOOKUP_SLOTS]>,
    page_slots: Vec<Option<Page>>,
    regions: Vec<Option<Region>>,
    recency: Box<[u32; REGION_SLOTS]>,
    pending: Option<Pending>,
    request: u32,
    hand: usize,
    page_hand: usize,
    budget: usize,
    threshold: u16,
    qualified: bool,
    pub stats: Stats,
    links: Box<[Link; LINK_PLANES * LINK_SLOTS]>,
    fetch_proofs: Box<[FetchProof; LINK_PLANES * LINK_SLOTS]>,
    context: Option<(u64, u32)>, // paging epoch and CS base; privilege has separate link planes
    #[cfg(feature = "jit-profile")]
    misses: BTreeMap<(u32, u8), (u64, [u8; 15])>,
    #[cfg(feature = "jit-profile")]
    samples: u64,
    #[cfg(feature = "jit-profile")]
    trace: super::trace::Trace,
}

impl Engine {
    pub fn new(budget: usize, threshold: u16) -> Self {
        assert!(budget >= 40 * 1024 * 1024 && threshold > 0);
        Self {
            frame: Box::default(),
            transient: None,
            // Fixed capacities are part of the type, so masked hot indexes
            // need no dynamic length check. Convert after heap allocation.
            heat: vec![Heat::default(); HEAT_SLOTS]
                .into_boxed_slice()
                .try_into()
                .ok()
                .unwrap(),
            candidate_at: vec![0; HEAT_SLOTS]
                .into_boxed_slice()
                .try_into()
                .ok()
                .unwrap(),
            heat_epoch: 0,
            // Fixed 32 KiB of interpreter scheduling hints, inside the reserve.
            cold_entries: Box::new(ColdEntries::new()),
            // Fixed metadata is covered by the existing scratch/owner reserve.
            retired_roots: vec![RetiredRoot::default(); RETIRED_ROOT_SLOTS]
                .into_boxed_slice()
                .try_into()
                .ok()
                .unwrap(),
            pages: BTreeMap::new(),
            // Fixed 16 KiB, covered by the scratch/owner reserve.
            page_lookup: vec![PageLookup::default(); PAGE_LOOKUP_SLOTS]
                .into_boxed_slice()
                .try_into()
                .ok()
                .unwrap(),
            page_slots: (0..MAX_PAGES).map(|_| None).collect(),
            regions: (0..REGION_SLOTS).map(|_| None).collect(),
            recency: vec![0; REGION_SLOTS]
                .into_boxed_slice()
                .try_into()
                .ok()
                .unwrap(),
            pending: None,
            request: 0,
            hand: 0,
            page_hand: 0,
            budget,
            threshold,
            qualified: false,
            stats: Stats {
                charged_bytes: SCRATCH_CHARGE,
                peak_charged_bytes: SCRATCH_CHARGE,
                ..Stats::default()
            },
            links: vec![Link::default(); LINK_PLANES * LINK_SLOTS]
                .into_boxed_slice()
                .try_into()
                .ok()
                .unwrap(),
            fetch_proofs: vec![FetchProof::default(); LINK_PLANES * LINK_SLOTS]
                .into_boxed_slice()
                .try_into()
                .ok()
                .unwrap(),
            context: None,
            #[cfg(feature = "jit-profile")]
            misses: BTreeMap::new(),
            #[cfg(feature = "jit-profile")]
            samples: 0,
            #[cfg(feature = "jit-profile")]
            trace: super::trace::Trace::new(),
        }
    }
    pub fn report(&self) -> String {
        let report = format!(
            "wasm_jit: {:?}\nwasm_jit.budget={} pages={} live_regions={} pending={}\n",
            self.stats,
            self.budget,
            self.pages.len(),
            self.regions.iter().filter(|r| r.is_some()).count(),
            self.pending.is_some()
        );
        #[cfg(feature = "jit-profile")]
        {
            let mut report = report;
            let mut misses: Vec<_> = self.misses.iter().collect();
            misses.sort_unstable_by_key(|(_, (count, _))| core::cmp::Reverse(*count));
            for ((pc, kind), (count, bytes)) in misses.into_iter().take(40) {
                if *kind == 8 {
                    report.push_str(&format!("dispatch {pc:08x} samples={count}\n"));
                    continue;
                }
                report.push_str(&format!(
                    "miss {pc:08x} reason={kind} samples={count} {bytes:02x?} {:?}\n",
                    super::super::decode::block(bytes, *pc, 1)
                        .instructions
                        .first()
                        .map(|i| i.op)
                ));
            }
            return report;
        }
        #[cfg(not(feature = "jit-profile"))]
        report
    }
    fn charge(&mut self, bytes: usize) {
        self.stats.charged_bytes += bytes;
        self.stats.peak_charged_bytes = self.stats.peak_charged_bytes.max(self.stats.charged_bytes);
        assert!(
            self.stats.charged_bytes <= self.budget,
            "Wasm admission exceeded budget"
        );
    }
    #[cfg(feature = "jit-profile")]
    pub fn take_trace(&mut self) -> String {
        use core::fmt::Write;
        let mut out = self.trace.drain();
        let mut separator = "";
        for (slot, region) in self.regions.iter().enumerate() {
            if let Some(region) = region {
                let page = self.page_slots[region.page].as_ref().unwrap();
                write!(
                    out,
                    "{separator}[{},{},{},{},{},{},{}]",
                    region.trace_request,
                    slot,
                    (self.recency[slot] & 0x7fff_fffc) >> 2,
                    region.charge,
                    region.function_starts.len(),
                    page.key.0,
                    page.key.1
                )
                .unwrap();
                separator = ",";
            }
        }
        write!(
            out,
            "],\"native_steps\":{},\"interpreted_steps\":{},\"charged_bytes\":{}}}",
            self.stats.native_steps, self.stats.interpreted_steps, self.stats.charged_bytes
        )
        .unwrap();
        out
    }
    fn retire(&mut self, slot: usize, reason: RetireReason) {
        if let Some(region) = self.regions[slot].take() {
            #[cfg(feature = "jit-profile")]
            self.trace.push(
                self.stats.native_steps,
                self.stats.interpreted_steps,
                5,
                [
                    u64::from(region.trace_request),
                    slot as u64,
                    u64::from((self.recency[slot] & 0x7fff_fffc) >> 2),
                    match reason {
                        RetireReason::Capacity => 1,
                        RetireReason::SourceChanged => 2,
                        RetireReason::Reset => 3,
                    },
                    region.charge as u64,
                    0,
                    0,
                    0,
                ],
            );
            let page = self.page_slots[region.page].as_mut().unwrap();
            if reason == RetireReason::Capacity {
                self.stats.capacity_evictions += 1;
                for &offset in &region.function_starts {
                    let pc = page.key.1.wrapping_add(u32::from(offset));
                    let key = (u64::from(page.key.0) << 32) | u64::from(pc);
                    let at = Self::heat_index(page.key.0, pc) & (RETIRED_ROOT_SLOTS - 1);
                    let old = &mut self.retired_roots[at];
                    self.stats.retirement_history_collisions +=
                        u64::from(old.valid && old.key != key);
                    *old = RetiredRoot {
                        key,
                        version: page.version,
                        used: self.recency[slot] & 0x80000000 != 0,
                        valid: true,
                    };
                }
            }
            for &(offset, _) in &region.entries {
                let entry = page.entries[usize::from(offset)];
                if entry != 0 && ((entry >> ENTRY_BITS) - 1) >> GROUP_BITS == slot as u32 {
                    page.entries[usize::from(offset)] = 0;
                }
                let pc = page.key.1.wrapping_add(u32::from(offset));
                let heat = &mut self.heat[Self::heat_index(page.key.0, pc)];
                if heat.tag == (u64::from(page.key.0) << 32) | u64::from(pc) {
                    heat.clear_count();
                }
            }
            page.regions.retain(|&index| index != slot);
            self.stats.charged_bytes -= region.charge;
            self.stats.evicted_unused += u64::from(self.recency[slot] & 0x80000000 == 0);
            self.recency[slot] = 0;
            // Clear entries/functions before slot reuse. A module embeds only
            // its own slot and page addresses, never another module's identity.
            unsafe { jit_retire(slot as u32) };
            self.stats.evicted += 1;
        }
    }
    fn retire_page(&mut self, page: usize, reason: RetireReason) {
        while let Some(&slot) = self.page_slots[page].as_ref().unwrap().regions.last() {
            self.retire(slot, reason);
        }
        let page = self.page_slots[page].take().unwrap();
        self.pages.remove(&page.key);
        self.cache_page(page.key, None);
        self.stats.charged_bytes -= PAGE_CHARGE;
        self.links.fill(Link::default()); // release aliases before freeing entries
    }
    fn victim(&mut self) -> Option<usize> {
        for _ in 0..REGION_SLOTS * 4 {
            let slot = self.hand;
            self.hand = (slot + 1) % REGION_SLOTS;
            if self.pending.as_ref().is_some_and(|p| p.slot == slot) {
                continue;
            }
            if self.regions[slot].is_some() {
                if self.recency[slot] & 3 != 0 {
                    self.recency[slot] -= 1;
                } else {
                    return Some(slot);
                }
            }
        }
        None
    }
    fn page_victim(&mut self, keep: Option<usize>) -> Option<usize> {
        for _ in 0..MAX_PAGES * 2 {
            let slot = self.page_hand;
            self.page_hand = (slot + 1) % MAX_PAGES;
            if Some(slot) == keep || self.pending.as_ref().is_some_and(|p| p.region.page == slot) {
                continue;
            }
            if let Some(page) = &mut self.page_slots[slot] {
                if page.recent {
                    page.recent = false;
                } else if page.regions.is_empty() {
                    return Some(slot);
                }
            }
        }
        None
    }
    fn reserve(&mut self, bytes: usize, keep: Option<usize>) -> bool {
        if bytes > self.budget - SCRATCH_CHARGE {
            return false;
        }
        while self.stats.charged_bytes > self.budget - bytes {
            if let Some(page) = self.page_victim(keep) {
                self.retire_page(page, RetireReason::Capacity);
            } else if let Some(slot) = self.victim() {
                self.retire(slot, RetireReason::Capacity);
            } else {
                return false;
            }
        }
        true
    }
    pub(crate) fn clear(&mut self, bus: &mut HostBus) {
        if let Some(pending) = self.pending.take() {
            #[cfg(feature = "jit-profile")]
            self.trace.push(
                self.stats.native_steps,
                self.stats.interpreted_steps,
                5,
                [
                    u64::from(pending.request),
                    pending.slot as u64,
                    0,
                    3,
                    pending.region.charge as u64,
                    0,
                    0,
                    0,
                ],
            );
            self.stats.charged_bytes -= pending.region.charge;
        }
        for page in 0..MAX_PAGES {
            if self.page_slots[page].is_some() {
                self.retire_page(page, RetireReason::Reset);
            }
        }
        // A late completion is rejected. The host blocks another job until it settles.
        self.request = self
            .request
            .checked_add(1)
            .expect("Wasm request identity exhausted");
        bus.jit_release_code_pages();
        self.heat.fill(Heat::default());
        self.candidate_at.fill(0);
        self.cold_entries.0.fill(ColdEntry::default());
        self.retired_roots.fill(RetiredRoot::default());
    }
    pub(crate) fn complete(
        &mut self,
        cpu: &mut Cpu,
        bus: &mut HostBus,
        request: u32,
        function: u32,
    ) -> bool {
        if !self.pending.as_ref().is_some_and(|p| p.request == request) {
            return false;
        }
        let Pending {
            slot, mut region, ..
        } = self.pending.take().unwrap();
        let page = self.page_slots[region.page].as_mut().unwrap();
        let current = bus.jit_code_bytes(u64::from(page.key.0), region.content.end);
        if !region.content.matches(current) {
            #[cfg(feature = "jit-profile")]
            self.trace.push(
                self.stats.native_steps,
                self.stats.interpreted_steps,
                5,
                [
                    u64::from(request),
                    slot as u64,
                    0,
                    4,
                    region.charge as u64,
                    0,
                    0,
                    0,
                ],
            );
            self.stats.charged_bytes -= region.charge;
            self.stats.discarded += 1;
            return false;
        }
        // Check neighbours before accepting a new page version: this pending
        // compilation may cover bytes unaffected by a write elsewhere on it.
        let token = BackingToken(current.as_ptr() as u64);
        let (_, version, version_ptr) = bus.jit_code_stamp(cpu, token);
        self.validate_page(region.page, version, version_ptr, bus);
        let page = self.page_slots[region.page].as_mut().unwrap();
        region.function = function;
        for &(offset, entry) in &region.entries {
            page.entries[usize::from(offset)] =
                (((slot * GROUP_FUNCTIONS) as u32 + 1) << ENTRY_BITS) + u32::from(entry - 1);
            self.cold_entries
                .forget(page.key.1.wrapping_add(u32::from(offset)));
        }
        page.regions.push(slot);
        self.recency[slot] = 0;
        #[cfg(feature = "jit-profile")]
        self.trace.push(
            self.stats.native_steps,
            self.stats.interpreted_steps,
            4,
            [
                u64::from(request),
                slot as u64,
                version,
                u64::from(function),
                0,
                0,
                0,
                0,
            ],
        );
        self.regions[slot] = Some(region);
        self.stats.published += 1;
        true
    }
    fn validate_page(
        &mut self,
        page_id: usize,
        version: u64,
        version_ptr: *const u64,
        bus: &HostBus,
    ) {
        let page = self.page_slots[page_id].as_ref().unwrap();
        if page.version == version {
            return;
        }
        let backing = page.key.0;
        let regions = page.regions.clone();
        for slot in regions {
            let region = self.regions[slot].as_ref().unwrap();
            if region
                .content
                .matches(bus.jit_code_bytes(u64::from(backing), region.content.end))
            {
                self.stats.content_reuses += 1;
            } else {
                self.stats.invalidated += 1;
                self.retire(slot, RetireReason::SourceChanged);
            }
        }
        let page = self.page_slots[page_id].as_mut().unwrap();
        page.version = version;
        page.version_ptr = version_ptr;
        // Each link compares its stable source-version cell before reading
        // the entry array. Old aliases of this page already fail that check;
        // unrelated pages retain their execute proofs. Page reclamation still
        // clears links before releasing entry-array storage.
    }
    fn heat_index(backing: u32, pc: u32) -> usize {
        ((pc ^ backing.rotate_left(13)).wrapping_mul(0x9e3779b9) >> 18) as usize
    }

    #[inline]
    fn cache_page(&mut self, key: (u32, u32), page: Option<usize>) {
        let at = Self::heat_index(key.0, key.1) & (PAGE_LOOKUP_SLOTS - 1);
        self.page_lookup[at] = PageLookup {
            key: (u64::from(key.0) << 32) | u64::from(key.1),
            page: page.map_or(0, |id| id as u16 + 1),
            valid: true,
        };
    }

    #[inline]
    fn find_page(&mut self, key: (u32, u32)) -> Option<usize> {
        let at = Self::heat_index(key.0, key.1) & (PAGE_LOOKUP_SLOTS - 1);
        let cached = self.page_lookup[at];
        if cached.valid && cached.key == (u64::from(key.0) << 32) | u64::from(key.1) {
            return cached.page.checked_sub(1).map(usize::from);
        }
        let page = self.pages.get(&key).copied();
        self.cache_page(key, page);
        page
    }

    fn lookup(&mut self, cpu: &mut Cpu, bus: &mut HostBus) -> Option<(u32, u32)> {
        self.qualified = false;
        let linear = cpu.seg(SegmentReg::Cs).base.wrapping_add(cpu.eip);
        let offset = (linear & 4095) as usize;
        let link_index =
            ((linear as usize >> 12) & (LINK_SLOTS - 1)) + usize::from(cpu.cpl() == 3) * LINK_SLOTS;
        let link = self.links[link_index];
        let (key, existing) = if link.tag == (linear & !4095) | 1
            // Links are cleared before their page-entry storage is freed.
            // CodePages owns stable version words until the machine is dropped.
            && unsafe { *(link.version_ptr as *const u64) } == link.version
        {
            let entry = unsafe { *((link.entries as *const u32).add(offset)) };
            if entry != 0 {
                let slot = (entry >> ENTRY_BITS) - 1;
                return Some((link.function + slot, entry & ENTRY_MASK));
            }
            // This proof covers the entire source page, including an entry
            // that has not earned translation yet. Page reclamation clears
            // links before recycling this owner slot or its entry allocation.
            let page = self.page_slots[link.page as usize].as_mut().unwrap();
            page.recent = true;
            (page.key, Some(link.page as usize))
        } else {
            // The interpreter must have approved execute access first. The
            // ownership cache below cannot qualify a mapping or permissions.
            let hit = cpu.fetch_cache_lookup(LinearAddr(linear))?;
            let backing = bus
                .jit_backing_offset(hit.backing)
                .wrapping_sub(offset as u32);
            let key = (backing, cpu.eip.wrapping_sub(offset as u32));
            let existing = self.find_page(key);
            if let Some(page_id) = existing {
                let version_ptr = self.page_slots[page_id].as_ref().unwrap().version_ptr;
                // An odd version is already watched. Its stable cell suffices
                // here just as it does on a link hit. A write retires the watch
                // to an even version: re-promotion must still revoke writable
                // aliases before dependency validation or native publication.
                let mut version = unsafe { *version_ptr };
                if version & 1 == 0 {
                    version = bus.jit_code_stamp(cpu, hit.backing).1;
                }
                self.validate_page(page_id, version, version_ptr, bus);
                let page = self.page_slots[page_id].as_mut().unwrap();
                page.recent = true;
                let entry = page.entries[offset];
                let owner = if entry != 0 {
                    Some(((entry >> ENTRY_BITS) as usize - 1) >> GROUP_BITS)
                } else {
                    page.regions.first().copied()
                };
                if let Some(slot) = owner {
                    let region = self.regions[slot].as_ref().unwrap();
                    // The execute/source proof covers cold entries too. All
                    // regions use the host's one fixed function-table range,
                    // so any live region supplies its base. Retirement clears
                    // entries before functions, and page release clears links.
                    self.links[link_index] = Link {
                        tag: (linear & !4095) | 1,
                        function: region.function - (slot * GROUP_FUNCTIONS) as u32,
                        entries: page.entries.as_ptr() as u32,
                        version_ptr: page.version_ptr as u32,
                        version: page.version,
                        page: page_id as u32,
                    };
                    self.fetch_proofs[link_index] = FetchProof {
                        phys_global: (hit.phys.0 & !4095)
                            | u64::from(cpu.fetch_translation_is_global(LinearAddr(linear))),
                        backing: hit.backing.0.wrapping_sub(offset as u64),
                    };
                    // The user execute proof is stronger than supervisor's.
                    // Publish the same alias there so a CPL transition need
                    // not refetch/requalify this page. Both planes retain the
                    // same mapping/CS epoch, source version and retirement.
                    if cpu.cpl() == 3 {
                        self.links[link_index - LINK_SLOTS] = self.links[link_index];
                        self.fetch_proofs[link_index - LINK_SLOTS] = self.fetch_proofs[link_index];
                    }
                    if entry != 0 {
                        return Some((
                            self.links[link_index].function + (entry >> ENTRY_BITS) - 1,
                            entry & ENTRY_MASK,
                        ));
                    }
                }
            }
            (key, existing)
        };
        self.qualified = true;
        let backing = key.0;
        let visits = self.cold_entries.observe(cpu.eip);
        // This is reached only for uncompiled code. Existing counters already
        // count guest work across native/interpreted execution and resets;
        // neither realtime HLT nor host compilation advances this clock.
        let step = self.stats.native_steps + self.stats.interpreted_steps;
        let threshold = u32::from(self.threshold);
        let heat_index = Self::heat_index(backing, cpu.eip);
        let heat = &mut self.heat[heat_index];
        let tag = (u64::from(backing) << 32) | u64::from(cpu.eip);
        if heat.tag != tag || heat.age(step) > HEAT_IDLE_LIMIT {
            #[cfg(feature = "jit-profile")]
            if heat.count() >= (threshold / 4).max(1) {
                self.trace.push(
                    self.stats.native_steps,
                    self.stats.interpreted_steps,
                    2,
                    [
                        heat.tag,
                        u64::from(heat.count()),
                        step.wrapping_sub(heat.age(step)),
                        self.candidate_at[heat_index],
                        if heat.tag == tag { 1 } else { 2 },
                        tag,
                        heat.last_seen_interpreted,
                        0,
                    ],
                );
            }
            self.stats.delay_expired += u64::from(heat.count() >= threshold);
            *heat = Heat {
                tag,
                ..Heat::default()
            };
            self.candidate_at[heat_index] = 0;
        }
        #[cfg(feature = "jit-profile")]
        let previous_seen = step.wrapping_sub(heat.age(step));
        #[cfg(feature = "jit-profile")]
        {
            heat.origin = key.1;
            heat.last_seen_interpreted = self.stats.interpreted_steps;
        }
        let previous = heat.count();
        heat.observe(step, previous.saturating_add(visits));
        if previous < threshold && heat.count() >= threshold {
            self.candidate_at[heat_index] = step;
            self.stats.delay_candidates += 1;
        }
        #[cfg(feature = "jit-profile")]
        if (previous < (threshold / 4).max(1) && heat.count() >= (threshold / 4).max(1))
            || (previous < threshold && heat.count() >= threshold)
        {
            self.trace.push(
                self.stats.native_steps,
                self.stats.interpreted_steps,
                1,
                [
                    tag,
                    u64::from(heat.count()),
                    previous_seen,
                    self.candidate_at[heat_index],
                    0,
                    0,
                    0,
                    0,
                ],
            );
        }
        if heat.count() < threshold || self.pending.is_some() {
            return None;
        }

        // A remembered, actually used capacity victim has already earned its
        // trial. It must still reach the normal threshold, and its source
        // version must match; history is never an execute/mapping proof.
        let old = self.retired_roots[Self::heat_index(backing, cpu.eip) & (RETIRED_ROOT_SLOTS - 1)];
        let reused = old.valid && old.used && old.key == tag && {
            let version = existing.map_or_else(
                || {
                    let token =
                        BackingToken(bus.jit_code_bytes(u64::from(backing), 1).as_ptr() as u64);
                    bus.jit_code_stamp(cpu, token).1
                },
                |id| self.page_slots[id].as_ref().unwrap().version,
            );
            old.version == version
        };
        let burst = heat.count() >= threshold * 4;
        let mature = step - self.candidate_at[heat_index] >= ADMISSION_DELAY
            && heat.count() >= threshold + (threshold / 4).max(1);
        if !reused && !burst && !mature {
            return None;
        }
        heat.clear_count();
        self.submit_entry(cpu, bus, key, offset, existing, reused, burst);
        None
    }

    fn prepare_interpreter_fetch(&self, cpu: &mut Cpu) {
        if cpu.fetch_cursor_backing_u8(cpu.eip).is_some() {
            return;
        }
        // The caller has synchronized the mapping/CS epoch and INVLPG before
        // lookup. Generated execution cannot change either or the current CPL.
        // Check this destination's own execute plane, including cold entries.
        let linear = cpu.seg(SegmentReg::Cs).base.wrapping_add(cpu.eip);
        let index =
            ((linear as usize >> 12) & (LINK_SLOTS - 1)) + usize::from(cpu.cpl() == 3) * LINK_SLOTS;
        let link = self.links[index];
        if link.tag != (linear & !4095) | 1 {
            return;
        }
        let proof = self.fetch_proofs[index];
        let offset = u64::from(linear & 4095);
        let backing = BackingToken(proof.backing.wrapping_add(offset));
        // Restore the ordinary entry too: fetch invalidation owns the cursor
        // through this cache. Global mappings retain their architectural scope.
        cpu.fill_fetch_cache_with_backing(
            LinearAddr(linear),
            PhysAddr((proof.phys_global & !1) + offset),
            backing,
            proof.phys_global & 1 != 0,
        );
        // Source versions protect decoded native code, not this byte cursor.
        // The interpreter reads the current bytes, including self modifications.
        let window = (4096 - offset).min(u64::from(u32::MAX - cpu.eip) + 1);
        cpu.fill_fetch_cursor_after(cpu.eip, backing, 1, window as u32);
    }

    /// Compilation/admission runs only after a cold entry has earned a trial.
    /// Keep allocation, page scans and eviction out of the dispatch function.
    #[inline(never)]
    fn submit_entry(
        &mut self,
        cpu: &mut Cpu,
        bus: &mut HostBus,
        key: (u32, u32),
        offset: usize,
        existing: Option<usize>,
        reused: bool,
        burst: bool,
    ) {
        let backing = key.0;
        let step = self.stats.native_steps + self.stats.interpreted_steps;
        let threshold = u32::from(self.threshold);
        let Some(slot) = self
            .regions
            .iter()
            .position(Option::is_none)
            .or_else(|| self.victim())
        else {
            return;
        };
        // A prospective page allocation lives in compiler scratch until
        // admission succeeds. Moving its Box into Page preserves the address.
        let fresh_entries = existing
            .is_none()
            .then(|| vec![0u32; 4096].into_boxed_slice());
        let bytes = bus.jit_code_bytes(u64::from(backing), 4096);
        let known = existing.map_or(&[][..], |page| {
            &self.page_slots[page].as_ref().unwrap().entries
        });
        // Entry frequency admits a root. Actual observed control flow bounds
        // its growth: reaching that root often does not qualify every adjacent
        // branch/call target. Keep a small page-local snapshot for this compile.
        let observations: Vec<_> = self
            .heat
            .iter()
            .filter_map(|heat| {
                let at = (heat.tag as u32).wrapping_sub(key.1) as usize;
                (heat.tag >> 32 == u64::from(backing)
                    && heat.count() != 0
                    && heat.age(step) <= HEAT_IDLE_LIMIT
                    && at < 4096
                    && known.get(at).is_none_or(|&entry| entry == 0))
                .then_some((heat.count(), at as u16))
            })
            .collect();
        let observed: Vec<_> = observations
            .iter()
            .filter_map(|&(count, at)| (count >= 2).then_some(at))
            .collect();
        // Amortize one module over cohot roots, not over arbitrary same-page
        // functions. The trigger remains first in the compiler's LIFO queue.
        let mut candidates: Vec<_> = observations
            .into_iter()
            .filter(|&(count, at)| count >= (threshold / 4).max(1) && usize::from(at) != offset)
            .collect();
        candidates.sort_unstable_by_key(|&(count, _)| core::cmp::Reverse(count));
        candidates.truncate(GROUP_FUNCTIONS - 1);
        let mut starts: Vec<_> = candidates.iter().rev().map(|&(_, at)| at).collect();
        starts.push(offset as u16);
        #[cfg(feature = "jit-profile")]
        let compiler_started = crate::browser::now_ms();
        let compiled = compile::batch(
            bytes,
            key.1,
            &starts,
            &observed,
            known,
            compile::Bindings {
                frame: (&*self.frame as *const Frame) as u32,
                entries: fresh_entries
                    .as_ref()
                    .map_or(known.as_ptr(), |v| v.as_ptr()) as u32,
                links: self.links.as_ptr() as u32,
                recency: (&self.recency[slot] as *const u32) as u32,
                slot: slot as u32,
            },
            core::arch::wasm32::memory_size(0) as u32,
        );
        #[cfg(feature = "jit-profile")]
        let compiler_elapsed = crate::browser::now_ms() - compiler_started;
        #[cfg(feature = "jit-profile")]
        {
            self.stats.compiler_ms += compiler_elapsed;
        }
        let Some(compiled) = compiled else {
            return;
        };
        if compiled.bytes.len() > MAX_MODULE_BYTES {
            return;
        }
        // Include source snapshots and the host's per-module overhead, with
        // conservative native expansion. V8 scratch is reserved separately.
        let charge = (compiled.bytes.len() * 6).div_ceil(16 * 1024) * (16 * 1024) + 16 * 1024;
        let mut content = CodeContent::new();
        for &(at, length) in &compiled.dependencies {
            let at = usize::from(at);
            content.record(at, &bytes[at..at + usize::from(length)]);
        }
        if !self.reserve(
            charge + if existing.is_none() { PAGE_CHARGE } else { 0 },
            existing,
        ) {
            return;
        }
        let page_id = if let Some(page_id) = existing {
            page_id
        } else {
            while self.page_slots.iter().all(Option::is_some) {
                if let Some(page) = self.page_victim(None) {
                    self.retire_page(page, RetireReason::Capacity);
                } else {
                    let Some(slot) = self.victim() else {
                        return;
                    };
                    self.retire(slot, RetireReason::Capacity);
                }
            }
            let page_id = self.page_slots.iter().position(Option::is_none).unwrap();
            let token = BackingToken(bus.jit_code_bytes(u64::from(backing), 1).as_ptr() as u64);
            let (_, version, version_ptr) = bus.jit_code_stamp(cpu, token);
            self.page_slots[page_id] = Some(Page {
                key,
                version,
                version_ptr,
                recent: true,
                entries: fresh_entries.unwrap(),
                regions: Vec::new(),
            });
            self.pages.insert(key, page_id);
            self.cache_page(key, Some(page_id));
            self.charge(PAGE_CHARGE);
            page_id
        };
        // The core is exclusive: reserve may have freed this selected slot,
        // but cannot have installed a different module in it.
        if self.regions[slot].is_some() {
            self.retire(slot, RetireReason::Capacity);
        }
        self.request = self
            .request
            .checked_add(1)
            .expect("Wasm compilation identity exhausted");
        // The import constructs a bounded module but cannot publish or reenter
        // the core; the host serializes publication after this batch yields.
        #[cfg(feature = "jit-profile")]
        let host_started = crate::browser::now_ms();
        let accepted = unsafe {
            jit_compile(
                slot as u32,
                self.request,
                compiled.bytes.as_ptr(),
                compiled.bytes.len() as u32,
            )
        };
        if accepted == 0 {
            return;
        }
        #[cfg(feature = "jit-profile")]
        self.trace.push(
            self.stats.native_steps,
            self.stats.interpreted_steps,
            8,
            [
                u64::from(self.request),
                (compiler_elapsed * 1e6) as u64,
                ((crate::browser::now_ms() - host_started) * 1e6) as u64,
                0,
                0,
                0,
                0,
                0,
            ],
        );
        #[cfg(feature = "jit-profile")]
        {
            let mut mature = [(0, 0); GROUP_FUNCTIONS - 1];
            let mut warm = mature;
            let mut mature_len = 0;
            let mut warm_len = 0;
            for (heat_index, heat) in self.heat.iter().enumerate() {
                if heat.count() < (threshold / 4).max(1) || heat.age(step) > HEAT_IDLE_LIMIT {
                    continue;
                }
                let other = ((heat.tag >> 32) as u32, heat.origin);
                let at = (heat.tag as u32).wrapping_sub(heat.origin) as usize;
                if other == key
                    || at >= 4096
                    || self
                        .pages
                        .get(&other)
                        .is_some_and(|&id| self.page_slots[id].as_ref().unwrap().entries[at] != 0)
                {
                    continue;
                }
                if warm_len < warm.len() && !warm[..warm_len].contains(&other) {
                    warm[warm_len] = other;
                    warm_len += 1;
                }
                if (heat.count() >= threshold * 4
                    || (heat.count() >= threshold + (threshold / 4).max(1)
                        && step - self.candidate_at[heat_index] >= ADMISSION_DELAY))
                    && mature_len < mature.len()
                    && !mature[..mature_len].contains(&other)
                {
                    mature[mature_len] = other;
                    mature_len += 1;
                }
            }
            self.stats.mature_companions[mature_len] += 1;
            self.stats.warm_companions[warm_len] += 1;
        }
        self.charge(charge);
        self.stats.compiled += 1;
        self.stats.admitted_reuse += u64::from(reused);
        self.stats.admitted_burst += u64::from(!reused && burst);
        self.stats.admitted_after_delay += u64::from(!reused && !burst);
        let version = self.page_slots[page_id].as_ref().unwrap().version;
        for &at in &compiled.function_starts {
            let pc = key.1.wrapping_add(u32::from(at));
            let tag = (u64::from(backing) << 32) | u64::from(pc);
            let index = Self::heat_index(backing, pc) & (RETIRED_ROOT_SLOTS - 1);
            let old = &mut self.retired_roots[index];
            if old.valid && old.key == tag {
                self.stats.recent_capacity_recompiles += 1;
                self.stats.same_version_recompiles += u64::from(old.version == version);
                old.valid = false;
            }
        }
        self.stats.generated_functions += compiled.function_starts.len() as u64;
        self.stats.deferred_edges += compiled.deferred_edges as u64;
        for &(at, _) in &compiled.entries {
            let pc = key.1.wrapping_add(u32::from(at));
            let heat = &mut self.heat[Self::heat_index(backing, pc)];
            if heat.tag == (u64::from(backing) << 32) | u64::from(pc) {
                heat.clear_count();
            }
        }
        self.stats.module_bytes += compiled.bytes.len() as u64;
        self.stats.split_functions += compiled.splits as u64;
        #[cfg(feature = "jit-profile")]
        self.trace.push(
            self.stats.native_steps,
            self.stats.interpreted_steps,
            3,
            [
                u64::from(self.request),
                slot as u64,
                (u64::from(backing) << 32) | u64::from(cpu.eip),
                compiled.bytes.len() as u64,
                charge as u64,
                compiled.instructions as u64,
                compiled.function_starts.len() as u64,
                if reused {
                    3
                } else if burst {
                    2
                } else {
                    1
                },
            ],
        );
        self.pending = Some(Pending {
            slot,
            request: self.request,
            region: Region {
                #[cfg(feature = "jit-profile")]
                trace_request: self.request,
                page: page_id,
                function: 0,
                charge,
                entries: compiled.entries,
                content,
                function_starts: compiled.function_starts,
            },
        });
    }
    pub(crate) fn run_batch(
        &mut self,
        cpu: &mut Cpu,
        bus: &mut HostBus,
        start: u64,
        budget: u64,
    ) -> Result<BatchResult, String> {
        bus.sync_ioram(cpu);
        let mut ran = 0;
        // The packed timestamp covers 17.6 trillion guest instructions. Drop
        // hints on its epoch change; this is policy only, never execute proof.
        let epoch = (self.stats.native_steps + self.stats.interpreted_steps) >> HEAT_TIME_BITS;
        if self.heat_epoch != epoch {
            self.heat.fill(Heat::default());
            self.candidate_at.fill(0);
            self.heat_epoch = epoch;
        }
        let request = self.request;
        self.transient = None;
        // A dispatch-only return preserves segment/privilege/mapping inputs
        // and the arithmetic record. Generated code cannot change that context:
        // segment/CR/CPL transitions use interpretation, POPF rejects TF, and
        // STI/IRQ attention returns to Machine. Interpretation or any Machine
        // return ends the proof; it never survives external event handling.
        let mut native_context = false;
        while ran < budget {
            let mut handoff = false;
            let mut repeat = None;
            #[cfg(feature = "jit-profile")]
            let mut miss_kind = 9; // cold/unqualified; generated exit 7 is a segment load
            let mut uncached_miss = false;
            let eligible = native_context || {
                let (read, write) = cpu.jit_segment_masks();
                !cpu.halted
                    && !cpu.interrupt_shadow()
                    && !cpu.code_default16()
                    && cpu.seg(SegmentReg::Cs).limit == u32::MAX
                    && cpu.eflags & ((1 << 8) | (1 << 17)) == 0
                    && (read | (write << 8)) & ENTRY_SEGMENTS == ENTRY_SEGMENTS
            };
            if eligible {
                if !native_context {
                    let context = (cpu.jit_mapping_epoch(), cpu.seg(SegmentReg::Cs).base);
                    let invalidated = cpu.take_jit_invalidated_range();
                    if self.context != Some(context) {
                        self.links.fill(Link::default());
                        self.context = Some(context);
                    } else if let Some((base, mask)) = invalidated {
                        // Match the shared cache's architectural invalidation,
                        // including all pieces of a cached large-page mapping.
                        // The ordinary page affects at most its direct-mapped slot.
                        if mask == 0xffff_f000 {
                            let slot = (base as usize >> 12) & (LINK_SLOTS - 1);
                            for plane in 0..LINK_PLANES {
                                let link = &mut self.links[plane * LINK_SLOTS + slot];
                                if link.tag & mask == base {
                                    link.tag = 0;
                                }
                            }
                        } else {
                            for link in self.links.iter_mut() {
                                if link.tag & mask == base {
                                    link.tag = 0;
                                }
                            }
                        }
                    }
                }
                if let Some((function, entry)) = self.lookup(cpu, bus) {
                    // Cold interpretation computes its own event limit.
                    let (limit, attention) = bus
                        .batch_attention_limit(start + ran, (budget - ran).min(u64::from(u32::MAX)))
                        .unwrap_or((1, false));
                    handoff = true;
                    cpu.begin_instruction();
                    cpu.segment_override = None;
                    let frame = &mut *self.frame;
                    if !native_context {
                        frame.prepare(cpu);
                    } else {
                        frame.refill_intent = 0;
                    }
                    // Rust's Wasm32 C function pointers are indices in the
                    // exported indirect-function table. JS installs only this
                    // emitter's validated integer-state export into our slots.
                    // All pointers are live for the exclusive invocation.
                    let invoke: unsafe extern "C" fn(
                        u32,
                        u32,
                        u32,
                        u32,
                        u32,
                        u32,
                        u32,
                        u32,
                        u32,
                        u32,
                        u32,
                        u32,
                        u32,
                        u32,
                    ) = unsafe { core::mem::transmute(function) };
                    let r = cpu.regs;
                    let (flags, lhs, result, kind) = cpu.jit_arithmetic();
                    #[cfg(feature = "jit-profile")]
                    let started = (self.stats.invocations & 255 == 0).then(crate::browser::now_ms);
                    #[cfg(feature = "jit-profile")]
                    let entered_pc = cpu.eip;
                    // Generated modules exclusively borrow these ABI rows and
                    // cannot call Rust or devices. Save the current RAM row
                    // each time: code qualification between invocations may
                    // revoke its write proof. Restore before any Rust cache
                    // access, including the resident-miss probe below.
                    let saved = self.transient.as_ref().map(|page| unsafe {
                        let slot = (frame.tlb as *mut LinearTranslationCacheEntry).add(page.index);
                        (slot, slot.replace(page.row))
                    });
                    unsafe {
                        invoke(
                            entry,
                            r[0],
                            r[1],
                            r[2],
                            r[3],
                            r[4],
                            r[5],
                            r[6],
                            r[7],
                            flags,
                            limit as u32,
                            lhs,
                            result,
                            kind,
                        )
                    };
                    if let Some((slot, row)) = saved {
                        unsafe { slot.write(row) };
                    }
                    #[cfg(feature = "jit-profile")]
                    if let Some(started) = started {
                        let elapsed = crate::browser::now_ms() - started;
                        self.stats.sampled_native_ms += elapsed;
                        self.trace.push(
                            self.stats.native_steps,
                            self.stats.interpreted_steps,
                            7,
                            [
                                u64::from(entered_pc),
                                function as u64,
                                entry as u64,
                                limit - u64::from(frame.remaining),
                                (elapsed * 1e6) as u64,
                                u64::from(frame.exit),
                                0,
                                0,
                            ],
                        );
                    }
                    assert!(
                        frame.remaining <= limit as u32 && frame.exit <= 7,
                        "invalid generated exit"
                    );
                    let mut completed = limit - u64::from(frame.remaining);
                    cpu.regs = frame.regs;
                    cpu.eip = frame.eip;
                    frame.commit_flags(cpu);
                    native_context = true;
                    let segment_loaded = if frame.exit == 7 {
                        let load = &frame.segment_load;
                        assert!(
                            matches!(load.segment, 0 | 3..=5) && frame.remaining != 0,
                            "invalid generated segment load"
                        );
                        if crate::x86::interp::try_load_cached_segment(
                            cpu,
                            bus,
                            SegmentReg::from_modrm_reg(load.segment as u8),
                            load.selector as u16,
                        ) {
                            cpu.eip = load.next_eip;
                            completed += 1;
                            // DS/ES can become non-flat or unusable. Recheck
                            // eligibility and refresh all segment inputs before
                            // another invocation; never follow a stale proof.
                            native_context = false;
                            self.transient = None;
                            self.stats.cached_segment_loads += 1;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if frame.refill_intent != 0 {
                        #[cfg(feature = "jit-profile")]
                        {
                            use crate::x86::cpu::LINEAR_TRANSLATION_USER;
                            let (table, state) = cpu.translation_cache.jit_view();
                            let index = (frame.refill_linear as usize >> 12)
                                & ((1 << LINEAR_TRANSLATION_CACHE_BITS) - 1);
                            let row = unsafe { *table.add(index) };
                            let expected = (frame.refill_linear & !4095) | state as u32;
                            let cause = if row.tag == 0 {
                                0
                            } else if row.tag ^ expected == LINEAR_TRANSLATION_USER {
                                1
                            } else if row.tag == expected {
                                2
                            } else if (row.tag ^ expected) & !4095 != 0 {
                                3
                            } else {
                                4
                            };
                            self.stats.data_miss_causes[cause] += 1;
                        }
                        // A native TLB miss has committed only the completed
                        // prefix. Warm a resident RAM translation here before
                        // shared interpretation performs this instruction.
                        // This probe has no fault, A/D or guest-visible I/O effects.
                        let refill = crate::x86::mmu::try_resident_data_translation(
                            cpu,
                            bus,
                            LinearAddr(frame.refill_linear & !4095),
                            frame.refill_intent == 2,
                        );
                        uncached_miss = refill.is_some()
                            && cpu
                                .cached_read_translation(
                                    LinearAddr(frame.refill_linear),
                                    cpu.cpl() == 3,
                                )
                                .is_none();
                        #[cfg(feature = "jit-profile")]
                        {
                            let kind = if refill.is_none() {
                                0
                            } else if uncached_miss {
                                2
                            } else {
                                1
                            };
                            self.stats.resident_refills[kind] += 1;
                        }
                    }
                    cpu.advance_tsc(completed);
                    ran += completed;
                    self.stats.native_steps += completed;
                    self.stats.invocations += 1;
                    #[cfg(feature = "jit-profile")]
                    {
                        self.stats.exits[frame.exit as usize] += 1;
                        if frame.exit == 0
                            && self.stats.invocations & 255 == 0
                            && self.misses.len() < 32768
                        {
                            let item = self.misses.entry((frame.eip, 8)).or_insert((0, [0; 15]));
                            item.0 += 1;
                        }
                    }
                    self.stats.memory_exits += u64::from(frame.exit == 1);
                    self.stats.semantic_exits +=
                        u64::from(frame.exit == 5 || (frame.exit == 7 && !segment_loaded));
                    if frame.exit == 6 {
                        self.stats.repeat_exits += 1;
                        self.stats.partial_repeat_elements += u64::from(frame.repeat.completed);
                        repeat = Some(core::mem::take(&mut frame.repeat));
                    }
                    #[cfg(feature = "jit-profile")]
                    {
                        miss_kind = frame.exit as u8;
                    }
                    self.stats.zero_step_exits += u64::from(completed == 0);
                    self.stats.short_budget_exits += u64::from(completed == 0 && frame.exit == 2);
                    if completed != 0 {
                        bus.begin_instruction(start + ran - 1, cpu.tsc() - 1);
                    }
                    if frame.exit == 4 {
                        cpu.inhibit_maskable_interrupts();
                    }
                    if matches!(frame.exit, 3 | 4) || (completed == limit && attention) {
                        return Ok(BatchResult {
                            ran,
                            stop: BatchStop::Attention {
                                instruction_step: start + ran - 1,
                                was_interruptible: false,
                            },
                        });
                    }
                    if completed == limit || (completed != 0 && frame.exit == 0) || segment_loaded {
                        continue;
                    }
                }
                if self.request != request {
                    return Ok(BatchResult {
                        ran,
                        stop: BatchStop::Budget,
                    });
                }
            }
            #[cfg(feature = "jit-profile")]
            {
                self.samples += 1;
            }
            #[cfg(feature = "jit-profile")]
            if eligible && self.samples & 255 == 0 {
                let pc = cpu.eip;
                if let Some(item) = self.misses.get_mut(&(pc, miss_kind)) {
                    item.0 += 1;
                } else if self.misses.len() < 32768 {
                    let linear = cpu.seg(SegmentReg::Cs).base.wrapping_add(pc);
                    if let Some(hit) = cpu.fetch_cache_lookup(LinearAddr(linear)) {
                        let backing = u64::from(bus.jit_backing_offset(hit.backing));
                        let mut bytes = [0; 15];
                        let len = (4096 - (linear as usize & 4095)).min(15);
                        bytes[..len].copy_from_slice(bus.jit_code_bytes(backing, len));
                        self.misses.insert((pc, miss_kind), (1, bytes));
                    }
                }
            }
            // A cold trace stops on control transfer. Establish execute access
            // with one instruction first if the backing page is not qualified.
            if eligible {
                self.prepare_interpreter_fetch(cpu);
            }
            native_context = false;
            self.transient = None;
            #[cfg(feature = "jit-profile")]
            let started = (self.samples & 255 == 0).then(crate::browser::now_ms);
            #[cfg(feature = "jit-profile")]
            let interpreted_pc = (cpu.eip, cpu.cr3);
            let batch = if let Some(repeat) = repeat {
                Interpreter::resume_repeat(cpu, bus, start + ran, &repeat)
            } else if handoff || (eligible && !self.qualified) {
                Interpreter::run_one(cpu, bus, start + ran)
            } else if eligible {
                Interpreter::run_until_control(
                    cpu,
                    bus,
                    start + ran,
                    (budget - ran).min(256),
                    &mut self.cold_entries,
                )
            } else {
                Interpreter::run_batch(cpu, bus, start + ran, budget - ran)
            };
            #[cfg(feature = "jit-profile")]
            if let Some(started) = started {
                let elapsed = crate::browser::now_ms() - started;
                self.stats.sampled_interpreter_ms += elapsed;
                self.trace.push(
                    self.stats.native_steps,
                    self.stats.interpreted_steps,
                    6,
                    [
                        u64::from(interpreted_pc.0),
                        u64::from(interpreted_pc.1),
                        batch.ran,
                        (elapsed * 1e6) as u64,
                        u64::from(miss_kind),
                        u64::from(eligible),
                        0,
                        0,
                    ],
                );
                if uncached_miss {
                    self.stats.sampled_uncached_interpreter_ms += elapsed;
                }
            }
            ran += batch.ran;
            self.stats.interpreted_steps += batch.ran;
            if !eligible {
                self.stats.ineligible_steps += batch.ran;
            }
            if !matches!(batch.stop, BatchStop::Budget) {
                return Ok(BatchResult {
                    ran,
                    stop: batch.stop,
                });
            }
            if uncached_miss {
                // Interpretation can change translations, PCI decode or the
                // VGA dirty state. Requalify in the resulting state instead
                // of retaining the pointer obtained before that instruction.
                // A write proof marks the entire page dirty. No device/event
                // callback runs during subsequent generated execution.
                let linear = LinearAddr(self.frame.refill_linear & !4095);
                let write = self.frame.refill_intent == 2;
                if let Some(token) =
                    crate::x86::mmu::try_resident_data_translation(cpu, bus, linear, write)
                    && cpu
                        .cached_read_translation(linear, cpu.cpl() == 3)
                        .is_none()
                {
                    let tag = linear.0 | cpu.translation_cache.jit_view().1 as u32;
                    self.transient = Some(TransientPage {
                        index: (linear.0 as usize >> 12)
                            & ((1 << LINEAR_TRANSLATION_CACHE_BITS) - 1),
                        row: LinearTranslationCacheEntry {
                            bias: u64::from((token.0 as u32).wrapping_sub(linear.0)),
                            tag,
                            write_tag: if write { tag } else { 0 },
                        },
                    });
                }
            }
        }
        Ok(BatchResult {
            ran,
            stop: BatchStop::Budget,
        })
    }
}
