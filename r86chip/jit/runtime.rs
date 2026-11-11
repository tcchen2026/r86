//! Resident bank-1 execution. The shared interpreter owns all x86 fault delivery,
//! page walks with A/D writes, devices and unsupported instructions. Refills use
//! its side-effect-free RAM proofs; they do not export resident registers.
//!
//! Only the current source page is installed in XJT. Replacement/context changes
//! flush XJT (including its return stack) before dropping code. Watched source
//! writes remain interpreter accesses; the existing code epoch retires the cache.
use super::{
    compile::{self, Block, End},
    native,
};
use crate::machine::HostBus;
use crate::x86::cpu::{SEGMENT_FLAG_DEFAULT_BIG, SegmentReg};
use crate::x86::{BatchResult, BatchStop, Bus, Cpu, Interpreter, LinearAddr, mmu};
use alloc::{boxed::Box, vec::Vec};

const SLOTS: usize = 2048;
const CODE_LIMIT: usize = 8 * 1024 * 1024;
const HOT: u8 = 3;
const SEGMENTS: [SegmentReg; 6] = [
    SegmentReg::Es,
    SegmentReg::Cs,
    SegmentReg::Ss,
    SegmentReg::Ds,
    SegmentReg::Fs,
    SegmentReg::Gs,
];

struct Entry {
    eip: u32,
    backing: u64,
    heat: u8,
    block: Option<Block>,
}
#[derive(Debug)]
pub struct Stats {
    pub native_steps: u64,
    pub native_user_steps: u64,
    pub interpreted_steps: u64,
    pub invocations: u64,
    pub compiled_blocks: u64,
    pub unsupported_entries: u64,
    pub invalidations: u64,
    pub cache_clears: u64,
    /// Runtime exits: miss, service, tick, interpret. Services count attempts,
    /// including refills that resume the same word without a guest boundary.
    pub leaves: [u64; 4],
    pub services: [u64; 5],
    /// Dynamic interpreter handoffs, indexed by first non-prefix opcode byte.
    pub unsupported: [u64; 256],
    /// Compiled endings: branch, unsupported, page/window, instruction cap.
    pub ends: [u64; 4],
}
impl Default for Stats {
    fn default() -> Self {
        Self {
            native_steps: 0,
            native_user_steps: 0,
            interpreted_steps: 0,
            invocations: 0,
            compiled_blocks: 0,
            unsupported_entries: 0,
            invalidations: 0,
            cache_clears: 0,
            leaves: [0; 4],
            services: [0; 5],
            unsupported: [0; 256],
            ends: [0; 4],
        }
    }
}
#[derive(PartialEq, Eq)]
struct Context {
    epoch: u64,
    control: u32,
    cs_base: u32,
    segments: [(u32, u32, u32); 6],
}
pub struct Engine {
    entries: Box<[Option<Entry>]>,
    code_bytes: usize,
    code_epoch: u64,
    context: Option<Context>,
    page: Option<u32>,
    pub stats: Stats,
}
impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}
impl Engine {
    pub fn new() -> Self {
        Self {
            entries: (0..SLOTS)
                .map(|_| None)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            code_bytes: 0,
            code_epoch: 0,
            context: None,
            page: None,
            stats: Stats::default(),
        }
    }
    pub(crate) fn clear(&mut self, bus: &mut HostBus) {
        // Return stubs and JAL self-edges point into these stable Vecs. No
        // allocation can be freed while the hardware retains its address.
        native::flush_jumps();
        native::flush_data();
        self.page = None;
        for entry in &mut self.entries {
            *entry = None;
        }
        self.code_bytes = 0;
        bus.jit_release_code_pages();
        self.code_epoch = bus.jit_code_epoch();
        self.stats.cache_clears += 1;
    }
    fn sync_context(&mut self, cpu: &mut Cpu, bus: &HostBus) {
        // A single INVLPG is queued separately from the full mapping epoch.
        // Retire hardware proofs even when no software fetch row was present.
        if cpu.take_jit_invalidated_range().is_some() {
            native::flush_data();
            self.unlink();
            self.page = None;
        }
        let control = (cpu.cr0 & 1)
            | ((cpu.cr0 >> 30) & 2)
            | ((cpu.cr0 >> 14) & 4)
            | ((cpu.eflags >> 14) & 8)
            | (u32::from(cpu.cpl()) << 4);
        let next = Context {
            epoch: cpu.jit_mapping_epoch(),
            control,
            cs_base: cpu.seg(SegmentReg::Cs).base,
            segments: SEGMENTS.map(|seg| {
                let cache = cpu.seg(seg);
                // No unchecked access while x86 alignment checking may apply.
                let rights = if cpu.eflags & (1 << 18) != 0 && cpu.cr0 & (1 << 18) != 0 {
                    0
                } else {
                    cpu.chip_segment_rights(seg)
                };
                (cache.base, cache.limit, rights)
            }),
        };
        if self.context.as_ref() == Some(&next) {
            return;
        }
        let changed = self
            .context
            .as_ref()
            .is_none_or(|old| old.epoch != next.epoch || old.control != next.control);
        if changed {
            let (start, end) = bus.chip_ram_bounds();
            native::region(start, end);
            // Epoch is u64; explicit flush also handles low-word reuse.
            native::flush_data();
            native::context(control, next.epoch as u32);
        }
        if changed
            || self
                .context
                .as_ref()
                .is_none_or(|old| old.cs_base != next.cs_base)
        {
            self.unlink();
            self.page = None;
        }
        macro_rules! seg {
            ($n:literal) => {{
                let (base, limit, rights) = next.segments[$n];
                native::segment::<$n>(base, limit, rights);
            }};
        }
        seg!(0);
        seg!(1);
        seg!(2);
        seg!(3);
        seg!(4);
        seg!(5);
        self.context = Some(next);
    }
    fn lookup(&mut self, cpu: &mut Cpu, bus: &mut HostBus, eip: u32) -> Option<u32> {
        if self.code_bytes >= CODE_LIMIT {
            self.clear(bus);
        }
        let linear = LinearAddr(cpu.seg(SegmentReg::Cs).base.wrapping_add(eip));
        let hit = cpu
            .fetch_cache_lookup(linear)
            .or_else(|| mmu::try_resident_fetch_translation(cpu, bus, linear))?;
        let index = ((eip ^ (eip >> 12) ^ (hit.backing.0 as u32 >> 4)) as usize) & (SLOTS - 1);
        if !self.entries[index]
            .as_ref()
            .is_some_and(|e| e.eip == eip && e.backing == hit.backing.0)
        {
            if let Some(old) = &self.entries[index] {
                if let Some(block) = &old.block {
                    self.code_bytes -= block.words.len() * 4;
                }
            }
            // Direct links may name the displaced allocation. Restore their
            // slow exits before replacing it; self-links die with their owner.
            if self.entries[index]
                .as_ref()
                .is_some_and(|e| e.block.is_some())
            {
                self.unlink();
            }
            self.entries[index] = Some(Entry {
                eip,
                backing: hit.backing.0,
                heat: 0,
                block: None,
            });
        }
        let entry = self.entries[index].as_mut().unwrap();
        if entry.heat < HOT {
            entry.heat += 1;
            if entry.heat < HOT {
                return None;
            }
            let window =
                u64::from(4096 - (linear.0 & 4095)).min(u64::from(u32::MAX - eip) + 1) as usize;
            // Execute-cache tokens are stable ordinary RAM, approved by the
            // shared fetcher. Never speculate across a guest code page.
            let bytes = unsafe { core::slice::from_raw_parts(hit.backing.0 as *const u8, window) };
            entry.block = compile::block(bytes, eip, 4095);
            bus.jit_code_stamp(cpu, hit.backing);
            // Promotion revokes software aliases; retire pre-promotion hardware
            // write grants too. Subsequent RAM refills consult the same watch.
            native::flush_data();
            if let Some(block) = &entry.block {
                self.code_bytes += block.words.len() * 4;
                self.stats.compiled_blocks += 1;
                self.stats.ends[match block.end {
                    End::Branch => 0,
                    End::Unsupported(_) => 1,
                    End::Window => 2,
                    End::Limit => 3,
                }] += 1;
                native::publish();
            } else {
                self.stats.unsupported_entries += 1;
            }
        }
        if entry.block.is_none() {
            self.unsupported(cpu, eip);
            return None;
        }
        let entry = self.entries[index].as_ref().unwrap();
        let block = entry.block.as_ref().unwrap();
        let page = linear.0 & !4095;
        if self.page != Some(page) {
            native::flush_jumps();
            self.page = Some(page);
        }
        let pc = block.words.as_ptr() as u32;
        native::fill_jump(eip, pc);
        Some(pc)
    }
    fn unlink(&mut self) {
        native::flush_jumps();
        for entry in self.entries.iter_mut().flatten() {
            if let Some(block) = &mut entry.block {
                for link in &block.links {
                    if link.eip != entry.eip {
                        block.words[link.start as usize / 4] = link.original;
                    }
                }
            }
        }
        native::publish();
    }
    fn link(&mut self, leave: u32, target: u32, pc: u32, cs: u32) {
        for entry in self.entries.iter_mut().flatten() {
            if (entry.eip.wrapping_add(cs) ^ target.wrapping_add(cs)) & !4095 != 0 {
                continue;
            }
            if let Some(block) = &mut entry.block {
                let base = block.words.as_ptr() as u32;
                let offset = leave.wrapping_sub(base).wrapping_sub(8);
                if let Some(link) = block
                    .links
                    .iter()
                    .find(|l| l.offset == offset && l.eip == target)
                {
                    if let Some(word) = compile::jump(i64::from(pc) - i64::from(base + link.start))
                    {
                        block.words[link.start as usize / 4] = word;
                        native::publish();
                    }
                    return;
                }
            }
        }
    }
    fn site(&self, pc: u32) -> (compile::Site, u32) {
        for entry in self.entries.iter().flatten() {
            if let Some(block) = &entry.block {
                let offset = pc.wrapping_sub(block.words.as_ptr() as u32);
                if offset < block.words.len() as u32 * 4 {
                    return (block.site(offset), block.instructions);
                }
            }
        }
        panic!("chip exit outside live native code: {pc:#x}");
    }
    fn refill(cpu: &mut Cpu, bus: &mut HostBus, offset: u32, word: u32) -> bool {
        let f = (word >> 12) & 7;
        let (seg, write) = match word & 127 {
            0x07 => (SegmentReg::Ds, matches!(f, 3 | 6 | 7)),
            0x47 => (SegmentReg::Ss, matches!(f, 3 | 6 | 7)),
            0x27 => (SegmentReg::Ds, true),
            0x77 => (SegmentReg::Ss, true),
            0x5b => (
                SEGMENTS[((word >> 25) & 7) as usize],
                f >= 4 || word & (1 << 28) != 0,
            ),
            _ => panic!("chip service from non-memory word {word:#x}"),
        };
        let linear = LinearAddr(cpu.seg(seg).base.wrapping_add(offset));
        let user = cpu.cpl() == 3;
        let token = if write {
            cpu.cached_write_translation(linear, user)
        } else {
            cpu.cached_read_translation(linear, user)
        }
        .or_else(|| mmu::try_resident_data_translation(cpu, bus, linear, write));
        let Some(token) = token else {
            return false;
        };
        let (start, end) = bus.chip_ram_bounds();
        let page = (token.0 & !4095) as u32;
        if token.0 > u64::from(u32::MAX)
            || page < start
            || page > end - 4096
            || token.0 as u32 & 4095 != linear.0 & 4095
        {
            return false;
        }
        native::fill_data(linear.0, page | if write { 3 } else { 1 });
        true
    }
    fn unsupported(&mut self, cpu: &mut Cpu, eip: u32) {
        let linear = LinearAddr(cpu.seg(SegmentReg::Cs).base.wrapping_add(eip));
        if let Some(hit) = cpu.fetch_cache_lookup(linear) {
            let len = (4096 - (linear.0 & 4095)).min(15) as usize;
            let bytes = unsafe { core::slice::from_raw_parts(hit.backing.0 as *const u8, len) };
            self.stats.unsupported[compile::opcode(bytes) as usize] += 1;
        }
    }
    /// Keep bank-1 state across misses/refills. Only the final interpretation or
    /// event boundary exports it. XTICK reservations are refunded by the side
    /// table if a service hands an unfinished instruction to the interpreter.
    fn execute(&mut self, cpu: &mut Cpu, bus: &mut HostBus, mut pc: u32, budget: u32) -> u32 {
        let flags = cpu.effective_eflags();
        native::import(&cpu.regs, flags);
        native::ticks(budget);
        let old_vector = native::vector(native::service as *const () as u32);
        let (eip, completed) = loop {
            let exit = native::enter(pc);
            self.stats.invocations += 1;
            let done = budget - exit.remaining;
            match exit.reason {
                compile::MISS => {
                    self.stats.leaves[0] += 1;
                    // Do not link across source pages in this batch. A runtime
                    // dispatch may select another page after flushing XJT/RAS.
                    if done < budget {
                        let cs = cpu.seg(SegmentReg::Cs).base;
                        // Cross-page links are excluded. Reject them before
                        // searching the source block's native-PC side table.
                        let same_page = self.page == Some(cs.wrapping_add(exit.eip) & !4095);
                        if let Some(next) = self.lookup(cpu, bus, exit.eip) {
                            if same_page {
                                self.link(exit.pc, exit.eip, next, cs);
                            }
                            pc = next;
                            continue;
                        }
                    }
                    break (exit.eip, done);
                }
                compile::SERVICE => {
                    self.stats.leaves[1] += 1;
                    let [retry, offset, word, reason] = native::service_record();
                    assert!((1..=4).contains(&reason), "invalid chip service reason");
                    self.stats.services[reason as usize] += 1;
                    if reason == 4 && Self::refill(cpu, bus, offset, word) {
                        pc = retry;
                        continue;
                    }
                    let (site, reserved) = self.site(retry);
                    break (
                        site.eip,
                        done.checked_sub(reserved - site.index)
                            .expect("chip refund exceeds reservation"),
                    );
                }
                compile::TICK => {
                    self.stats.leaves[2] += 1;
                    let (site, _) = self.site(exit.pc);
                    break (site.eip, done);
                }
                compile::INTERPRET => {
                    self.stats.leaves[3] += 1;
                    self.unsupported(cpu, exit.eip);
                    break (exit.eip, done);
                }
                other => panic!("unexpected chip leave {other:#x} at {:#x}", exit.pc),
            }
        };
        native::vector(old_vector);
        let flags = native::export(&mut cpu.regs, flags);
        cpu.eip = eip;
        cpu.jit_commit_flags(flags);
        cpu.advance_tsc(u64::from(completed));
        cpu.chip_counts.completed += u64::from(completed);
        self.stats.native_steps += u64::from(completed);
        if cpu.cpl() == 3 {
            self.stats.native_user_steps += u64::from(completed);
        }
        completed
    }
    pub(crate) fn run_batch(
        &mut self,
        cpu: &mut Cpu,
        bus: &mut HostBus,
        start: u64,
        budget: u64,
    ) -> BatchResult {
        let mut ran = 0;
        while ran < budget {
            if bus.jit_code_epoch() != self.code_epoch {
                self.stats.invalidations += 1;
                self.clear(bus);
            }
            let remaining = budget - ran;
            let eligible = !cpu.halted
                && cpu.cr0 & 1 != 0
                && !cpu.code_default16()
                && cpu.seg(SegmentReg::Ss).flags & SEGMENT_FLAG_DEFAULT_BIG != 0
                && !cpu.interrupt_shadow()
                && cpu.seg(SegmentReg::Cs).limit == u32::MAX
                && cpu.eflags & ((1 << 8) | (1 << 17)) == 0;
            let (limit, attention) = bus
                .batch_attention_limit(start.wrapping_add(ran), remaining)
                .unwrap_or((1, false));
            if eligible && limit != 0 {
                // Context must be synchronized before lookup installs XJT.
                self.sync_context(cpu, bus);
                if let Some(pc) = self.lookup(cpu, bus, cpu.eip) {
                    bus.begin_instruction(start.wrapping_add(ran), cpu.tsc());
                    cpu.begin_instruction();
                    cpu.segment_override = None;
                    let count = u64::from(self.execute(
                        cpu,
                        bus,
                        pc,
                        limit.min(u64::from(u32::MAX)) as u32,
                    ));
                    ran += count;
                    if attention && count == limit {
                        return BatchResult {
                            ran,
                            stop: BatchStop::Attention {
                                instruction_step: start.wrapping_add(ran).wrapping_sub(1),
                                was_interruptible: false,
                            },
                        };
                    }
                    if ran == budget {
                        break;
                    }
                }
            }
            // Even a zero-progress native expiry/service executes one shared
            // instruction next, never retries an unchanged block indefinitely.
            let batch = Interpreter::run_batch(
                cpu,
                bus,
                start.wrapping_add(ran),
                if eligible { 1 } else { budget - ran },
            );
            ran += batch.ran;
            self.stats.interpreted_steps += batch.ran;
            if !matches!(batch.stop, BatchStop::Budget) {
                return BatchResult {
                    ran,
                    stop: batch.stop,
                };
            }
        }
        BatchResult {
            ran,
            stop: BatchStop::Budget,
        }
    }
}
