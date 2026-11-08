//! Same-core B1 baseline: heat-qualified register blocks and shared fallback.
//!
//! Every dispatch requires the interpreter's valid execute-cache entry. Source
//! page versions cover interpreted stores, aliases, page walks and devices.
//! This first slice emits no guest-memory access or internal link: only the
//! firmware can change code/mappings, while native execution is stopped. Event
//! deadlines are checked before entry; a too-large block uses normal semantics.
use super::compile::{self, Block, Frame};
use crate::machine::HostBus;
use crate::x86::cpu::SegmentReg;
use crate::x86::{BatchResult, BatchStop, Bus, Cpu, Interpreter, LinearAddr};
use alloc::{boxed::Box, vec::Vec};
use core::arch::asm;

const SLOTS: usize = 2048;
const CODE_LIMIT: usize = 8 * 1024 * 1024;
const HOT: u8 = 3;

enum State {
    Cold(u8),
    Published {
        version: u64,
        version_ptr: *const u64,
        block: Option<Block>,
    },
}
struct Entry {
    eip: u32,
    backing: u64,
    state: State,
}
#[derive(Default, Debug)]
pub struct Stats {
    pub native_steps: u64,
    pub native_user_steps: u64,
    pub interpreted_steps: u64,
    pub invocations: u64,
    pub compiled_blocks: u64,
    pub unsupported_entries: u64,
    pub invalidations: u64,
    pub cache_clears: u64,
}
pub struct Engine {
    entries: Box<[Option<Entry>]>,
    code_bytes: usize,
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
            stats: Stats::default(),
        }
    }
    pub(crate) fn clear(&mut self, bus: &mut HostBus) {
        for entry in &mut self.entries {
            *entry = None;
        }
        self.code_bytes = 0;
        bus.jit_release_code_pages();
        self.stats.cache_clears += 1;
    }
    fn lookup(&mut self, cpu: &mut Cpu, bus: &mut HostBus) -> Option<&Block> {
        if self.code_bytes >= CODE_LIMIT {
            self.clear(bus);
        }
        let linear = LinearAddr(cpu.seg(SegmentReg::Cs).base.wrapping_add(cpu.eip));
        let hit = cpu.fetch_cache_lookup(linear)?;
        let index =
            ((cpu.eip ^ (cpu.eip >> 12) ^ (hit.backing.0 as u32 >> 4)) as usize) & (SLOTS - 1);
        let entry = &mut self.entries[index];
        if !entry
            .as_ref()
            .is_some_and(|e| e.eip == cpu.eip && e.backing == hit.backing.0)
        {
            if let Some(Entry {
                state: State::Published {
                    block: Some(old), ..
                },
                ..
            }) = entry
            {
                self.code_bytes -= old.words.len() * 4;
            }
            *entry = Some(Entry {
                eip: cpu.eip,
                backing: hit.backing.0,
                state: State::Cold(0),
            });
        }
        let entry = entry.as_mut().unwrap();
        if let State::Published {
            version,
            version_ptr,
            block,
        } = &entry.state
        {
            // The HostBus owns a stable CodePages allocation until all entries
            // have been cleared. Mutations happen only outside this invocation.
            if unsafe { **version_ptr } != *version {
                if let Some(old) = block {
                    self.code_bytes -= old.words.len() * 4;
                }
                entry.state = State::Cold(0);
                self.stats.invalidations += 1;
            }
        }
        if let State::Cold(heat) = &mut entry.state {
            *heat += 1;
            if *heat < HOT {
                return None;
            }
            let window = (4096 - (linear.0 & 4095)).min(240);
            let window = u64::from(window).min(u64::from(u32::MAX - cpu.eip) + 1) as usize;
            // HostBus execute hits are side-effect-free stable RAM tokens for
            // this page. No speculative page walk or future A-bit update occurs.
            let bytes = unsafe { core::slice::from_raw_parts(hit.backing.0 as *const u8, window) };
            let block = compile::block(bytes, cpu.eip, 16);
            let (_, version, version_ptr) = bus.jit_code_stamp(cpu, hit.backing);
            if let Some(code) = &block {
                self.code_bytes += code.words.len() * 4;
                self.stats.compiled_blocks += 1;
                // Publication follows complete stores into the executable Vec.
                unsafe {
                    asm!("fence.i", options(nostack));
                }
            } else {
                self.stats.unsupported_entries += 1;
            }
            entry.state = State::Published {
                version,
                version_ptr,
                block,
            };
        }
        match &entry.state {
            State::Published { block, .. } => block.as_ref(),
            _ => None,
        }
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
            let step = start.wrapping_add(ran);
            let remaining = budget - ran;
            let eligible = !cpu.halted
                && !cpu.code_default16()
                && !cpu.interrupt_shadow()
                && cpu.seg(SegmentReg::Cs).limit == u32::MAX
                && cpu.eflags & ((1 << 8) | (1 << 17)) == 0;
            let (limit, attention) = bus
                .batch_attention_limit(step, remaining)
                .unwrap_or((1, false));
            let native = if eligible {
                self.lookup(cpu, bus)
            } else {
                None
            };
            if let Some(block) = native.filter(|b| u64::from(b.instructions) <= limit) {
                let count = u64::from(block.instructions);
                let mut frame = Frame {
                    regs: cpu.regs,
                    flags: cpu.effective_eflags(),
                    eip: cpu.eip,
                    executed: 0,
                };
                bus.begin_instruction(step, cpu.tsc());
                cpu.begin_instruction();
                cpu.segment_override = None;
                // Bank 0, including all Rust ABI registers and its stack, is
                // preserved. Generated code only writes this exclusive frame;
                // XLEAVE returns to the instruction following XENTER.
                unsafe {
                    asm!(".insn i 0x7b, 7, x31, a1, 0", ".insn i 0x7b, 4, x0, a0, 0",
                         in("a0") block.words.as_ptr(), in("a1") &mut frame, options(nostack));
                }
                assert_eq!(
                    u64::from(frame.executed),
                    count,
                    "B1 native boundary mismatch"
                );
                cpu.regs = frame.regs;
                cpu.eip = frame.eip;
                cpu.jit_commit_flags(frame.flags);
                cpu.advance_tsc(count);
                cpu.chip_counts.completed += count;
                ran += count;
                self.stats.native_steps += count;
                if cpu.cpl() == 3 {
                    self.stats.native_user_steps += count;
                }
                self.stats.invocations += 1;
                if attention && count == limit {
                    return BatchResult {
                        ran,
                        stop: BatchStop::Attention {
                            instruction_step: start.wrapping_add(ran).wrapping_sub(1),
                            was_interruptible: false,
                        },
                    };
                }
            } else {
                let batch =
                    Interpreter::run_batch(cpu, bus, step, if eligible { 1 } else { remaining });
                ran += batch.ran;
                self.stats.interpreted_steps += batch.ran;
                if !matches!(batch.stop, BatchStop::Budget) {
                    return BatchResult {
                        ran,
                        stop: batch.stop,
                    };
                }
            }
        }
        BatchResult {
            ran,
            stop: BatchStop::Budget,
        }
    }
}
