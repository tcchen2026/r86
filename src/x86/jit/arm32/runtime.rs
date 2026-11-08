//! ARMv6K block dispatch for the shared Machine.
//!
//! A block is keyed by its EIP, the backing byte from the current execute
//! translation and the flat-segment facts it was compiled under. It may run
//! while that translation exists and its CodePages version is unchanged; the
//! interpreter's fetch path owns every page walk and fetch fault. After a
//! version change the page is watched again, which revokes writable aliases,
//! and the block is kept if its own source bytes are unchanged: code and data
//! often share pages in firmware and drivers. Blocks live until the arena or
//! the bounded block map is reset.
//!
//! Only control-transfer targets and instructions after an interpreter handoff
//! are entries; straight-line cold code is interpreted up to the next transfer.
//! A direct-mapped counter admits an entry on its sixteenth visit: with a
//! 4 MiB arena, compiling lukewarm boot code thrashes the cache (measured on
//! the TinyCore boot: admission at three visits compiled each block ~8 times).
//!
//! Blocks chain through a native table of [eip, code] pairs, one plane per
//! privilege so supervisor-only code is never reached from CPL 3. An entry is
//! inserted only after the full lookup above succeeds. Native code cannot
//! change mappings, CPL, CS, segment facts or watched code, so the table stays
//! valid for a whole invocation; before each invocation the runtime clears it
//! after any fact, CS base or global mapping change, drops non-global entries
//! after a non-global flush, and an INVLPG range's or a retired page's entries.
//!
//! Each block reserves its length from the invocation budget, which ends at
//! the next machine event; a precise exit hands its instruction to the
//! interpreter.

use super::code::Arena;
use super::compile::{self, EXIT_INTERPRET, Frame, STACK_FACT, TABLE_BITS};
use crate::machine::HostBus;
use crate::x86::cpu::SegmentReg;
use crate::x86::jit::decode;
use crate::x86::{BatchResult, BatchStop, Bus, Cpu, Interpreter, LinearAddr};
use alloc::{boxed::Box, format, string::String, vec::Vec};
use core::hash::{BuildHasherDefault, Hasher};
#[cfg(not(target_os = "linux"))]
use hashbrown::HashMap;
#[cfg(target_os = "linux")]
use std::collections::HashMap;

const HEAT: usize = 4096;
const HOT: u8 = 16;
/// Native blocks average a few hundred bytes, so the 4 MiB arena fills first.
const BLOCKS: usize = 16384;
const BLOCK_INSTRUCTIONS: usize = 32;
const WINDOW: usize = 480;
const TF: u32 = 1 << 8;
const VM: u32 = 1 << 17;
/// DS/ES/SS flat read and write proofs, plus the 32-bit stack proof.
const FACTS: u32 = 0b1101 | 0b1101 << 8 | STACK_FACT;
const PLANE: usize = 1 << TABLE_BITS;

/// (EIP | facts << 32, backing).
type Key = (u64, u64);

/// Mixes the two key words; the default byte hasher is needlessly slow here.
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 32)
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(25) ^ value).wrapping_mul(0x9e37_79b1_85eb_ca87);
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_u64(u64::from(byte));
        }
    }
}

struct Entry {
    version: u64,
    version_ptr: *const u64,
    /// Native code, or 0 when the first instruction needs the interpreter.
    address: usize,
    instructions: u32,
    /// Guest bytes a native block depends on.
    source: Box<[u8]>,
    #[cfg(feature = "jit-profile")]
    opcode: u16,
}

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
    #[cfg(all(feature = "jit-profile", target_os = "linux"))]
    pub compile_nanoseconds: u128,
}

/// What the whole dispatch table was filled under.
#[derive(Clone, Copy, Eq, PartialEq)]
struct Context {
    global: u64,
    facts: u32,
    cs_base: u32,
}

pub struct Engine {
    arena: Arena,
    entry: usize,
    dispatch: usize,
    miss: u32,
    shared_words: usize,
    blocks: HashMap<Key, Entry, BuildHasherDefault<KeyHasher>>,
    /// Visits of not yet admitted entries: (EIP, count), direct-mapped.
    heat: Box<[(u32, u8)]>,
    /// Two planes (supervisor, user) of [eip, code] pairs.
    table: Box<[[u32; 2]]>,
    /// Table indices filled since the last clear.
    filled: Vec<u16>,
    /// Per table slot: the block's linear page, bit 0 for a global
    /// translation, and its CodePages version cell.
    pages: Box<[(u32, *const u64)]>,
    context: Option<Context>,
    mapping: u64,
    code: u64,
    pub stats: Stats,
    /// Interpreted entries by opcode; see `profile_key`.
    #[cfg(feature = "jit-profile")]
    unsupported: Box<[u64; 2048]>,
}

/// A valid native block.
#[derive(Clone, Copy)]
struct Native {
    address: usize,
    instructions: u32,
    /// CodePages version cell of its page.
    version: *const u64,
}

enum Lookup {
    Native(Native),
    /// The interpreter executes this instruction; the next may be native.
    Interpret,
    /// Not yet admitted.
    Cold,
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
        let shared = compile::shared();
        let entry = arena.publish(&shared.words).ok_or("JIT arena too small")?;
        let miss = (entry + shared.miss * 4) as u32;
        Ok(Self {
            arena,
            entry,
            dispatch: entry + shared.dispatch * 4,
            miss,
            shared_words: shared.words.len(),
            blocks: HashMap::default(),
            heat: alloc::vec![(0, 0); HEAT].into_boxed_slice(),
            table: alloc::vec![[0, miss]; 2 * PLANE].into_boxed_slice(),
            filled: Vec::new(),
            pages: alloc::vec![(0, core::ptr::null()); 2 * PLANE].into_boxed_slice(),
            context: None,
            mapping: 0,
            code: 0,
            stats: Stats::default(),
            #[cfg(feature = "jit-profile")]
            unsupported: Box::new([0; 2048]),
        })
    }

    pub(crate) fn clear(&mut self, bus: &mut HostBus) -> Result<(), String> {
        self.blocks.clear();
        self.clear_table();
        self.arena.reset(self.shared_words);
        // No block can observe an old version after this point.
        bus.jit_release_code_pages();
        self.stats.arena_clears += 1;
        Ok(())
    }

    fn clear_table(&mut self) {
        if self.filled.len() > PLANE / 8 {
            self.table.fill([0, self.miss]);
        } else {
            for &index in &self.filled {
                self.table[usize::from(index)] = [0, self.miss];
            }
        }
        self.filled.clear();
        self.stats.table_clears += 1;
    }

    pub fn report(&self) -> String {
        #[allow(unused_mut)]
        let mut report = format!(
            "arm32 jit: {:?}, {} blocks, code {} KiB\n",
            self.stats,
            self.blocks.len(),
            self.arena.used_bytes() / 1024
        );
        #[cfg(feature = "jit-profile")]
        {
            let mut top: Vec<_> = (0..2048).filter(|&i| self.unsupported[i] != 0).collect();
            top.sort_by_key(|&i| core::cmp::Reverse(self.unsupported[i]));
            for i in top.into_iter().take(32) {
                report += &format!(
                    "interpreted entry {}: {}\n",
                    profile_name(i as u16),
                    self.unsupported[i]
                );
            }
        }
        report
    }

    fn facts(cpu: &Cpu) -> u32 {
        let (flat, flat_write) = cpu.jit_segment_masks();
        (flat | flat_write << 8) & FACTS
    }

    /// Remove the table entries that `retire` selects by page and version.
    fn retire_table(&mut self, retire: impl Fn(u32, *const u64) -> bool) {
        let (table, pages, miss) = (&mut self.table, &self.pages, self.miss);
        self.filled.retain(|&slot| {
            let slot = usize::from(slot);
            let keep = !retire(pages[slot].0, pages[slot].1);
            if !keep {
                table[slot] = [0, miss];
            }
            keep
        });
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
        let code = bus.jit_code_epoch();
        if code != self.code {
            match bus.jit_single_retired_version(self.code) {
                Some(version) => self.retire_table(|_, page| page == version),
                None => self.clear_table(),
            }
            self.code = code;
        }
        let mapping = cpu.jit_mapping_epoch();
        let invalidated = cpu.take_jit_invalidated_range();
        if self.context != Some(context) {
            self.clear_table();
            self.context = Some(context);
        } else {
            if self.mapping != mapping {
                self.retire_table(|page, _| page & 1 == 0);
            }
            if let Some((base, mask)) = invalidated {
                self.retire_table(|page, _| page & mask == base);
            }
        }
        self.mapping = mapping;
    }

    /// Classify the current EIP, admitting or compiling it when hot.
    fn lookup(&mut self, cpu: &mut Cpu, bus: &mut HostBus) -> Result<Lookup, String> {
        let linear = cpu.seg(SegmentReg::Cs).base.wrapping_add(cpu.eip);
        // A miss stays an architectural fetch for the interpreter.
        let Some(hit) = cpu.fetch_cache_lookup(LinearAddr(linear)) else {
            return Ok(Lookup::Interpret);
        };
        let facts = Self::facts(cpu);
        let key = (u64::from(cpu.eip) | u64::from(facts) << 32, hit.backing.0);
        if let Some(entry) = self.blocks.get_mut(&key) {
            // SAFETY: CodePages owns a stable cell until clear() discards all
            // entries; it changes only outside native invocations.
            if unsafe { *entry.version_ptr } != entry.version {
                // Promotion revokes writable aliases before the bytes are
                // compared, so they cannot change again before execution.
                let (offset, version, version_ptr) = bus.jit_code_stamp(cpu, hit.backing);
                if entry.address == 0
                    || bus.jit_code_bytes(offset, entry.source.len()) != &entry.source[..]
                {
                    self.stats.invalidations += 1;
                    self.blocks.remove(&key);
                    return Ok(Lookup::Cold);
                }
                entry.version = version;
                entry.version_ptr = version_ptr;
                self.stats.revalidations += 1;
            }
            #[cfg(feature = "jit-profile")]
            if entry.address == 0 {
                self.unsupported[usize::from(entry.opcode)] += 1;
            }
            return Ok(if entry.address == 0 {
                Lookup::Interpret
            } else {
                Lookup::Native(Native {
                    address: entry.address,
                    instructions: entry.instructions,
                    version: entry.version_ptr,
                })
            });
        }
        let heat = &mut self.heat
            [(cpu.eip ^ (cpu.eip >> 12) ^ (hit.backing.0 as u32 >> 7)) as usize & (HEAT - 1)];
        if heat.0 != cpu.eip {
            *heat = (cpu.eip, 1);
            return Ok(Lookup::Cold);
        }
        heat.1 += 1;
        if heat.1 < HOT {
            return Ok(Lookup::Cold);
        }
        *heat = (0, 0);
        if self.blocks.len() >= BLOCKS {
            self.clear(bus)?;
        }
        #[cfg(all(feature = "jit-profile", target_os = "linux"))]
        let compilation_start = std::time::Instant::now();
        // Promotion revokes writable aliases before any native execution.
        let (offset, mut version, mut version_ptr) = bus.jit_code_stamp(cpu, hit.backing);
        let window = (4096 - (linear as usize & 4095))
            .min(WINDOW)
            .min((u32::MAX - cpu.eip) as usize + 1);
        let bytes = bus.jit_code_bytes(offset, window);
        let decoded = decode::block(bytes, cpu.eip, BLOCK_INSTRUCTIONS);
        #[cfg(feature = "jit-profile")]
        let opcode = profile_key(bytes);
        let Some(compiled) = compile::block(&decoded, facts) else {
            self.stats.unsupported_entries += 1;
            self.blocks.insert(
                key,
                Entry {
                    version,
                    version_ptr,
                    address: 0,
                    instructions: 0,
                    source: Box::default(),
                    #[cfg(feature = "jit-profile")]
                    opcode,
                },
            );
            return Ok(Lookup::Interpret);
        };
        let address = match self.arena.publish(&compiled.words) {
            Some(address) => address,
            None => {
                // No native code is active; every entry and watch is retired.
                self.clear(bus)?;
                (_, version, version_ptr) = bus.jit_code_stamp(cpu, hit.backing);
                self.arena
                    .publish(&compiled.words)
                    .ok_or("JIT block larger than the arena")?
            }
        };
        self.stats.compiled_blocks += 1;
        #[cfg(all(feature = "jit-profile", target_os = "linux"))]
        {
            self.stats.compile_nanoseconds += compilation_start.elapsed().as_nanos();
        }
        self.blocks.insert(
            key,
            Entry {
                version,
                version_ptr,
                address,
                instructions: compiled.instructions,
                source: bus.jit_code_bytes(offset, compiled.bytes).into(),
                #[cfg(feature = "jit-profile")]
                opcode,
            },
        );
        Ok(Lookup::Native(Native {
            address,
            instructions: compiled.instructions,
            version: version_ptr,
        }))
    }

    /// Run a chain starting with the block at `address` if it fits `limit`.
    /// Returns completed instructions and whether the chain stopped before an
    /// instruction that the interpreter must execute.
    fn invoke(&mut self, cpu: &mut Cpu, block: Native, limit: u64) -> Option<(u64, bool)> {
        let Native {
            address,
            instructions,
            version,
        } = block;
        if u64::from(instructions) > limit {
            return None;
        }
        let slot = usize::from(cpu.cpl() == 3) * PLANE + compile::table_slot(cpu.eip);
        // A validated block may be reached from any chain in this context.
        if self.table[slot][1] == self.miss {
            self.filled.push(slot as u16);
        }
        self.table[slot] = [cpu.eip, address as u32];
        let linear = cpu.seg(SegmentReg::Cs).base.wrapping_add(cpu.eip);
        // The fetch-cache entry found by lookup is still current.
        let global = cpu.fetch_translation_is_global(LinearAddr(linear));
        self.pages[slot] = (linear & !4095 | u32::from(global), version);
        let plane = slot & !(PLANE - 1);
        cpu.begin_instruction();
        cpu.segment_override = None;
        let budget = limit.min(u64::from(u32::MAX)) as u32;
        let mut frame = Frame {
            regs: cpu.regs,
            eip: cpu.eip,
            remaining: budget,
            table: self.table[plane..].as_ptr() as usize as u32,
            dispatch: self.dispatch as u32,
            ..Frame::default()
        };
        frame.set_eflags(cpu.effective_eflags());
        let (tlb, state) = cpu.translation_cache.jit_view();
        frame.tlb = tlb as usize as u32;
        frame.tlb_state = state as u32;
        let (bases, limits, based, based_write) = cpu.jit_based_segments();
        let fs_gs = |mask: u32| (mask >> SegmentReg::Fs as u32) & 3;
        frame.based = fs_gs(based) | fs_gs(based_write) << 2;
        frame.bases = bases;
        frame.limits = limits;
        // SAFETY: the entry, dispatcher and blocks were emitted for this ABI
        // and published in the live arena; table entries name only blocks
        // validated in the current context. The frame outlives the call. TLB
        // pointers are HostBus RAM tokens whose write tags exclude watched
        // code, and the translation cache is not modified during the call.
        unsafe {
            let entry: Invoke = core::mem::transmute(self.entry);
            entry(&mut frame, address);
        }
        let executed = u64::from(budget - frame.remaining);
        cpu.regs = frame.regs;
        cpu.eip = frame.eip;
        cpu.jit_commit_flags(frame.eflags());
        cpu.advance_tsc(executed);
        self.stats.invocations += 1;
        Some((executed, frame.exit == EXIT_INTERPRET))
    }

    pub(crate) fn run_batch(
        &mut self,
        cpu: &mut Cpu,
        bus: &mut HostBus,
        start: u64,
        budget: u64,
    ) -> Result<BatchResult, String> {
        let mut ran = 0;
        // EIP of the last instruction interpreted as cold code. Its sequential
        // successor is no block entry; interpretation continues to a transfer.
        let mut cold = None::<u32>;
        while ran < budget {
            let shadowed = cpu.interrupt_shadow();
            let eligible = !shadowed
                && !cpu.halted
                && !cpu.code_default16()
                && cpu.seg(SegmentReg::Cs).limit == u32::MAX
                && cpu.eflags & (TF | VM) == 0;
            let (limit, attention) = bus
                .batch_attention_limit(start.wrapping_add(ran), budget - ran)
                .unwrap_or((1, false));
            let sequential = cold.is_some_and(|eip| cpu.eip.wrapping_sub(eip).wrapping_sub(1) < 15);
            cold = None;
            if eligible && sequential {
                cold = Some(cpu.eip);
            } else if eligible {
                self.sync(cpu, bus);
                let native = match self.lookup(cpu, bus)? {
                    Lookup::Native(block) => self.invoke(cpu, block, limit),
                    Lookup::Interpret => None,
                    Lookup::Cold => {
                        cold = Some(cpu.eip);
                        None
                    }
                };
                if let Some((executed, interpret)) = native {
                    ran += executed;
                    self.stats.native_steps += executed;
                    if executed != 0 {
                        bus.begin_instruction(
                            start.wrapping_add(ran).wrapping_sub(1),
                            cpu.tsc().wrapping_sub(1),
                        );
                    }
                    if executed == limit && attention {
                        return Ok(BatchResult {
                            ran,
                            stop: BatchStop::Attention {
                                instruction_step: start.wrapping_add(ran).wrapping_sub(1),
                                was_interruptible: false,
                            },
                        });
                    }
                    if !interpret {
                        continue;
                    }
                    self.stats.memory_exits += 1;
                }
            }
            let mut batch = if eligible || shadowed {
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

/// Profile bucket of an entry's first instruction: the opcode after prefixes
/// (0x100 + second byte for 0F), ModRM reg and form for common groups at
/// 0x200, and 0x400 for an operand-size prefix.
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
