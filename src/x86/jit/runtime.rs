//! AArch64 invocation/lookup owner shared by Linux and the EL1 Machine monitor.
//! The code arena selects executable mappings at build time; x86 semantics,
//! admission, invalidation and event deadlines are identical on both hosts.
//!
//! Lookups require a valid shared execute translation and the recorded backing
//! page's content version. Guarded native blocks cannot change code or mappings
//! internally. Writable aliases are revoked on code-page promotion; scalar,
//! token, page-walk, bulk and device RAM writes notify the version owner. Loaders
//! and reset discard all derived code. The validity module is also usable by a
//! future HardMMU protection handler. No future page walk is done by lookahead.
//! A renewed watch may retain a page's compiled entries only after comparing
//! every byte used by its native and negative entries. Retain or retire the
//! whole page together: a same-page direct branch can reach any of those bodies.
//! This comparison follows execute revalidation and writable-alias revocation;
//! it never restores old version tokens or survives the owning process.
//! Execute revalidation is per page: the previous per-PC tags caused about
//! 88 million context-related exits in the firmware workload. Restoring all
//! entries of a revalidated page reduced a development cold boot from 48.1 to
//! 32.5 seconds on the RPi, without changing its 1,975,715,720 guest steps.
//! These single-run diagnostics justify the structure, not final acceptance.
//!
//! Unsupported instructions execute once through the interpreter at their exact
//! PC, with the existing fault and attempted-tick behavior. Compiler/publication
//! failures are errors, never an excuse to silently switch engines.
//!
//! Build r86-run with `--features jit` for aarch64-unknown-linux-musl (using
//! RUSTFLAGS='-C linker=rust-lld' when cross compiling), then select
//! `--cpu-engine jit` or `cpu-engine = 'jit'` in TOML. Explicit CLI wins. The
//! CLI defaults to JIT on supported ARM64 Linux builds, otherwise interpretation.
//! Statistics distinguish native steps from interpreter boundaries; a successful
//! boot alone is not JIT validation.
//! The no_std Machine monitor selects this same engine with `monitor,machine,jit`.
//! Its paged generated reads use protected shadow windows and exact native-PC
//! recovery. Qualified SS stores/RMW also use hardware windows; other writes
//! retain SoftMMU qualification. JIT read-abort counts are separate from
//! interpreter HardMMU counters. Unpaged EL1 blocks use
//! the shared software qualification, with the mode included in their code key.

use super::code::{CodeArena, CodeError, CodeHandle, ENTRY_TABLES, TableIndex};
use super::validity::CodeContent;
use super::{
    compile, decode,
    frame::Frame,
    links::{self, COLD, INTERPRET, Link, PAGE_BYTES, SLOTS},
    soft::View,
};
use crate::machine::HostBus;
use crate::x86::cpu::{FetchCacheHit, SegmentReg, TranslationRecheck};
use crate::x86::{
    BackingToken, BatchResult, BatchStop, Bus, Cpu, Interpreter, LinearAddr, PhysAddr,
};
use alloc::{boxed::Box, format, string::String, vec, vec::Vec};
use core::hash::{BuildHasherDefault, Hasher};
#[cfg(target_os = "none")]
use hashbrown::HashMap;
#[cfg(target_os = "linux")]
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

struct CompiledPage {
    version: u64,
    content: Option<Box<CodeContent>>,
    // Stable arena table; live links can address every possible x86 byte entry.
    // Zero is unseen, one is interpreted, two/three count cold visits; larger
    // values are signed byte offsets from the table's RX address. Code precedes
    // metadata in the same bounded mapping, so native displacements are negative
    // and cannot collide with sentinels. Cold entries use ordinary semantics.
    entries: TableIndex,
    // Native-PC metadata retains its associated emitted body address.
    #[cfg(target_os = "linux")]
    faults: Vec<(usize, Box<[super::memory::FaultSite]>)>,
    pending_edges: Vec<(usize, u16)>, // RX branch site and target byte offset.
    #[cfg(feature = "jit-profile")]
    opcodes: Box<[u16]>,
}

impl CompiledPage {
    fn new(version: u64, entries: TableIndex) -> Self {
        Self {
            version,
            content: None,
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
        arena.entries_mut(self.entries).fill(0);
        #[cfg(target_os = "linux")]
        self.faults.clear();
        self.pending_edges.clear();
        match &mut self.content {
            Some(content) => {
                content.clear();
            }
            None => self.content = Some(CodeContent::new()),
        }
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
unsafe extern "C" fn load_cached_segment(frame: *mut Frame, segment: u32, selector: u32) -> u32 {
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
            &mut *view.bus.cast::<HostBus>(),
            segment,
            selector as u16,
        ) {
            return 0;
        }
        frame.selectors[segment as usize] = selector as u16;
        if view.refresh_segments(cpu) { 1 } else { 2 }
    }
}

#[derive(Clone, Copy, Default)]
struct CodeStamp {
    version_ptr: *const u64,
    version: u64,
    phys_page: u64,
    backing_page: u64,
}

#[derive(Default, Debug)]
pub struct Stats {
    pub native_steps: u64,
    pub interpreted_steps: u64,
    pub compiled_blocks: u64,
    pub content_changes: u64,
    #[cfg(any(test, feature = "jit-profile"))]
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
    #[cfg(target_os = "none")]
    pub hard_read_aborts: u64,
    #[cfg(target_os = "none")]
    pub hard_write_aborts: u64,
    #[cfg(target_os = "none")]
    pub hard_refills: u64,
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

pub struct Engine {
    arena: CodeArena,
    #[cfg(target_os = "none")]
    hard_faults: Vec<super::memory::NativeFault>,
    entry: Entry,
    // Key: EIP at the linear page's start, canonical physical backing page,
    // flat DS/ES/SS facts and the EL1 paged/unpaged memory lowering.
    // This also handles nonzero/unaligned CS bases and different RAM aliases.
    pages: HashMap<(u64, u64), CompiledPage, BuildHasherDefault<PageHasher>>,
    // Both privilege planes have fixed bounds. Keep that ownership invariant
    // in the types so a masked page index needs no dynamic slice-length check.
    links: Box<[Link; 2 * SLOTS]>,
    user_links: bool,
    // Content stamps are read only after a code write, outside native execution.
    // Keeping them out of Link halves the generated dispatch table's footprint.
    stamps: Box<[CodeStamp; 2 * SLOTS]>,
    rechecks: Box<[TranslationRecheck; 2 * SLOTS]>,
    returns: Box<links::ReturnStack>,
    cs_base: u32,
    block_facts: u32,
    code_epoch: u64,
    mapping_epoch: Option<u64>,
    #[cfg(feature = "jit-profile")]
    previous_cr3: u32,
    global_epoch: u64,
    global_links: [u64; 2 * SLOTS / 64],
    link_context: u32,
    #[cfg(feature = "jit-profile")]
    unsupported: Box<[u64; 576]>,
    pub stats: Stats,
}

impl Engine {
    pub fn new() -> Result<Self, String> {
        // The firmware workload repeatedly outgrew 32 MiB, recompiling more
        // than 1 GiB cumulatively. Reserve 128 MiB; pages are committed only as
        // published, and both executable bytes and metadata remain bounded.
        // This sizing choice does not establish a sufficient working set for
        // long-lived guests. Either this arena or the 4096-page metadata limit
        // below reclaims everything; measure recompilation under process/page
        // churn before choosing finer-grained reclamation. Validity must remain
        // correct regardless of whether an entire boot fits in the cache.
        Self::with_capacity(128 * 1024 * 1024)
    }

    pub(crate) fn with_capacity(bytes: usize) -> Result<Self, String> {
        let mut arena =
            CodeArena::new(bytes).map_err(|error| format!("JIT code mapping: {error:?}"))?;
        let entry = Self::publish_entry(&mut arena)?;
        Ok(Self {
            arena,
            #[cfg(target_os = "none")]
            hard_faults: Vec::new(),
            entry,
            pages: HashMap::default(),
            links: vec![Link::EMPTY; 2 * SLOTS]
                .into_boxed_slice()
                .try_into()
                .ok()
                .expect("fixed link count"),
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
            cs_base: 0,
            block_facts: 0,
            code_epoch: 0,
            mapping_epoch: None,
            #[cfg(feature = "jit-profile")]
            previous_cr3: 0,
            global_epoch: 0,
            global_links: [0; 2 * SLOTS / 64],
            link_context: 1,
            #[cfg(feature = "jit-profile")]
            unsupported: Box::new([0; 576]),
            stats: Stats::default(),
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
        self.links.fill(Link::EMPTY);
        self.global_links.fill(0);
        self.pages.clear();
        #[cfg(target_os = "none")]
        self.hard_faults.clear();
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
        let result = format!("jit: {:?}\n", self.stats);
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

    fn sync_mapping(&mut self, cpu: &mut Cpu) {
        // Every linked block was compiled for these segment and memory-mode facts.
        // Clear both privilege planes: an execute-walk recheck cannot certify
        // compiled assumptions. Bases/limits not covered by flat qualification
        // are still loaded and checked from each invocation's memory view.
        let permissions = View::block_facts(cpu);
        if self.block_facts != permissions {
            self.links.fill(Link::EMPTY);
            self.block_facts = permissions;
        }
        // Native absolute branch PCs are CS-relative. Paging rechecks alone
        // cannot certify code compiled with another CS base.
        if self.cs_base != cpu.seg(SegmentReg::Cs).base {
            self.links.fill(Link::EMPTY);
            self.cs_base = cpu.seg(SegmentReg::Cs).base;
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
        let old_context = self.link_context;
        self.link_context = self.link_context.wrapping_add(1);
        if self.link_context == 0 {
            self.links.fill(Link::EMPTY);
            self.link_context = 1;
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
                    if link.tag >> 32 == u64::from(old_context) {
                        link.tag = (u64::from(self.link_context) << 32) | (link.tag & 0xffff_ffff);
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
        version: u64,
        version_ptr: *const u64,
        entries: *const u32,
        global: bool,
        recheck: TranslationRecheck,
        fetch: FetchCacheHit,
    ) {
        let slot = usize::from(self.user_links) * SLOTS + links::slot(linear);
        self.links[slot] = Link {
            tag: (u64::from(self.link_context) << 32) | u64::from(linear & !4095),
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
        let single = bus.jit_single_retired_version(self.code_epoch);
        self.code_epoch = epoch;
        if let Some(version_ptr) = single {
            // SAFETY: CodePages just supplied its stable, live version cell.
            // One retirement since our last scan touched only this backing
            // page. All linear/privilege aliases share that cell; compare its
            // current version once instead of loading every published version.
            let version = unsafe { *version_ptr };
            for (link, stamp) in self.links.iter_mut().zip(self.stamps.iter()) {
                if stamp.version_ptr == version_ptr && stamp.version != version {
                    *link = Link::EMPTY;
                }
            }
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

    fn prepare_interpreter_fetch(&self, cpu: &mut Cpu) {
        if cpu.fetch_cursor_backing_u8(cpu.eip).is_some() {
            return;
        }
        let linear = LinearAddr(self.cs_base.wrapping_add(cpu.eip));
        let slot = usize::from(self.user_links) * SLOTS + links::slot(linear.0);
        if self.links[slot].tag
            != (u64::from(self.link_context) << 32) | u64::from(linear.0 & !4095)
        {
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
        let window = (PAGE_BYTES as u64 - offset).min(u64::from(u32::MAX - cpu.eip) + 1);
        cpu.fill_fetch_cursor_after(cpu.eip, backing, 1, window as u32);
    }

    fn lookup(&mut self, cpu: &mut Cpu, bus: &mut HostBus) -> Result<Option<NativeBlock>, String> {
        self.sync_mapping(cpu);
        self.sync_code(bus);
        let linear = LinearAddr(cpu.seg(SegmentReg::Cs).base.wrapping_add(cpu.eip));
        let offset = (linear.0 & 4095) as usize;
        let link = self.links[usize::from(self.user_links) * SLOTS + links::slot(linear.0)];
        if link.tag == (u64::from(self.link_context) << 32) | u64::from(linear.0 & !4095) {
            // SAFETY: matching page tag owns a PAGE_BYTES-entry allocation;
            // sync_code retired any changed contents before this lookup.
            // clear drops the allocation only after invalidating all links.
            let entry = unsafe { *link.entries.add(offset) };
            if entry > COLD {
                return Ok(Some(NativeBlock {
                    body: relative_entry(link.entries, entry),
                }));
            }
            // Opcode diagnostics intentionally revisit the negative cache.
            #[cfg(not(feature = "jit-profile"))]
            if entry == INTERPRET {
                return Ok(None);
            }
        }
        bus.instruction_address(linear.0);
        // A miss remains an architectural fetch. Do not fill a future code page
        // or mutate page-table A bits while merely exploring a possible block.
        let Some(hit) = cpu.fetch_cache_lookup(linear) else {
            return Ok(None);
        };
        let global = cpu.fetch_translation_is_global(linear);
        let recheck = cpu.fetch_recheck(linear);
        let window = (PAGE_BYTES - offset).min(480);
        let window = window.min((u32::MAX - cpu.eip) as usize + 1);
        let (backing, mut version, mut version_ptr) = bus.jit_code_stamp(cpu, hit.backing);
        // Preserve the full EIP origin and segment qualifications in one word.
        // Lookup hashes and compares two u64s instead of three scalar fields;
        // no qualification bits are discarded, including unaligned CS bases.
        let key = (
            u64::from(cpu.eip.wrapping_sub(offset as u32)) | (u64::from(self.block_facts) << 32),
            backing - offset as u64,
        );
        let (page_entries, entry) =
            if let Some(page) = self.pages.get_mut(&key) {
                if page.version != version {
                    if page.content.as_ref().is_some_and(|content| {
                        content.matches(bus.jit_code_bytes(key.1, content.end))
                    }) {
                        // sync_code retired the old links; jit_code_stamp renewed
                        // the watch and revoked writable aliases before this proof.
                        page.version = version;
                        #[cfg(any(test, feature = "jit-profile"))]
                        {
                            self.stats.content_reuses += 1;
                        }
                    } else {
                        page.invalidate(version, &mut self.arena);
                        self.stats.content_changes += 1;
                    }
                }
                let entry = self.arena.entries(page.entries)[offset];
                if entry == INTERPRET || entry > COLD {
                    #[cfg(feature = "jit-profile")]
                    if entry == INTERPRET {
                        self.unsupported[usize::from(page.opcodes[offset])] += 1;
                    }
                    let entries = self.arena.entries_pointer(page.entries);
                    self.link(
                        linear.0,
                        version,
                        version_ptr,
                        entries,
                        global,
                        recheck,
                        hit,
                    );
                    return Ok((entry > COLD).then_some(NativeBlock {
                        body: relative_entry(entries, entry),
                    }));
                }
                // Admission reuses this table and word without another lookup.
                // No mutation or arena reclamation can occur before their use.
                (page.entries, entry)
            } else {
                if self.pages.len() >= ENTRY_TABLES {
                    self.stats.table_clears += 1;
                    // Bound the second-level tables to 64 MiB, alongside the bounded
                    // native arena. Negative entries need no per-instruction allocation.
                    self.clear(bus)?;
                    (_, version, version_ptr) = bus.jit_code_stamp(cpu, hit.backing);
                }
                let page = self
                    .pages
                    .entry(key)
                    .or_insert_with(|| CompiledPage::new(version, self.arena.allocate_entries()));
                // allocate_entries zeroes every word of this new page's table.
                (page.entries, 0)
            };
        if entry < COLD {
            // Compilation and publication cost are part of a cold boot. Wait
            // for a third visit before emitting a block; cold passes retain
            // ordinary interpretation and architectural fetch behavior.
            self.arena.entries_mut(page_entries)[offset] = entry.max(1) + 1;
            let entries = self.arena.entries_pointer(page_entries);
            self.link(
                linear.0,
                version,
                version_ptr,
                entries,
                global,
                recheck,
                hit,
            );
            return Ok(None);
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
        let decoded = decode::block(bus.jit_code_bytes(backing, window), cpu.eip, 32);
        let (native, _faults, edges) = if decoded.instructions.is_empty() {
            (None, Vec::new(), Vec::new())
        } else {
            let mut code = compile::block(
                &decoded,
                Some(links::Page {
                    eip_start: key.0 as u32,
                }),
                self.block_facts,
            )
            .map_err(|error| format!("JIT compiler at {:#x}: {error:?}", cpu.eip))?;
            let entry = match self.arena.publish_block(&mut code) {
                Err(CodeError::Capacity) => {
                    self.stats.capacity_clears += 1;
                    // No active native stack exists here; reclaiming an arena
                    // invalidates all handles, lookup entries and pending edges.
                    // Emitted blocks contain no old-arena/table addresses.
                    self.clear(bus)?;
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
            #[cfg(target_os = "none")]
            for (fault, recovery) in code.faults.iter().zip(&code.recoveries) {
                // The exact site owns the instruction's precise commit/exit.
                // SS stores may fault; software accesses cannot fault in the
                // selected hardware window and are excluded by FAR validation.
                if cpu.cr0 & crate::x86::cpu::CR0_PG == 0 {
                    continue;
                }
                let pc = body + fault.native_offset as usize;
                assert!(self.hard_faults.last().is_none_or(|last| last.pc < pc));
                self.hard_faults.push(super::memory::NativeFault {
                    pc,
                    // Bit 0 carries guest write intent, also for RMW loads.
                    // Native PCs are 4-byte aligned; the vector masks the bit.
                    recovery: (body + *recovery as usize) | ((fault.access as usize >> 8) & 1),
                });
            }
            (Some(NativeBlock { body }), code.faults, code.edges)
        };
        let page = self
            .pages
            .entry(key)
            .or_insert_with(|| CompiledPage::new(version, self.arena.allocate_entries()));
        if let Some(content) = &mut page.content {
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
        let entries = self.arena.entries_pointer(page.entries);
        self.arena.entries_mut(page.entries)[offset] = native.map_or(INTERPRET, |block| {
            i32::try_from(block.body as isize - entries as isize)
                .expect("bounded relative native entry") as u32
        });
        if let Some(block) = native {
            #[cfg(target_os = "linux")]
            page.faults.push((block.body, _faults.into_boxed_slice()));
            page.pending_edges.extend(
                edges
                    .into_iter()
                    .map(|edge| (block.body + edge.native_offset as usize, edge.target_offset)),
            );
        }
        // Every entry on this page has the same mapping/content validity.
        // A target is published once per version, so a resolved edge needs
        // no backlink/unpatch machinery: invalidation retires its source too.
        for index in (0..page.pending_edges.len()).rev() {
            let (source, offset) = page.pending_edges[index];
            let target = self.arena.entries(page.entries)[usize::from(offset)];
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
        self.link(
            linear.0,
            version,
            version_ptr,
            entries,
            global,
            recheck,
            hit,
        );
        Ok(native)
    }

    pub(crate) fn run_batch(
        &mut self,
        cpu: &mut Cpu,
        bus: &mut HostBus,
        start: u64,
        budget: u64,
    ) -> Result<BatchResult, String> {
        let mut ran = 0;
        while ran < budget {
            let mut repeat = None;
            #[cfg(feature = "jit-profile")]
            let mut write_token_miss = false;
            // These modes currently have no native lowering. Taking the normal
            // interpreter batch also avoids a slow per-instruction real-mode
            // path during BIOS startup. The next machine boundary retries JIT.
            let shadowed = cpu.interrupt_shadow();
            let eligible = !cpu.halted
                && !cpu.code_default16()
                && cpu.seg(SegmentReg::Cs).limit == u32::MAX
                && cpu.eflags & ((1 << 8) | (1 << 17)) == 0;
            let plan = bus.batch_attention_limit(start.wrapping_add(ran), budget - ran);
            let (limit, attention) = plan.unwrap_or((1, false));
            let native_limit = if shadowed { 1 } else { limit };
            let native_attention = attention || (shadowed && limit > 1);
            if eligible {
                if let Some(NativeBlock { body }) = self
                    .lookup(cpu, bus)?
                    .filter(|block| !shadowed || block.is_single_instruction())
                {
                    cpu.begin_instruction();
                    cpu.segment_override = None;
                    let mut frame = Frame::from_cpu(cpu, native_limit);
                    #[cfg(target_os = "none")]
                    {
                        frame.hard_read_base = if cpu.cr0 & crate::x86::cpu::CR0_PG != 0 {
                            bus.jit_prepare_hard_reads(cpu)
                        } else {
                            0 // This block has software-qualified unpaged reads.
                        };
                        frame.hard_faults = self.hard_faults.as_ptr();
                        frame.hard_fault_count = self.hard_faults.len();
                    }
                    frame.native_entry = body;
                    frame.dispatch_entry = self.entry.dispatch;
                    frame.call_dispatch_entry = self.entry.call_dispatch;
                    frame.return_entry = self.entry.finish;
                    frame.recheck_entry = self.entry.recheck;
                    let link_base = usize::from(self.user_links) * SLOTS;
                    frame.links = self.links[link_base..].as_mut_ptr();
                    frame.link_rechecks = self.rechecks[link_base..].as_ptr();
                    frame.link_context = u64::from(self.link_context) << 32;
                    frame.call_base = &mut *self.returns;
                    let mut memory = View::new(cpu, bus.jit_rechecks_enabled());
                    memory.bus = (bus as *mut HostBus).cast();
                    memory.refill = refill_data_translation as *const () as usize;
                    memory.load_segment = load_cached_segment as *const () as usize;
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
                    #[cfg(target_os = "none")]
                    {
                        self.stats.hard_refills += frame.hard_refills;
                    }
                    #[cfg(target_os = "none")]
                    if frame.hard_fault_address != 0 {
                        // Memory-source DIV shares the precise division exit;
                        // its operand fault must precede any divide exception.
                        assert!(matches!(frame.exit_kind, 1 | 3 | 4));
                        cpu.translation_cache
                            .discard_hard_access_miss(LinearAddr(frame.hard_fault_address as u32));
                        if frame.hard_fault_write != 0 {
                            self.stats.hard_write_aborts += 1;
                        } else {
                            self.stats.hard_read_aborts += 1;
                        }
                    }
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
                    #[cfg(feature = "jit-profile")]
                    {
                        self.stats.empty_invocations += u64::from(frame.completed == 0);
                        self.stats.budget_exits += u64::from(frame.exit_kind == 2);
                        if frame.exit_kind == 0 {
                            let linear = self.cs_base.wrapping_add(cpu.eip);
                            let link = self.links
                                [usize::from(self.user_links) * SLOTS + links::slot(linear)];
                            if link.tag != frame.link_context | u64::from(linear & !4095) {
                                self.stats.dispatch_misses += 1;
                                if link.tag >> 32 == frame.link_context >> 32 {
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
                        let linear = self.cs_base.wrapping_add(cpu.eip);
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
                        self.lookup(cpu, bus)?; // Update optional opcode counts.
                    }
                    assert!(matches!(frame.exit_kind, 0..=4), "unknown JIT exit kind");
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
            let mut batch = if let Some(repeat) = repeat {
                Interpreter::resume_repeat(cpu, bus, start.wrapping_add(ran), &repeat)
            } else if eligible {
                self.prepare_interpreter_fetch(cpu);
                Interpreter::run_one(cpu, bus, start.wrapping_add(ran))
            } else if cpu.interrupt_shadow() {
                // The ordinary batch truncates this state to one instruction.
                // Reuse its single-instruction kernel, retaining the forced
                // attention when shadow expiry shortened the caller's plan.
                // This also applies when the instruction clears IF; faults,
                // halt and explicit attention keep their own precise result.
                Interpreter::run_one(cpu, bus, start.wrapping_add(ran))
            } else {
                Interpreter::run_batch(cpu, bus, start.wrapping_add(ran), budget - ran)
            };
            if shadowed && limit > 1 && matches!(batch.stop, BatchStop::Budget) {
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
