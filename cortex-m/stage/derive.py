#!/usr/bin/env python3
"""Derive the Thumb-2 lowering, runtime and block tests from the ARMv6K ones.

usage: derive.py REPO_ROOT

Reads   src/x86/jit/arm32/{compile,runtime,tests}.rs   (never modified)
Writes  src/x86/jit/t32/{lower,runtime,tests}.rs

Every edit names the exact text it replaces and fails if that text is not
present the expected number of times, so a changed ARMv6K source stops the
script instead of producing a wrong file. The script is deterministic: running
it again rewrites the three files with identical bytes.
"""
import hashlib
import pathlib
import sys

EXPECTED = {
    "compile.rs": "8042245862488fdc",
    "runtime.rs": "909ac1e92104927a",
    "tests.rs": "eba0a1dab1ea2c07",
}


class Text:
    def __init__(self, name, text):
        self.name, self.text = name, text

    def rep(self, old, new, count=1):
        found = self.text.count(old)
        if found != count:
            sys.exit(f"{self.name}: expected {count} of {old[:70]!r}, found {found}")
        self.text = self.text.replace(old, new)

    def between(self, start, end, new):
        """Replace from `start` (unique) up to, not including, the next `end`."""
        if self.text.count(start) != 1:
            sys.exit(f"{self.name}: start marker not unique: {start[:70]!r}")
        a = self.text.index(start)
        b = self.text.find(end, a + len(start))
        if b < 0:
            sys.exit(f"{self.name}: end marker missing: {end[:70]!r}")
        self.text = self.text[:a] + new + self.text[b:]

    def absent(self, *needles):
        for needle in needles:
            if needle in self.text:
                sys.exit(f"{self.name}: {needle!r} survived")


def source(root, name):
    data = (root / "src/x86/jit/arm32" / name).read_bytes()
    digest = hashlib.sha256(data).hexdigest()[:16]
    if digest != EXPECTED[name]:
        sys.exit(f"arm32/{name}: sha256 {digest}, expected {EXPECTED[name]}; the reference changed")
    return Text(name, data.decode())


# --------------------------------------------------------------------------
def lower(root):
    t = source(root, "compile.rs")
    t.rep("//! i386 -> ARMv6K block lowering.\n", "//! i386 -> Thumb-2 block lowering.\n")
    t.rep(
        "//! Frame.result/auxiliary from their last live writer.\n//!\n",
        "//! Frame.result/auxiliary from their last live writer.\n//!\n"
        "//! Thumb-2 specifics: partial register writes use BFI/BFC, a shift by CL is\n"
        "//! its own instruction, wide constants are MOVW/MOVT, and a conditional\n"
        "//! instruction is a one-instruction IT block. Every code address stored in\n"
        "//! the frame or a dispatch-table entry has bit 0 set (Thumb state).\n//!\n",
    )
    # Comments only: the enum variants are spelled Cpsr.
    t.text = t.text.replace("CPSR", "APSR")
    t.rep(
        "/// for other targets and probe misses.\npub struct Shared {\n    pub words: Vec<u32>,\n",
        "/// for other targets and probe misses. `dispatch` and `miss` are halfword\n"
        "/// offsets into `code`.\npub struct Shared {\n    pub code: Vec<u16>,\n",
    )
    t.rep("        words: a.finish().unwrap(),\n", "        code: a.finish(),\n")
    t.rep(
        "pub struct Compiled {\n    pub words: Vec<u32>,\n",
        "pub struct Compiled {\n    pub code: Vec<u16>,\n",
    )
    t.between(
        "pub fn block(block: &Block, facts: u32) -> Option<Compiled> {\n",
        "/// Where the current x86 status flags live.\n",
        """pub fn block(block: &Block, facts: u32) -> Option<Compiled> {
    let check = Lower::new(facts);
    let count = block
        .instructions
        .iter()
        .position(|instruction| !check.supported(instruction.op))
        .unwrap_or(block.instructions.len());
    if count == 0 {
        return None;
    }
    let prefix = Block {
        instructions: block.instructions[..count].to_vec(),
        byte_len: 0,
    };
    let first = prefix.instructions[0].eip;
    let end = prefix.instructions[count - 1].next_eip;
    Some(Compiled {
        code: Lower::new(facts).emit(&prefix),
        instructions: count as u32,
        bytes: end.wrapping_sub(first) as usize,
    })
}

""",
    )
    t.rep(
        "fn mask(bits: u8) -> u32 {\n    u32::MAX >> (32 - bits)\n}\n",
        """fn mask(bits: u8) -> u32 {
    u32::MAX >> (32 - bits)
}

/// How a shift count reaches the shifter.
#[derive(Clone, Copy)]
enum By {
    Imm(Shift),
    /// Shift kind (0 LSL, 1 LSR, 2 ASR, 3 ROR) and the count register.
    Reg(u8, Reg),
}

/// rd = rn shifted `by`. With `s`, N, Z and the carry-out are set; a zero
/// register count changes no flag.
fn shift_by(a: &mut Assembler, by: By, s: bool, rd: Reg, rn: Reg) {
    match by {
        By::Imm(shift) if s => a.movs(rd, Operand::Reg(rn, shift)),
        By::Imm(shift) => a.mov(rd, Operand::Reg(rn, shift)),
        By::Reg(kind, count) => a.shift_reg(kind, s, rd, rn, count),
    }
}
""",
    )
    t.rep(
        "    fn emit(mut self, block: &Block) -> Option<Vec<u32>> {\n",
        "    fn emit(mut self, block: &Block) -> Vec<u16> {\n",
    )
    t.between(
        "    /// Write a value held in the low bits of `value` (other bits ignored).\n",
        "    // ----- Memory -----\n",
        """    /// Write a value held in the low bits of `value` (other bits ignored).
    fn write_low(&mut self, dst: Register, value: Reg) {
        let target = guest(dst.index);
        match (dst.bits, dst.shift) {
            (32 | 16, _) | (8, 0) if value == target => {}
            (32, _) => self.a.mov(target, value),
            (bits, shift) => self.a.bfi(target, value, shift, bits),
        }
    }

    /// Write a top-aligned value whose low 32 - bits bits are zero.
    fn write_top(&mut self, dst: Register, value: Reg) {
        let target = guest(dst.index);
        let a = &mut self.a;
        match (dst.bits, dst.shift) {
            (32, _) => {
                if value != target {
                    a.mov(target, value);
                }
            }
            (bits, shift) => {
                a.bfc(target, shift, bits);
                a.dp(
                    Dp::Orr,
                    false,
                    target,
                    target,
                    Operand::Reg(value, Shift::Lsr(32 - bits - shift)),
                );
            }
        }
    }

""",
    )
    t.rep(
        "        a.ldrd(R2, FRAME, TLB_STATE as u32);\n",
        "        a.ldrd(R2, R3, FRAME, TLB_STATE as u32);\n",
    )
    t.rep(
        "                        self.a.mvn(R0, target);\n                        self.a.pkhbt(target, R0, target);\n",
        "                        self.a.mvn(R0, target);\n                        self.a.bfi(target, R0, 0, 16);\n",
    )
    t.rep(
        "                    self.a.pkhbt(guest(2), R0, guest(2));\n",
        "                    self.a.bfi(guest(2), R0, 0, 16);\n",
    )
    # shift(): the count is an immediate shift or a register.
    t.rep(
        """        let operand = match amount {
            Some(count) => Operand::Reg(value, shift(count)),
            None => {
                self.a.dp(Dp::And, false, R2, guest(1), Operand::Imm(31));
                if kind == 4 {
                    self.a.dp(Dp::Rsb, false, R0, R2, Operand::Imm(32));
                    Operand::RegShift(value, 3, R0)
                } else {
                    Operand::RegShift(value, kind, R2)
                }
            }
        };
""",
        """        let by = match amount {
            Some(count) => By::Imm(shift(count)),
            None => {
                self.a.dp(Dp::And, false, R2, guest(1), Operand::Imm(31));
                if kind == 4 {
                    self.a.dp(Dp::Rsb, false, R0, R2, Operand::Imm(32));
                    By::Reg(3, R0)
                } else {
                    By::Reg(kind, R2)
                }
            }
        };
""",
    )
    t.rep(
        "            // ARM shifts and rotates by zero leave the value unchanged.\n            a.mov(value, operand);\n",
        "            // ARM shifts and rotates by zero leave the value unchanged.\n            shift_by(a, by, false, value, value);\n",
    )
    t.rep(
        "            if rotate {\n                a.mov(value, operand);\n",
        "            if rotate {\n                shift_by(a, by, false, value, value);\n",
    )
    t.rep("                a.movs(value, operand);\n", "                shift_by(a, by, true, value, value);\n")
    # double_shift(): the same for the bits leaving the destination.
    t.rep(
        """        // R3 = the source bits entering the vacated end.
        let shifted = match amount {
""",
        """        // R3 = the source bits entering the vacated end; `out` shifts the
        // destination.
        let out = match amount {
""",
    )
    t.rep(
        "                a.mov(R3, Operand::Reg(source, fill));\n                Operand::Reg(target, out)\n",
        "                a.mov(R3, Operand::Reg(source, fill));\n                By::Imm(out)\n",
    )
    t.rep(
        "                a.mov(R3, Operand::RegShift(source, fill, R2));\n                Operand::RegShift(target, out, R0)\n",
        "                a.shift_reg(fill, false, R3, source, R2);\n                By::Reg(out, R0)\n",
    )
    t.rep(
        "        if needed == 0 {\n            a.dp(Dp::Orr, false, target, R3, shifted);\n            return;\n        }\n",
        """        if needed == 0 {
            match out {
                By::Imm(shift) => a.dp(Dp::Orr, false, target, R3, Operand::Reg(target, shift)),
                By::Reg(..) => {
                    shift_by(a, out, false, R1, target);
                    a.dp(Dp::Orr, false, target, R3, R1);
                }
            }
            return;
        }
""",
    )
    t.rep("        a.movs(R1, shifted);\n", "        shift_by(a, out, true, R1, target);\n")
    t.rep(
        "                self.a.mov(R2, Operand::RegShift(R12, 0, R2));\n",
        "                self.a.shift_reg(0, false, R2, R12, R2);\n",
    )
    # 32-bit DIV/IDIV: SDIV/UDIV exist here, unlike on ARMv6K.
    t.rep(
        "            Op::Multiply { src, bits: 32, .. } => self.value(src, false),\n",
        "            Op::Multiply { src, bits: 32, .. } | Op::Divide { src, bits: 32, .. } => {\n"
        "                self.value(src, false)\n            }\n",
    )
    t.rep(
        "            Op::Exchange { kind, dst, src } => self.exchange(index, kind, dst, src, needed),\n",
        """            Op::Divide { src, signed, .. } => {
                // EDX:EAX / src when EDX is the zero or sign extension of
                // EAX, so the dividend fits 32 bits. Any other dividend, a
                // zero divisor and the one overflowing signed quotient leave
                // before the instruction; the interpreter raises #DE. The
                // interpreter keeps every flag, so only the scratch APSR is
                // used here.
                let divisor = self.load(src, index);
                let divisor = self.low(divisor, R1);
                self.clobber();
                let miss = self.miss(index);
                let (eax, edx) = (guest(0), guest(2));
                let a = &mut self.a;
                a.cmp(divisor, Operand::Imm(0));
                a.b_cond(Cond::Eq, miss);
                if signed {
                    a.cmp(edx, Operand::Reg(eax, Shift::Asr(31)));
                    a.b_cond(Cond::Ne, miss);
                    // INT_MIN / -1 does not fit.
                    a.cmn(divisor, Operand::Imm(1));
                    a.with_cond(Cond::Eq, |a| a.cmp(eax, Operand::Imm(0x8000_0000)));
                    a.b_cond(Cond::Eq, miss);
                    a.sdiv(R0, eax, divisor);
                } else {
                    a.cmp(edx, Operand::Imm(0));
                    a.b_cond(Cond::Ne, miss);
                    a.udiv(R0, eax, divisor);
                }
                // Both results exist before either register is written: the
                // divisor may be EAX or EDX.
                a.mls(R2, R0, divisor, eax);
                a.mov(eax, R0);
                a.mov(edx, R2);
            }
            Op::Exchange { kind, dst, src } => self.exchange(index, kind, dst, src, needed),
""",
    )
    t.absent("RegShift", "pkh", "uxtab", "words", "ARMv6", "Vec<u32>", "literal")
    return t.text


# --------------------------------------------------------------------------
def runtime(root):
    t = source(root, "runtime.rs")
    t.rep("//! ARMv6K block dispatch for the shared Machine.\n", "//! Thumb-2 block dispatch for the shared Machine.\n")
    t.rep(
        "//! often share pages in firmware and drivers. Blocks live until the arena or\n"
        "//! the bounded block map is reset.\n",
        "//! often share pages in firmware and drivers. Blocks live until the arena is\n"
        "//! reset. The block index is a fixed table ([`super::store`]); a full set\n"
        "//! drops one record, which only forgets that block.\n",
    )
    t.rep(
        "//! the next machine event; a precise exit hands its instruction to the\n//! interpreter.\n",
        "//! the next machine event; a precise exit hands its instruction to the\n//! interpreter.\n"
        "//!\n"
        "//! A chain that returns at a dispatch miss with budget left resumes from the\n"
        "//! same frame when the target already has a valid record. Native code cannot\n"
        "//! change mappings, CPL, CS, segment facts or watched code, so the context\n"
        "//! `sync` checked still holds. Every code address handed to native code has\n"
        "//! bit 0 set (Thumb state).\n",
    )
    t.between(
        "use super::code::Arena;\n",
        "\nconst HEAT: usize = 4096;\n",
        """use super::arena::Arena;
use super::lower::{self, EXIT_BOUNDARY, EXIT_INTERPRET, Frame, STACK_FACT, TABLE_BITS};
use super::store::{Record, Store};
use crate::machine::HostBus;
use crate::x86::cpu::SegmentReg;
use crate::x86::jit::decode;
use crate::x86::{BackingToken, BatchResult, BatchStop, Bus, Cpu, Interpreter, LinearAddr};
use alloc::{boxed::Box, format, string::String, vec::Vec};
""",
    )
    t.rep(
        "/// Native blocks average a few hundred bytes, so the 4 MiB arena fills first.\nconst BLOCKS: usize = 16384;\n",
        "/// Records in the block index: four-way sets. A 4 MiB arena holds about ten\n"
        "/// thousand blocks. On the TinyCore boot 16384 records replaced 21,324 live\n"
        "/// records in full sets and 32768 replaced 3,216.\nconst BLOCKS: usize = 32768;\n",
    )
    t.between("/// (EIP | facts << 32, backing).\n", "#[derive(Debug, Default)]\npub struct Stats {\n", "")
    t.rep(
        "    pub arena_clears: u64,\n",
        "    pub arena_clears: u64,\n"
        "    /// Dispatch misses resumed without leaving `invoke`.\n"
        "    pub resolved: u64,\n"
        "    /// Live records replaced in a full set of the block index.\n"
        "    pub store_evictions: u64,\n",
    )
    t.rep(
        "    arena: Arena,\n    entry: usize,\n    dispatch: usize,\n    miss: u32,\n    shared_words: usize,\n"
        "    blocks: HashMap<Key, Entry, BuildHasherDefault<KeyHasher>>,\n",
        "    arena: Arena,\n"
        "    /// Shared stubs, as Thumb addresses (bit 0 set).\n"
        "    entry: usize,\n    dispatch: usize,\n    miss: u32,\n    shared_bytes: usize,\n"
        "    store: Store,\n",
    )
    t.rep(
        "/// A valid native block.\n#[derive(Clone, Copy)]\nstruct Native {\n    address: usize,\n",
        "/// A valid native block.\n#[derive(Clone, Copy)]\nstruct Native {\n"
        "    /// Thumb address (bit 0 set).\n    address: usize,\n",
    )
    t.rep(
        "    /// Not yet admitted.\n    Cold,\n}\n",
        "    /// Not yet admitted.\n    Cold,\n}\n\n"
        "/// What the block index holds for the current EIP.\n"
        "enum Probe {\n    Known(Lookup),\n"
        "    /// No record. The execute translation names this backing byte.\n"
        "    Unknown(BackingToken),\n}\n",
    )
    t.between(
        "    pub(crate) fn with_capacity(bytes: usize) -> Result<Self, String> {\n",
        "    fn clear_table(&mut self) {\n",
        """    pub(crate) fn with_capacity(bytes: usize) -> Result<Self, String> {
        let mut arena = Arena::new(bytes)?;
        let shared = lower::shared();
        let base = arena.publish(&shared.code).ok_or("JIT arena too small")?;
        let miss = (base + shared.miss * 2) as u32 | 1;
        Ok(Self {
            shared_bytes: arena.used_bytes(),
            arena,
            entry: base | 1,
            dispatch: (base + shared.dispatch * 2) | 1,
            miss,
            store: Store::new(BLOCKS),
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
        self.store.clear();
        self.clear_table();
        self.arena.reset(self.shared_bytes);
        // No block can observe an old version after this point.
        bus.jit_release_code_pages();
        self.stats.arena_clears += 1;
        Ok(())
    }

""",
    )
    t.rep('            "arm32 jit: {:?}, {} blocks, code {} KiB\\n",\n', '            "t32 jit: {:?}, {} blocks, code {} KiB\\n",\n')
    t.rep("            self.blocks.len(),\n", "            self.store.live(),\n")
    t.between(
        "    /// Classify the current EIP, admitting or compiling it when hot.\n",
        "    pub(crate) fn run_batch(\n",
        """    fn insert(&mut self, record: Record) {
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
            return Probe::Known(Lookup::Interpret);
        };
        let Some(slot) = self.store.find(cpu.eip, Self::facts(cpu), hit.backing.0) else {
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
                return Probe::Known(Lookup::Cold);
            }
            self.store.revalidate(slot, version, version_ptr);
            record.version_ptr = version_ptr;
            self.stats.revalidations += 1;
        }
        if record.address == 0 {
            #[cfg(feature = "jit-profile")]
            if profile {
                self.unsupported[usize::from(record.opcode)] += 1;
            }
            return Probe::Known(Lookup::Interpret);
        }
        Probe::Known(Lookup::Native(Native {
            address: record.address as usize,
            instructions: record.instructions,
            version: record.version_ptr,
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
            return Ok(Lookup::Cold);
        }
        heat.1 += 1;
        if heat.1 < HOT {
            return Ok(Lookup::Cold);
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
        let decoded = decode::block(bytes, cpu.eip, BLOCK_INSTRUCTIONS);
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
        }))
    }

    /// Enter the validated block for the current EIP in the dispatch table;
    /// returns the first slot of its privilege plane.
    fn publish(&mut self, cpu: &Cpu, block: Native) -> usize {
        let slot = usize::from(cpu.cpl() == 3) * PLANE + lower::table_slot(cpu.eip);
        // A validated block may be reached from any chain in this context.
        if self.table[slot][1] == self.miss {
            self.filled.push(slot as u16);
        }
        self.table[slot] = [cpu.eip, block.address as u32];
        let linear = cpu.seg(SegmentReg::Cs).base.wrapping_add(cpu.eip);
        // The fetch-cache entry found by the lookup is still current.
        let global = cpu.fetch_translation_is_global(LinearAddr(linear));
        self.pages[slot] = (linear & !4095 | u32::from(global), block.version);
        slot & !(PLANE - 1)
    }

    /// Run a chain starting with `block` if it fits `limit`. Returns completed
    /// instructions and whether the chain stopped before an instruction that
    /// the interpreter must execute.
    fn invoke(
        &mut self,
        cpu: &mut Cpu,
        bus: &mut HostBus,
        block: Native,
        limit: u64,
    ) -> Option<(u64, bool)> {
        if u64::from(block.instructions) > limit {
            return None;
        }
        let plane = self.publish(cpu, block);
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
        let (bases, limits, based, based_write) = cpu.jit_based_segments();
        let fs_gs = |mask: u32| (mask >> SegmentReg::Fs as u32) & 3;
        frame.based = fs_gs(based) | fs_gs(based_write) << 2;
        frame.bases = bases;
        frame.limits = limits;
        let mut address = block.address;
        loop {
            // A lookup below may promote a code page, which revokes write
            // rows in place; take the current view before every entry.
            let (tlb, state) = cpu.translation_cache.jit_view();
            frame.tlb = tlb as usize as u32;
            frame.tlb_state = state as u32;
            // SAFETY: the entry, dispatcher and blocks were emitted for this
            // ABI and published in the live arena; table entries name only
            // blocks validated in the current context. The frame outlives the
            // call. TLB pointers are HostBus RAM tokens whose write tags
            // exclude watched code, and the translation cache is not modified
            // during the call.
            unsafe {
                let entry: Invoke = core::mem::transmute(self.entry);
                entry(&mut frame, address);
            }
            self.stats.invocations += 1;
            if frame.exit != EXIT_BOUNDARY || frame.remaining == 0 {
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
                }
                _ => break,
            }
        }
        let executed = u64::from(budget - frame.remaining);
        cpu.regs = frame.regs;
        cpu.eip = frame.eip;
        cpu.jit_commit_flags(frame.eflags());
        cpu.advance_tsc(executed);
        Some((executed, frame.exit == EXIT_INTERPRET))
    }

""",
    )
    t.rep(
        "                    Lookup::Native(block) => self.invoke(cpu, block, limit),\n",
        "                    Lookup::Native(block) => self.invoke(cpu, bus, block, limit),\n",
    )
    t.absent("compile::", "HashMap", "KeyHasher", "self.blocks", "shared_words", "ARMv6", "arm32")
    return t.text


# --------------------------------------------------------------------------
SHARED_STUB = """#[test]
fn shared_stub_matches_reference() {
    // llvm-mc -triple=thumbv8m.main-none-eabi for: push.w {r4-r12, lr};
    // mov.w lr, r0; ldm.w lr, {r4-r11}; bx r1; dispatch: ldr.w r1, [lr, #60];
    // eor.w r2, r0, r0, lsr #12; lsl.w r2, r2, #19; add.w r1, r1, r2, lsr #16;
    // ldr.w r2, [r1]; cmp.w r2, r0; it eq; ldreq.w pc, [r1, #4]; miss:
    // str.w r0, [lr, #32]; mov.w r0, #0; str.w r0, [lr, #52];
    // stm.w lr, {r4-r11}; pop.w {r4-r12, pc}. It pins the frame offsets too.
    let shared = lower::shared();
    assert_eq!((shared.dispatch, shared.miss), (7, 22));
    assert_eq!(
        &shared.code[..],
        &[
            0xe92d, 0x5ff0, 0xea4f, 0x0e00, 0xe89e, 0x0ff0, 0x4708, 0xf8de, 0x103c, 0xea80, 0x3210,
            0xea4f, 0x42c2, 0xeb01, 0x4112, 0xf8d1, 0x2000, 0xebb2, 0x0f00, 0xbf08, 0xf8d1, 0xf004,
            0xf8ce, 0x0020, 0xf04f, 0x0000, 0xf8ce, 0x0034, 0xe88e, 0x0ff0, 0xe8bd, 0x9ff0
        ]
    );
}

"""

DIVISION = """
/// The block leaves before its last instruction, having completed the others
/// exactly as the interpreter does.
fn leaves_before_last(bytes: &[u8], completed: u32) {
    let fixture = Fixture::new(bytes);
    let mut memory = Bytes(alloc::vec![0; 0x10000]);
    memory.0[0x100..0x100 + bytes.len()].copy_from_slice(bytes);
    for seed in 0..8 {
        let mut expected = initial(seed);
        let mut start = expected.clone();
        let (actual, executed) = fixture.run_with(&mut start, fixture.count);
        Interpreter::run_batch(&mut expected, &mut memory, 0, u64::from(completed));
        assert_eq!(
            (actual.exit, executed),
            (lower::EXIT_INTERPRET, completed),
            "seed={seed} bytes={bytes:02x?}"
        );
        assert_eq!(actual.regs, expected.regs, "seed={seed} bytes={bytes:02x?}");
        assert_eq!(
            actual.eflags() | 2,
            expected.effective_eflags(),
            "seed={seed} bytes={bytes:02x?}"
        );
        assert_eq!(actual.eip, expected.eip, "seed={seed} bytes={bytes:02x?}");
    }
}

#[test]
fn division_matches_interpreter_and_leaves_before_a_fault() {
    for divisor in [
        1u32,
        2,
        3,
        7,
        0x10,
        0x7fff_ffff,
        0x8000_0000,
        0xffff_fffe,
        0x1234_5678,
    ] {
        // mov ecx,divisor; xor edx,edx; div ecx
        let mut unsigned = alloc::vec![0xb9];
        unsigned.extend(divisor.to_le_bytes());
        unsigned.extend([0x31, 0xd2, 0xf7, 0xf1]);
        compare(&unsigned, 40);
        // mov ecx,divisor; cdq; idiv ecx
        let mut signed = alloc::vec![0xb9];
        signed.extend(divisor.to_le_bytes());
        signed.extend([0x99, 0xf7, 0xf9]);
        compare(&signed, 40);
    }
    // The divisor may be the dividend's own registers.
    compare(&[0xb8, 0x39, 0x30, 0x00, 0x00, 0x31, 0xd2, 0xf7, 0xf0], 8); // div eax
    compare(&[0xb8, 0x39, 0x30, 0x00, 0x00, 0x99, 0xf7, 0xf8], 8); // idiv eax
    // mov ecx,0; xor edx,edx; div ecx: #DE belongs to the interpreter.
    leaves_before_last(&[0xb9, 0, 0, 0, 0, 0x31, 0xd2, 0xf7, 0xf1], 2);
    // mov edx,1; div ecx: the dividend does not fit 32 bits.
    leaves_before_last(&[0xba, 1, 0, 0, 0, 0xf7, 0xf1], 1);
    // mov edx,5; idiv ecx: EDX is not the sign of EAX.
    leaves_before_last(&[0xba, 5, 0, 0, 0, 0xf7, 0xf9], 1);
    // mov eax,80000000h; mov ecx,-1; cdq; idiv ecx: the quotient overflows.
    leaves_before_last(
        &[
            0xb8, 0, 0, 0, 0x80, 0xb9, 0xff, 0xff, 0xff, 0xff, 0x99, 0xf7, 0xf9,
        ],
        3,
    );
}
"""


def tests(root):
    t = source(root, "tests.rs")
    t.rep(
        "//! Run under `qemu-arm -cpu arm11mpcore` to reject non-ARMv6K encodings.\n",
        "//! Run under `qemu-arm -cpu cortex-a7`; `asm::tests` pins the encodings to\n"
        "//! Armv8-M Mainline.\n",
    )
    t.rep(
        "use super::asm::{Assembler, Cond, Dp, LR, Operand, R0, R1, R2, R12, Reg, Shift};\n"
        "use super::code::Arena;\nuse super::compile::{self, Frame, STACK_FACT};\n",
        "use super::arena::Arena;\nuse super::lower::{self, Frame, STACK_FACT};\n",
    )
    t.rep(
        "struct Fixture {\n    _arena: Arena,\n    entry: usize,\n",
        "struct Fixture {\n    _arena: Arena,\n    /// Code addresses carry the Thumb bit.\n    entry: usize,\n",
    )
    t.rep("        let entry = arena.publish(&shared.words).unwrap();\n", "        let entry = arena.publish(&shared.code).unwrap();\n")
    t.rep("        let block = arena.publish(&compiled.words).unwrap();\n", "        let block = arena.publish(&compiled.code).unwrap();\n")
    t.rep("        let miss = (entry + shared.miss * 4) as u32;\n", "        let miss = (entry + shared.miss * 2) as u32 | 1;\n")
    t.rep(
        "            entry,\n            dispatch: entry + shared.dispatch * 4,\n            block,\n",
        "            entry: entry | 1,\n            dispatch: (entry + shared.dispatch * 2) | 1,\n            block: block | 1,\n",
    )
    t.between("#[test]\nfn encodings_are_armv6k() {\n", "#[test]\nfn register_alu_matches_interpreter() {\n", SHARED_STUB)
    t.text = t.text.replace("compile::", "lower::")
    t.text += DIVISION
    t.absent("compile::", "super::code", "words", "armv6", "ARMv6", "arm11")
    return t.text


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    root = pathlib.Path(sys.argv[1])
    out = root / "src/x86/jit/t32"
    if not (out / "asm.rs").is_file():
        sys.exit(f"{out}/asm.rs is missing: install the staged files first")
    for name, make in (("lower.rs", lower), ("runtime.rs", runtime), ("tests.rs", tests)):
        text = make(root)
        (out / name).write_text(text)
        print(f"{name}: {len(text.splitlines())} lines, sha256 {hashlib.sha256(text.encode()).hexdigest()[:16]}")


if __name__ == "__main__":
    main()
