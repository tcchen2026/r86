//! Resident bank-1 execution. The shared interpreter owns all x86 fault delivery,
//! page walks with A/D writes, devices and unsupported instructions. Refills use
//! its side-effect-free RAM proofs; they do not export resident registers.
//!
//! XJT entries retain qualified fetch mappings until invalidation, including
//! across code pages. Key replacement retains code. Mapping changes or watched
//! writes flush XJT/return addresses before retiring the whole code generation.
use super::{
    cache::{Cache, Value},
    compile::{self, End},
    native,
};
use crate::machine::HostBus;
use crate::x86::cpu::{SEGMENT_FLAG_DEFAULT_BIG, SegmentReg};
use crate::x86::{BatchResult, BatchStop, Bus, Cpu, Interpreter, LinearAddr, mmu};
const HOT: u32 = 3;
const SEGMENTS: [SegmentReg; 6] = [
    SegmentReg::Es,
    SegmentReg::Cs,
    SegmentReg::Ss,
    SegmentReg::Ds,
    SegmentReg::Fs,
    SegmentReg::Gs,
];

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
    pub unsupported_f7: [u64; 8],
    pub unsupported_0f: [u64; 256],
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
            unsupported_f7: [0; 8],
            unsupported_0f: [0; 256],
            ends: [0; 4],
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Mapping {
    epoch: u64,
    control: u32,
    cs_base: u32,
}
pub struct Engine {
    cache: Cache,
    /// Conservative bounds include negative entries as well as native code.
    /// An INVLPG may discard extra entries, never preserve a stale fetch proof.
    code_pages: Option<(u32, u32)>,
    code_epoch: u64,
    mapping: Option<Mapping>,
    segments: Option<[(u32, u32, u32); 6]>,
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
            cache: Cache::new(),
            code_pages: None,
            code_epoch: 0,
            mapping: None,
            segments: None,
            stats: Stats::default(),
        }
    }
    pub(crate) fn clear(&mut self, bus: &HostBus) {
        // No native entry, return stub or direct edge survives arena reuse.
        native::flush_jumps();
        native::flush_data();
        self.cache.clear();
        self.code_pages = None;
        // Keep physical watches: an obsolete watch causes at most an extra
        // invalidation when written. Scanning/releasing the shared watch table
        // is unnecessary; Machine drops it when changing execution engines.
        self.code_epoch = bus.jit_code_epoch();
        self.stats.cache_clears += 1;
    }
    fn sync_mapping(&mut self, cpu: &mut Cpu, bus: &HostBus) {
        if let Some((base, mask)) = cpu.take_jit_invalidated_range() {
            if mask == 0xffff_f000 {
                native::invalidate_data(base);
            } else {
                native::flush_data();
            }
            if self
                .code_pages
                .is_some_and(|(lo, hi)| base <= hi && (base | !mask) >= lo)
            {
                self.clear(bus);
            }
        }
        let control = (cpu.cr0 & 1)
            | ((cpu.cr0 >> 30) & 2)
            | ((cpu.cr0 >> 14) & 4)
            | ((cpu.eflags >> 14) & 8)
            | (u32::from(cpu.cpl()) << 4);
        let next = Mapping {
            epoch: cpu.jit_mapping_epoch(),
            control,
            cs_base: cpu.seg(SegmentReg::Cs).base,
        };
        if self.mapping == Some(next) {
            return;
        }
        // Qualify cached fetches BEFORE lookup. This also covers negative
        // entries, which have no hardware jump-table row to invalidate.
        self.clear(bus);
        let (start, end) = bus.chip_ram_bounds();
        native::region(start, end);
        native::context(control, next.epoch as u32);
        // XTCONTEXT retires the hardware segment descriptors as well as data
        // proofs. Equal descriptor values still need republishing afterward.
        self.segments = None;
        self.mapping = Some(next);
    }
    fn sync_segments(&mut self, cpu: &Cpu) {
        let next = SEGMENTS.map(|seg| {
            let cache = cpu.seg(seg);
            let rights = if cpu.eflags & (1 << 18) != 0 && cpu.cr0 & (1 << 18) != 0 {
                0
            } else {
                cpu.chip_segment_rights(seg)
            };
            (cache.base, cache.limit, rights)
        });
        if self.segments == Some(next) {
            return;
        }
        macro_rules! seg {
            ($n:literal) => {{
                let (base, limit, rights) = next[$n];
                native::segment::<$n>(base, limit, rights);
            }};
        }
        seg!(0);
        seg!(1);
        seg!(2);
        seg!(3);
        seg!(4);
        seg!(5);
        self.segments = Some(next);
    }
    fn lookup(&mut self, cpu: &mut Cpu, bus: &mut HostBus, eip: u32) -> Option<u32> {
        let slot = self.cache.slot(eip);
        let value = self.cache.value(slot);
        if value.pc >= 4 {
            // Generation guard, mapping synchronization and physical watches
            // give this the same lifetime as a hardware fetch/jump proof.
            return Some(value.pc);
        }
        if value.pc == HOT {
            self.unsupported_backing(value.backing);
            return None;
        }
        let cs = cpu.seg(SegmentReg::Cs).base;
        let linear = LinearAddr(cs.wrapping_add(eip));
        let hit = cpu
            .fetch_cache_lookup(linear)
            .or_else(|| mmu::try_resident_fetch_translation(cpu, bus, linear))?;
        let heat = value.pc + 1;
        if heat < HOT {
            self.cache.put(
                slot,
                Value {
                    pc: heat,
                    backing: 0,
                },
            );
            return None;
        }
        bus.jit_code_stamp(cpu, hit.backing);
        // Promotion revokes old hardware write grants as well as software ones.
        native::flush_data();
        let pc = self.compile_group(bus, eip, cs, hit.backing.0 as u32);
        if pc == HOT {
            self.unsupported_backing(hit.backing.0 as u32);
            None
        } else {
            Some(pc)
        }
    }
    /// A hot root and its direct successors admit at most eight blocks total.
    /// Compilation has no guest effects; bounded batching amortizes FENCE.I
    /// without another fetch proof, decoder or opcode coverage rule.
    fn compile_group(&mut self, bus: &HostBus, eip: u32, cs: u32, backing: u32) -> u32 {
        const BLOCKS: usize = 8;
        let page = cs.wrapping_add(eip) & !4095;
        let mut pending = [0; BLOCKS * 2 + 1];
        pending[0] = eip;
        let (mut head, mut tail, mut built) = (0, 1, 0);
        let mut patches = [(0, 0); BLOCKS * 2];
        let mut patch_count = 0;
        let mut root = HOT;
        while head < tail && built < BLOCKS {
            let target = pending[head];
            head += 1;
            let mut slot = self.cache.slot(target);
            if self.cache.value(slot).pc >= HOT {
                continue;
            }
            let linear = cs.wrapping_add(target);
            let address = (backing & !4095) | (linear & 4095);
            let window =
                u64::from(4096 - (linear & 4095)).min(u64::from(u32::MAX - target) + 1) as usize;
            let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, window) };
            let block = compile::block(bytes, target, cs, 4095);
            if block.as_ref().is_some_and(|b| !self.cache.fits(b)) {
                if built != 0 {
                    break; // Retain this group's live root; next miss may reset.
                }
                self.clear(bus);
                slot = self.cache.slot(target);
            }
            built += 1;
            let pc = if let Some(block) = block {
                self.stats.compiled_blocks += 1;
                self.stats.ends[match block.end {
                    End::Branch => 0,
                    End::Unsupported(_) => 1,
                    End::Window => 2,
                    End::Limit => 3,
                }] += 1;
                let pc = self.cache.install(&block);
                for link in &block.links {
                    if cs.wrapping_add(link.eip) & !4095 == page {
                        patches[patch_count] = (pc - compile::ENTRY_OFFSET + link.start, link.eip);
                        patch_count += 1;
                        pending[tail] = link.eip;
                        tail += 1;
                    }
                }
                pc
            } else {
                self.stats.unsupported_entries += 1;
                HOT
            };
            self.cache.put(
                slot,
                Value {
                    pc,
                    backing: address,
                },
            );
            if built == 1 {
                root = pc;
            }
        }
        // At most sixteen known edges, independent of the resident cache size.
        for &(source, target) in &patches[..patch_count] {
            if let Some(pc) = self.cache.find(target) {
                self.link(source, pc);
            }
        }
        self.code_pages = Some(
            self.code_pages
                .map_or((page, page), |(lo, hi)| (lo.min(page), hi.max(page))),
        );
        root
    }
    fn link(&mut self, source: u32, target: u32) {
        if let Some(word) = compile::jump(i64::from(target) - i64::from(source)) {
            self.cache.patch(source, word);
        }
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
            self.unsupported_backing(hit.backing.0 as u32);
        }
    }
    fn unsupported_backing(&mut self, backing: u32) {
        let len = (4096 - (backing & 4095)).min(15) as usize;
        let bytes = unsafe { core::slice::from_raw_parts(backing as *const u8, len) };
        let opcode = compile::opcode_bytes(bytes);
        if let Some(&first) = opcode.first() {
            self.stats.unsupported[first as usize] += 1;
            if let Some(&second) = opcode.get(1) {
                match first {
                    0xf7 => self.stats.unsupported_f7[usize::from((second >> 3) & 7)] += 1,
                    0x0f => self.stats.unsupported_0f[usize::from(second)] += 1,
                    _ => (),
                }
            }
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
            if self.cache.dirty {
                native::publish();
                self.cache.dirty = false;
            }
            let exit = native::enter(pc);
            self.stats.invocations += 1;
            let done = budget - exit.remaining;
            match exit.reason {
                compile::MISS => {
                    self.stats.leaves[0] += 1;
                    if done < budget {
                        let cs = cpu.seg(SegmentReg::Cs).base;
                        // Read before lookup can reset the arena. The footer
                        // names a patch directly; no entry or link-list search.
                        let source = unsafe { *(exit.pc as *const u32) };
                        let generation = self.cache.epoch;
                        if let Some(next) = self.lookup(cpu, bus, exit.eip) {
                            native::fill_jump(exit.eip, next);
                            let distance = source & 4095;
                            if generation == self.cache.epoch
                                && distance != 0
                                && cs.wrapping_add(exit.eip) & !4095 == source & !4095
                            {
                                self.link(exit.pc - distance, next);
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
                    let site = self.cache.site(retry);
                    break (
                        site.eip,
                        done.checked_sub(site.count)
                            .expect("chip refund exceeds reservation"),
                    );
                }
                compile::TICK => {
                    self.stats.leaves[2] += 1;
                    // XTICK names its own PC and has an immutable EIP header.
                    // No lookup/allocation occurs between entry and this read.
                    let eip = unsafe { *((exit.pc - compile::ENTRY_OFFSET) as *const u32) };
                    break (eip, done);
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
                self.sync_mapping(cpu, bus);
                if let Some(pc) = self.lookup(cpu, bus, cpu.eip) {
                    self.sync_segments(cpu);
                    native::fill_jump(cpu.eip, pc);
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
