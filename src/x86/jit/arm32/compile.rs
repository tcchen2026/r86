//! i386 -> ARMv6K block lowering.
//!
//! R4..R11 hold EAX..EDI and LR points to the invocation [`Frame`]. R0..R3 and
//! R12 are scratch; SoftMMU guards use R2, R3 and R12 and leave the host bias
//! in R3. The CPSR carries x86 SF/ZF/OF and CF between instructions. Narrow
//! arithmetic shifts its operands into the high bits so NZCV describes the x86
//! width. CF is either ARM C (after ADD-like writers) or its inverse (after
//! SUB-like writers, where ARM C means "no borrow"); the compiler tracks that
//! polarity. Frame.nzcv always uses the inverted form. PF and AF stay in
//! Frame.result/auxiliary from their last live writer.
//!
//! Flags are spilled before any instruction that can exit, so every exit sees
//! complete Frame flags. A software TLB miss, unsupported segment or other slow
//! case leaves before the instruction, which the interpreter then executes with
//! its own fault and device semantics. Native code writes guest RAM only through
//! write-tag hits; those exclude watched code pages, so compiled bytes cannot
//! change during an invocation. DS/ES/SS accesses require the block's compiled
//! flat-segment facts; FS/GS check their dynamic base, limit and permission.
//!
//! Undefined flag values follow the shared interpreter, which is the
//! differential oracle: logic and shifts clear AF; SHLD/SHRD keep AF, and OF
//! unless the count is one; MUL/IMUL write only CF/OF; BSF/BSR write only ZF
//! and keep the destination for a zero source.

use super::asm::{
    Assembler, Cond, Dp, LR, Label, Operand, PC, R0, R1, R2, R3, R12, Reg, Shift, encode_imm, regs,
};
use crate::x86::cpu::{LINEAR_TRANSLATION_CACHE_BITS, LinearTranslationCacheEntry, SegmentReg};
use crate::x86::jit::compile::{STATUS, flag_requirements};
use crate::x86::jit::decode::{
    Address, Alu, Block, BranchCondition, ExchangeOp, Op, Register, Value,
};
use alloc::vec::Vec;
use core::mem::{offset_of, size_of};

pub const FRAME: Reg = LR;
const C_BIT: u32 = 1 << 29;
const V_BIT: u32 = 1 << 28;
const Z_BIT: u32 = 1 << 30;
const N_BIT: u32 = 1 << 31;
const PF_AF: u32 = 0x14;
/// Block fact: SS is a flat, writable 32-bit stack.
pub const STACK_FACT: u32 = 1 << 6;

/// Native invocation state. Generated code uses only these explicit offsets.
#[repr(C, align(8))]
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub regs: [u32; 8],
    pub eip: u32,
    /// ARM N/Z/C/V holding x86 SF/ZF/!CF/OF.
    pub nzcv: u32,
    /// The low byte gives PF; bit 4 of result ^ auxiliary gives AF.
    pub result: u32,
    pub auxiliary: u32,
    /// Non-status EFLAGS. CLD/STD update DF here.
    pub eflags: u32,
    /// Why the chain returned; `eip` always names the next instruction.
    pub exit: u32,
    /// Instructions the chain may still complete. Each block reserves its
    /// full length on entry; a precise exit refunds the unexecuted rest.
    pub remaining: u32,
    /// Dispatch table for the current privilege: [eip, code] pairs.
    pub table: u32,
    /// Loaded together with LDRD by every guard.
    pub tlb_state: u32,
    pub tlb: u32,
    pub dispatch: u32,
    /// Bits 0/1: FS/GS readable; bits 2/3: FS/GS writable.
    pub based: u32,
    pub bases: [u32; 2],
    pub limits: [u32; 2],
}

const fn field(offset: usize) -> i32 {
    offset as i32
}
const EIP: i32 = field(offset_of!(Frame, eip));
const NZCV: i32 = field(offset_of!(Frame, nzcv));
const RESULT: i32 = field(offset_of!(Frame, result));
const AUXILIARY: i32 = field(offset_of!(Frame, auxiliary));
const EFLAGS: i32 = field(offset_of!(Frame, eflags));
const EXIT: i32 = field(offset_of!(Frame, exit));
const REMAINING: i32 = field(offset_of!(Frame, remaining));
const TABLE: i32 = field(offset_of!(Frame, table));
const DISPATCH: i32 = field(offset_of!(Frame, dispatch));
const TLB: i32 = field(offset_of!(Frame, tlb));
const TLB_STATE: i32 = field(offset_of!(Frame, tlb_state));
const BASED: i32 = field(offset_of!(Frame, based));
const BASES: i32 = field(offset_of!(Frame, bases));
const LIMITS: i32 = field(offset_of!(Frame, limits));
const _: () = assert!(offset_of!(Frame, regs) == 0);
const _: () = assert!(TLB == TLB_STATE + 4 && TLB_STATE % 8 == 0);

// One 16-byte row: the low word of the bias, then the read and write tags.
const _: () = assert!(size_of::<LinearTranslationCacheEntry>() == 16);
const TAG: i32 = 8;
const WRITE_TAG: i32 = 12;

impl Frame {
    pub fn set_eflags(&mut self, eflags: u32) {
        self.nzcv = (eflags & 0xc0) << 24 | (!eflags & 1) << 29 | (eflags & 0x800) << 17;
        // Zero has even parity (PF=1); one has odd parity.
        self.result = u32::from(eflags & 4 == 0);
        self.auxiliary = self.result ^ (eflags & 0x10);
        self.eflags = eflags & !STATUS;
    }

    pub fn eflags(&self) -> u32 {
        let n = self.nzcv;
        let parity = (self.result as u8).count_ones() & 1 == 0;
        self.eflags
            | (!n >> 29 & 1)
            | u32::from(parity) << 2
            | ((self.auxiliary ^ self.result) & 0x10)
            | (n >> 24 & 0xc0)
            | (n >> 17 & 0x800)
    }
}

/// The chain returned at a block boundary: dispatch miss or end of budget.
pub const EXIT_BOUNDARY: u32 = 0;
/// The chain stopped before an instruction that needs the interpreter.
pub const EXIT_INTERPRET: u32 = 1;

/// log2 of the dispatch table entries per privilege plane.
pub const TABLE_BITS: u8 = 13;

/// Shared code: `entry(frame, block)` at offset 0, then the dispatcher at
/// `Shared::dispatch`. A table entry is [eip, code]; empty entries name
/// `Shared::miss`, which returns for the runtime to resolve `eip`. Blocks
/// probe the slot of a static target themselves and use the dispatcher only
/// for other targets and probe misses.
pub struct Shared {
    pub words: Vec<u32>,
    pub dispatch: usize,
    pub miss: usize,
}

pub fn shared() -> Shared {
    let mut a = Assembler::new();
    a.push(regs(&[4, 5, 6, 7, 8, 9, 10, 11, 12, 14]));
    a.mov(FRAME, R0);
    a.ldm(FRAME, GUESTS);
    a.bx(R1);
    // R0 = next EIP; flags are already in the frame.
    let dispatch = a.offset();
    a.ldr(R1, FRAME, TABLE);
    a.dp(Dp::Eor, false, R2, R0, Operand::Reg(R0, Shift::Lsr(12)));
    a.mov(R2, Operand::Reg(R2, Shift::Lsl(32 - TABLE_BITS)));
    a.dp(
        Dp::Add,
        false,
        R1,
        R1,
        Operand::Reg(R2, Shift::Lsr(29 - TABLE_BITS)),
    );
    a.ldr(R2, R1, 0);
    a.cmp(R2, R0);
    a.with_cond(Cond::Eq, |a| a.ldr(PC, R1, 4));
    let miss = a.offset();
    a.str(R0, FRAME, EIP);
    a.mov(R0, Operand::Imm(EXIT_BOUNDARY));
    a.str(R0, FRAME, EXIT);
    a.stm(FRAME, GUESTS);
    a.pop(regs(&[4, 5, 6, 7, 8, 9, 10, 11, 12, 15]));
    Shared {
        words: a.finish().unwrap(),
        dispatch,
        miss,
    }
}

/// Table slot for `eip`, matching the dispatcher's hash.
pub fn table_slot(eip: u32) -> usize {
    ((eip ^ (eip >> 12)) as usize) & ((1 << TABLE_BITS) - 1)
}

const GUESTS: u16 = regs(&[4, 5, 6, 7, 8, 9, 10, 11]);
const ESP: Reg = guest(4);
const EBP: Reg = guest(5);

const fn guest(index: u8) -> Reg {
    Reg(4 + index)
}

pub struct Compiled {
    pub words: Vec<u32>,
    pub instructions: u32,
    /// Guest bytes the native code depends on, from the block start.
    pub bytes: usize,
}

/// Compile the longest supported prefix of `block`. `facts` are the flat
/// segment proofs (`Cpu::jit_segment_masks`) that every entry must match.
pub fn block(block: &Block, facts: u32) -> Option<Compiled> {
    let check = Lower::new(facts);
    let mut count = block
        .instructions
        .iter()
        .position(|instruction| !check.supported(instruction.op))
        .unwrap_or(block.instructions.len());
    while count != 0 {
        let prefix = Block {
            instructions: block.instructions[..count].to_vec(),
            byte_len: 0,
        };
        if let Some(words) = Lower::new(facts).emit(&prefix) {
            let first = prefix.instructions[0].eip;
            let end = prefix.instructions[count - 1].next_eip;
            return Some(Compiled {
                words,
                instructions: count as u32,
                bytes: end.wrapping_sub(first) as usize,
            });
        }
        // The literal pool must stay within LDR's 4 KiB reach.
        count /= 2;
    }
    None
}

/// Where the current x86 status flags live.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Flags {
    /// Frame.nzcv is current; the CPSR is unrelated.
    Frame,
    /// Frame.nzcv is current and the CPSR holds the same value.
    Synced,
    /// The CPSR is current. `true`: ARM C is x86 CF; `false`: ARM C is !CF.
    Cpsr(bool),
    /// CPSR N/Z/V are current; Frame.nzcv holds the current carry.
    CpsrNzv,
}

#[derive(Clone, Copy)]
enum Src {
    /// The low bits of a register; higher bits may be unrelated.
    Reg(Reg),
    /// Bits 8..15 of a guest register (AH..BH).
    High(Reg),
    Imm(u32),
}

fn register(register: Register) -> Src {
    if register.shift == 8 {
        Src::High(guest(register.index))
    } else {
        Src::Reg(guest(register.index))
    }
}

fn mask(bits: u8) -> u32 {
    u32::MAX >> (32 - bits)
}

struct Lower {
    a: Assembler,
    flags: Flags,
    facts: u32,
    misses: Vec<(Label, u32)>,
    tail: Label,
}

impl Lower {
    fn new(facts: u32) -> Self {
        let mut a = Assembler::new();
        let tail = a.label();
        Self {
            a,
            flags: Flags::Frame,
            facts,
            misses: Vec::new(),
            tail,
        }
    }

    fn segment(&self, segment: SegmentReg, write: bool) -> bool {
        match segment {
            SegmentReg::Fs | SegmentReg::Gs => true,
            SegmentReg::Cs => false,
            segment => self.facts & 1 << (segment as u32 + if write { 8 } else { 0 }) != 0,
        }
    }

    fn value(&self, value: Value, write: bool) -> bool {
        match value {
            Value::Memory { address, .. } => self.segment(address.segment, write),
            _ => true,
        }
    }

    fn supported(&self, op: Op) -> bool {
        let stack = self.facts & STACK_FACT != 0;
        match op {
            Op::Nop
            | Op::ByteSwap { .. }
            | Op::Carry { .. }
            | Op::Direction { .. }
            | Op::Lea { .. }
            | Op::SignExtendPair { .. }
            | Op::IncDec { .. }
            | Op::Not { .. }
            | Op::Negate { .. }
            | Op::Branch { .. } => true,
            Op::Mov { src, .. } | Op::Extend { src, .. } => self.value(src, false),
            Op::Store { address, .. } => self.segment(address.segment, true),
            Op::Alu { kind, dst, src } => {
                (dst.bits == 32 || !matches!(kind, Alu::Adc | Alu::Sbb)) && self.value(src, false)
            }
            Op::Compare { lhs, .. } => self.value(lhs, false),
            Op::MemoryAlu {
                kind,
                address,
                bits,
                ..
            } => {
                (bits == 32 || !matches!(kind, Alu::Adc | Alu::Sbb))
                    && self.segment(address.segment, true)
            }
            Op::Shift { dst, kind, count } => {
                matches!(
                    count,
                    Value::Immediate(_)
                        | Value::Register(Register {
                            index: 1,
                            bits: 8,
                            shift: 0
                        })
                ) && match dst {
                    Value::Register(dst) => kind <= 2 || (kind <= 4 && dst.bits == 32),
                    Value::Memory { address, .. } => {
                        kind <= 2 && self.segment(address.segment, true)
                    }
                    Value::Immediate(_) => false,
                }
            }
            Op::DoubleShift { dst, src, .. } => dst.bits == 32 && src.bits == 32,
            Op::Imul { dst, lhs, rhs } => {
                dst.bits == 32 && self.value(lhs, false) && self.value(rhs, false)
            }
            Op::Multiply { src, bits: 32, .. } => self.value(src, false),
            Op::Exchange { dst, src, .. } => src.bits == 32 && self.value(dst, true),
            Op::ConditionalMove { dst, src, .. } => dst.bits == 32 && self.value(src, false),
            Op::SetCondition { dst, .. } => self.value(dst, true),
            Op::BitTest {
                src, index, kind, ..
            } => match src {
                Value::Register(_) => true,
                Value::Memory { address, .. } => {
                    matches!(index, Value::Immediate(_)) && self.segment(address.segment, kind != 0)
                }
                Value::Immediate(_) => false,
            },
            Op::BitScan { dst, src, .. } => dst.bits == 32 && self.value(src, false),
            Op::Movs {
                repeat: false,
                segment,
                ..
            } => self.segment(segment, false) && self.segment(SegmentReg::Es, true),
            Op::Push { src, bits: 32 } => stack && self.value(src, false),
            Op::Pop { dst } => stack && dst.bits == 32,
            Op::Call { target, bits: 32 } => stack && self.value(target, false),
            Op::Return { bits: 32, .. } | Op::Leave { bits: 32 } => stack,
            Op::IndirectJump { src } => {
                matches!(
                    src,
                    Value::Register(Register { bits: 32, .. }) | Value::Memory { bits: 32, .. }
                ) && self.value(src, false)
            }
            _ => false,
        }
    }

    fn emit(mut self, block: &Block) -> Option<Vec<u32>> {
        let needed = flag_requirements(block);
        let count = block.instructions.len() as u32;
        // Reserve the whole block, or return before its first instruction.
        let budget = self.a.label();
        self.a.ldr(R0, FRAME, REMAINING);
        self.a.dp(Dp::Sub, true, R0, R0, Operand::Imm(count));
        self.a.b_cond(Cond::Cc, budget);
        self.a.str(R0, FRAME, REMAINING);
        let mut terminated = false;
        for (index, instruction) in block.instructions.iter().enumerate() {
            terminated =
                self.instruction(index, instruction.op, instruction.next_eip, needed[index]);
        }
        if !terminated {
            let last = block.instructions.last().unwrap();
            self.exit_to(last.next_eip);
        }
        for (label, code) in core::mem::take(&mut self.misses) {
            let index = code as usize - 1;
            self.a.bind(label);
            self.a.ldr(R1, FRAME, REMAINING);
            self.a
                .dp(Dp::Add, false, R1, R1, Operand::Imm(count - index as u32));
            self.a.str(R1, FRAME, REMAINING);
            self.a.mov_imm(R0, block.instructions[index].eip);
            self.a.str(R0, FRAME, EIP);
            self.a.mov(R0, Operand::Imm(EXIT_INTERPRET));
            self.a.b(self.tail);
        }
        self.a.bind(budget);
        self.a.mov_imm(R0, block.instructions[0].eip);
        self.a.str(R0, FRAME, EIP);
        self.a.mov(R0, Operand::Imm(EXIT_BOUNDARY));
        self.a.bind(self.tail);
        self.a.str(R0, FRAME, EXIT);
        self.a.stm(FRAME, GUESTS);
        self.a.pop(regs(&[4, 5, 6, 7, 8, 9, 10, 11, 12, 15]));
        self.a.finish()
    }

    // ----- Flag state -----

    /// Make Frame.nzcv current. Uses R3 and R12.
    fn spill(&mut self) {
        let a = &mut self.a;
        match self.flags {
            Flags::Frame | Flags::Synced => {}
            Flags::Cpsr(direct) => {
                a.mrs(R12);
                if direct {
                    a.dp(Dp::Eor, false, R12, R12, Operand::Imm(C_BIT));
                }
                a.str(R12, FRAME, NZCV);
                self.flags = if direct { Flags::Frame } else { Flags::Synced };
            }
            Flags::CpsrNzv => {
                a.ldr(R3, FRAME, NZCV);
                a.mrs(R12);
                a.dp(Dp::And, false, R3, R3, Operand::Imm(C_BIT));
                a.dp(Dp::Bic, false, R12, R12, Operand::Imm(C_BIT));
                a.dp(Dp::Orr, false, R12, R12, R3);
                a.str(R12, FRAME, NZCV);
                a.msr_flags(R12);
                self.flags = Flags::Synced;
            }
        }
    }

    /// Spill before emitting CPSR writes that are not guest flags.
    fn clobber(&mut self) {
        self.spill();
        self.flags = Flags::Frame;
    }

    /// Put x86 CF into ARM C (`direct`) or its inverse, using `temp`.
    fn carry_in(&mut self, direct: bool, temp: Reg) {
        match self.flags {
            Flags::Cpsr(current) if current == direct => return,
            Flags::Synced if !direct => return,
            Flags::Cpsr(_) => self.a.mrs(temp),
            _ => self.a.ldr(temp, FRAME, NZCV),
        }
        let source_direct = self.flags == Flags::Cpsr(true);
        if source_direct != direct {
            self.a.dp(Dp::Eor, false, temp, temp, Operand::Imm(C_BIT));
        }
        self.a.msr_flags(temp);
        self.flags = Flags::Cpsr(direct);
    }

    /// Prepare the CPSR for x86 condition `cc` and return its ARM condition.
    /// Uses R12; parity conditions also clobber guest flags in the CPSR.
    fn condition(&mut self, cc: u8) -> Cond {
        if cc >> 1 == 5 {
            self.clobber();
            let a = &mut self.a;
            a.ldr(R12, FRAME, RESULT);
            a.dp(Dp::And, false, R12, R12, Operand::Imm(0xff));
            for shift in [4, 2, 1] {
                a.dp(
                    Dp::Eor,
                    false,
                    R12,
                    R12,
                    Operand::Reg(R12, Shift::Lsr(shift)),
                );
            }
            a.tst(R12, Operand::Imm(1));
            // Bit 0 is clear for even parity, which sets PF.
            return if cc & 1 == 0 { Cond::Eq } else { Cond::Ne };
        }
        let carry = matches!(cc >> 1, 1 | 3);
        match self.flags {
            Flags::Frame => {
                self.a.ldr(R12, FRAME, NZCV);
                self.a.msr_flags(R12);
                self.flags = Flags::Synced;
            }
            Flags::CpsrNzv if carry => {
                // spill() needs R3; no caller holds a value there here.
                self.spill();
            }
            Flags::Cpsr(true) if cc >> 1 == 3 => {
                self.a.mrs(R12);
                self.a.dp(Dp::Eor, false, R12, R12, Operand::Imm(C_BIT));
                self.a.msr_flags(R12);
                self.flags = Flags::Cpsr(false);
            }
            _ => {}
        }
        let direct = self.flags == Flags::Cpsr(true);
        match cc {
            0 => Cond::Vs,
            1 => Cond::Vc,
            2 if direct => Cond::Cs,
            2 => Cond::Cc,
            3 if direct => Cond::Cc,
            3 => Cond::Cs,
            4 => Cond::Eq,
            5 => Cond::Ne,
            6 => Cond::Ls,
            7 => Cond::Hi,
            8 => Cond::Mi,
            9 => Cond::Pl,
            12 => Cond::Lt,
            13 => Cond::Ge,
            14 => Cond::Le,
            15 => Cond::Gt,
            _ => unreachable!(),
        }
    }

    /// Store PF/AF sources when a later observer needs them.
    fn parity(&mut self, needed: u32, result: Reg, auxiliary: Reg) {
        if needed & PF_AF != 0 {
            self.a.str(result, FRAME, RESULT);
            self.a.str(auxiliary, FRAME, AUXILIARY);
        }
    }

    /// Store a manually computed Frame.nzcv from R12.
    fn store_flags(&mut self) {
        self.a.str(R12, FRAME, NZCV);
        self.flags = Flags::Frame;
    }

    // ----- Operands -----

    fn low(&mut self, src: Src, scratch: Reg) -> Reg {
        match src {
            Src::Reg(reg) => reg,
            Src::High(reg) => {
                self.a.mov(scratch, Operand::Reg(reg, Shift::Lsr(8)));
                scratch
            }
            Src::Imm(value) => {
                self.a.mov_imm(scratch, value);
                scratch
            }
        }
    }

    /// The operand shifted to bits 31..32-bits, with zero low bits.
    fn top(&mut self, src: Src, bits: u8, scratch: Reg) -> Reg {
        let shift = 32 - bits;
        match src {
            Src::Reg(reg) if bits == 32 => reg,
            Src::Reg(reg) => {
                self.a.mov(scratch, Operand::Reg(reg, Shift::Lsl(shift)));
                scratch
            }
            Src::High(reg) => {
                self.a.mov(scratch, Operand::Reg(reg, Shift::Lsr(8)));
                self.a.mov(scratch, Operand::Reg(scratch, Shift::Lsl(24)));
                scratch
            }
            Src::Imm(value) => {
                self.a.mov_imm(scratch, (value & mask(bits)) << shift);
                scratch
            }
        }
    }

    fn top_operand(&mut self, src: Src, bits: u8, scratch: Reg) -> Operand {
        let shift = 32 - bits;
        match src {
            Src::Reg(reg) if bits == 32 => reg.into(),
            Src::Reg(reg) => Operand::Reg(reg, Shift::Lsl(shift)),
            Src::High(reg) => {
                self.a.mov(scratch, Operand::Reg(reg, Shift::Lsr(8)));
                Operand::Reg(scratch, Shift::Lsl(24))
            }
            Src::Imm(value) => {
                let value = (value & mask(bits)) << shift;
                if encode_imm(value).is_some() {
                    Operand::Imm(value)
                } else {
                    self.a.mov_imm(scratch, value);
                    scratch.into()
                }
            }
        }
    }

    /// Write a value held in the low bits of `value` (other bits ignored).
    fn write_low(&mut self, dst: Register, value: Reg) {
        let target = guest(dst.index);
        let a = &mut self.a;
        match (dst.bits, dst.shift) {
            (32, _) => {
                if value != target {
                    a.mov(target, value);
                }
            }
            (16, _) => a.pkhbt(target, value, target),
            (8, 0) if value != target => {
                a.dp(Dp::Bic, false, target, target, Operand::Imm(0xff));
                a.uxtab(target, target, value);
            }
            (8, shift) => {
                a.dp(Dp::And, false, R12, value, Operand::Imm(0xff));
                a.dp(Dp::Bic, false, target, target, Operand::Imm(0xff << shift));
                a.dp(
                    Dp::Orr,
                    false,
                    target,
                    target,
                    Operand::Reg(R12, Shift::Lsl(shift)),
                );
            }
            _ => unreachable!(),
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
            (16, _) => a.pkhtb16(target, target, value),
            (8, shift) => {
                a.dp(Dp::Bic, false, target, target, Operand::Imm(0xff << shift));
                a.dp(
                    Dp::Orr,
                    false,
                    target,
                    target,
                    Operand::Reg(value, Shift::Lsr(24 - shift)),
                );
            }
            _ => unreachable!(),
        }
    }

    // ----- Memory -----

    /// The precise exit before instruction `index`, shared by its guards.
    fn miss(&mut self, index: usize) -> Label {
        let code = index as u32 + 1;
        if let Some(&(label, _)) = self.misses.last().filter(|(_, c)| *c == code) {
            return label;
        }
        let label = self.a.label();
        self.misses.push((label, code));
        label
    }

    /// The x86 offset, in `dst` unless it is a bare guest register. Uses R12.
    fn offset(&mut self, address: Address, dst: Reg) -> Reg {
        let scale = Shift::Lsl(address.scale);
        let base = match (address.base, address.index) {
            (Some(base), Some(index)) => {
                self.a.dp(
                    Dp::Add,
                    false,
                    dst,
                    guest(base),
                    Operand::Reg(guest(index), scale),
                );
                dst
            }
            (Some(base), None) => guest(base),
            (None, Some(index)) => {
                self.a.mov(dst, Operand::Reg(guest(index), scale));
                dst
            }
            (None, None) => {
                self.a.mov_imm(dst, address.displacement);
                return dst;
            }
        };
        if address.displacement == 0 {
            return base;
        }
        self.a.add_imm(dst, base, address.displacement, R12);
        dst
    }

    /// Linear address in `dst` (R0 or R1) or a bare guest register. FS/GS
    /// check permission and offset + width against the limit before adding
    /// the base, without 32-bit offset wrap. Uses R2 and R12.
    fn linear(&mut self, address: Address, bytes: u32, write: bool, miss: Label, dst: Reg) -> Reg {
        let offset = self.offset(address, dst);
        let index = match address.segment {
            SegmentReg::Fs => 0,
            SegmentReg::Gs => 1,
            _ => return offset,
        };
        let a = &mut self.a;
        a.ldr(R12, FRAME, BASED);
        a.tst(R12, Operand::Imm(1 << (index + if write { 2 } else { 0 })));
        a.b_cond(Cond::Eq, miss);
        let end = if bytes > 1 {
            a.dp(Dp::Add, true, R2, offset, Operand::Imm(bytes - 1));
            a.b_cond(Cond::Cs, miss);
            R2
        } else {
            offset
        };
        a.ldr(R12, FRAME, LIMITS + index * 4);
        a.cmp(end, R12);
        a.b_cond(Cond::Hi, miss);
        a.ldr(R12, FRAME, BASES + index * 4);
        a.dp(Dp::Add, false, dst, offset, R12);
        dst
    }

    /// Software TLB hit for every byte; leaves the host bias in R3. A split
    /// page or 32-bit wrap selects another slot, whose tag cannot match.
    fn guard(&mut self, linear: Reg, bytes: u32, write: bool, miss: Label) {
        assert!(![R2, R3, R12].contains(&linear));
        let bits = LINEAR_TRANSLATION_CACHE_BITS;
        let a = &mut self.a;
        // Row = table + (page mod slots) * 16; the shifts drop offset bits.
        a.ldrd(R2, FRAME, TLB_STATE as u32);
        a.mov(R12, Operand::Reg(linear, Shift::Lsr(12)));
        a.mov(R12, Operand::Reg(R12, Shift::Lsl(32 - bits)));
        a.dp(
            Dp::Add,
            false,
            R3,
            R3,
            Operand::Reg(R12, Shift::Lsr(28 - bits)),
        );
        a.ldr(R12, R3, if write { WRITE_TAG } else { TAG });
        a.ldr(R3, R3, 0);
        a.dp(Dp::Eor, false, R12, R12, R2);
        if bytes > 1 {
            a.dp(Dp::Add, false, R2, linear, Operand::Imm(bytes - 1));
            a.mov(R2, Operand::Reg(R2, Shift::Lsr(12)));
        } else {
            a.mov(R2, Operand::Reg(linear, Shift::Lsr(12)));
        }
        a.cmp(R12, Operand::Reg(R2, Shift::Lsl(12)));
        a.b_cond(Cond::Ne, miss);
    }

    /// Guarded access preparation. The caller has spilled flags.
    fn access(&mut self, address: Address, bits: u8, write: bool, miss: Label, dst: Reg) -> Reg {
        let linear = self.linear(address, u32::from(bits / 8), write, miss, dst);
        self.guard(linear, u32::from(bits / 8), write, miss);
        linear
    }

    /// Load a memory operand into R1 (zero-extended), using R0 for its address.
    fn load(&mut self, value: Value, index: usize) -> Src {
        match value {
            Value::Memory { address, bits } => {
                self.clobber();
                let miss = self.miss(index);
                let linear = self.access(address, bits, false, miss, R0);
                self.a.load_indexed(bits, false, R1, R3, linear);
                Src::Reg(R1)
            }
            Value::Register(reg) => register(reg),
            Value::Immediate(value) => Src::Imm(value),
        }
    }

    // ----- Arithmetic -----

    /// x86 ALU semantics. Writes `dst` unless the kind only compares.
    #[allow(clippy::too_many_arguments)]
    fn alu(
        &mut self,
        kind: Alu,
        bits: u8,
        lhs: Src,
        rhs: Src,
        needed: u32,
        dst: Option<Register>,
        keep_carry: bool,
    ) {
        let compare = matches!(kind, Alu::Cmp | Alu::Test);
        let logic = matches!(kind, Alu::And | Alu::Or | Alu::Xor | Alu::Test);
        if compare && needed == 0 {
            return;
        }
        if keep_carry && needed != 0 && matches!(self.flags, Flags::Cpsr(_)) {
            self.spill();
        }
        let op = match kind {
            Alu::Add => Dp::Add,
            Alu::Adc => Dp::Adc,
            Alu::Sbb => Dp::Sbc,
            Alu::Sub | Alu::Cmp => Dp::Sub,
            Alu::And | Alu::Test => Dp::And,
            Alu::Or => Dp::Orr,
            Alu::Xor => Dp::Eor,
        };
        // Dead flags leave the CPSR, which may hold earlier live flags, alone.
        let set = !logic && needed != 0;
        if bits == 32 {
            let left = self.low(lhs, R0);
            let right = match rhs {
                Src::Reg(reg) => reg.into(),
                Src::Imm(value) if encode_imm(value).is_some() => Operand::Imm(value),
                Src::Imm(value) => {
                    self.a.mov_imm(R12, value);
                    R12.into()
                }
                Src::High(_) => unreachable!(),
            };
            if matches!(kind, Alu::Adc | Alu::Sbb) {
                self.carry_in(kind == Alu::Adc, R2);
            }
            if needed & PF_AF != 0 && !logic {
                let auxiliary = match rhs {
                    Src::Imm(value) => Operand::Imm(value & 0xff),
                    _ => right,
                };
                self.a.dp(Dp::Eor, false, R2, left, auxiliary);
            }
            let result = match dst {
                Some(dst) if !compare => guest(dst.index),
                _ => R0,
            };
            self.a.dp(op, set, result, left, right);
            if logic && needed != 0 {
                self.a.cmp(result, Operand::Imm(0));
            }
            self.parity(needed, result, if logic { result } else { R2 });
        } else {
            let shift = 32 - bits;
            let left = self.top(lhs, bits, R0);
            let right = self.top_operand(rhs, bits, R12);
            let auxiliary = needed & PF_AF != 0 && !logic;
            if auxiliary {
                self.a.dp(Dp::Eor, false, R2, left, right);
            }
            self.a.dp(op, set, R0, left, right);
            if logic && needed != 0 {
                self.a.cmp(R0, Operand::Imm(0));
            }
            if needed & PF_AF != 0 {
                self.a.mov(R12, Operand::Reg(R0, Shift::Lsr(shift)));
            }
            if auxiliary {
                self.a.mov(R2, Operand::Reg(R2, Shift::Lsr(shift)));
            }
            self.parity(needed, R12, if logic { R12 } else { R2 });
            if let Some(dst) = dst.filter(|_| !compare) {
                self.write_top(dst, R0);
            }
        }
        if needed != 0 || set {
            self.flags = if keep_carry {
                Flags::CpsrNzv
            } else {
                Flags::Cpsr(matches!(kind, Alu::Add | Alu::Adc))
            };
        }
    }

    // ----- Instructions -----

    /// Lower one instruction. Returns true after an unconditional exit.
    fn instruction(&mut self, index: usize, op: Op, next: u32, needed: u32) -> bool {
        match op {
            Op::Nop => {}
            Op::Mov { dst, src } => self.mov(index, dst, src),
            Op::Store { address, bits, src } => {
                self.clobber();
                let miss = self.miss(index);
                let value = match src {
                    Value::Register(reg) => self.low(register(reg), R1),
                    Value::Immediate(value) => {
                        self.a.mov_imm(R1, value);
                        R1
                    }
                    Value::Memory { .. } => unreachable!(),
                };
                let linear = self.access(address, bits, true, miss, R0);
                self.a.store_indexed(bits, value, R3, linear);
            }
            Op::Alu { kind, dst, src } => {
                let rhs = self.load(src, index);
                self.alu(kind, dst.bits, register(dst), rhs, needed, Some(dst), false);
            }
            Op::Compare {
                lhs,
                rhs,
                bits,
                test,
            } => {
                let left = self.load(lhs, index);
                let right = match rhs {
                    Value::Register(reg) => register(reg),
                    Value::Immediate(value) => Src::Imm(value),
                    Value::Memory { .. } => unreachable!(),
                };
                let kind = if test { Alu::Test } else { Alu::Cmp };
                self.alu(kind, bits, left, right, needed, None, false);
            }
            Op::MemoryAlu {
                kind,
                address,
                bits,
                src,
                preserve_carry,
            } => {
                self.clobber();
                let miss = self.miss(index);
                let rhs = match src {
                    Value::Register(reg) => register(reg),
                    Value::Immediate(value) => Src::Imm(value),
                    Value::Memory { .. } => unreachable!(),
                };
                let linear = self.access(address, bits, true, miss, R0);
                self.a.load_indexed(bits, false, R1, R3, linear);
                // Keep the address in R0/guest and bias in R3 for the store.
                let rhs = match rhs {
                    Src::High(reg) => {
                        self.a.mov(R2, Operand::Reg(reg, Shift::Lsr(8)));
                        Src::Reg(R2)
                    }
                    rhs => rhs,
                };
                self.memory_alu(kind, bits, rhs, needed, linear, preserve_carry);
            }
            Op::IncDec { dst, decrement } => {
                let kind = if decrement { Alu::Sub } else { Alu::Add };
                self.alu(
                    kind,
                    dst.bits,
                    register(dst),
                    Src::Imm(1),
                    needed,
                    Some(dst),
                    true,
                );
            }
            Op::Not { dst } => {
                let target = guest(dst.index);
                match (dst.bits, dst.shift) {
                    (32, _) => self.a.mvn(target, target),
                    (16, _) => {
                        self.a.mvn(R0, target);
                        self.a.pkhbt(target, R0, target);
                    }
                    (_, shift) => {
                        self.a
                            .dp(Dp::Eor, false, target, target, Operand::Imm(0xff << shift))
                    }
                }
            }
            Op::Negate { dst } => self.negate(dst, needed),
            Op::Lea { dst, address } => {
                if dst.bits == 32 {
                    let target = guest(dst.index);
                    let value = self.offset(address, target);
                    if value != target {
                        self.a.mov(target, value);
                    }
                } else {
                    let value = self.offset(address, R0);
                    self.write_low(dst, value);
                }
            }
            Op::Extend {
                dst,
                src,
                source_bits,
                signed,
            } => {
                let target = if dst.bits == 32 { guest(dst.index) } else { R0 };
                match src {
                    Value::Register(reg) => {
                        self.a
                            .extend(source_bits, signed, target, guest(reg.index), reg.shift)
                    }
                    Value::Memory { address, .. } => {
                        self.clobber();
                        let miss = self.miss(index);
                        let linear = self.access(address, source_bits, false, miss, R0);
                        self.a.load_indexed(source_bits, signed, target, R3, linear);
                    }
                    Value::Immediate(_) => unreachable!(),
                }
                self.write_low(dst, target);
            }
            Op::SignExtendPair { bits } => {
                if bits == 32 {
                    self.a.mov(guest(2), Operand::Reg(guest(0), Shift::Asr(31)));
                } else {
                    self.a.mov(R0, Operand::Reg(guest(0), Shift::Lsl(16)));
                    self.a.mov(R0, Operand::Reg(R0, Shift::Asr(31)));
                    self.a.pkhbt(guest(2), R0, guest(2));
                }
            }
            Op::ByteSwap { dst } => self.a.rev(guest(dst.index), guest(dst.index)),
            Op::Shift { dst, kind, count } => self.shift(index, dst, kind, count, needed),
            Op::DoubleShift {
                dst,
                src,
                right,
                count,
            } => self.double_shift(dst, src, right, count, needed),
            Op::Imul { dst, lhs, rhs } => {
                if needed != 0 {
                    self.clobber();
                }
                let (left, right) = if rhs.is_memory() {
                    let right = self.load(rhs, index);
                    (self.load(lhs, index), right)
                } else {
                    let left = self.load(lhs, index);
                    (left, self.load(rhs, index))
                };
                let left = self.low(left, R0);
                let right = self.low(right, R12);
                self.a.mull(true, R2, R3, left, right);
                if needed != 0 {
                    self.a.cmp(R3, Operand::Reg(R2, Shift::Asr(31)));
                    self.overflow_flags();
                }
                self.a.mov(guest(dst.index), R2);
            }
            Op::Multiply { src, signed, .. } => {
                if needed != 0 {
                    self.clobber();
                }
                let value = self.load(src, index);
                let value = self.low(value, R1);
                self.a.mull(signed, R2, R3, guest(0), value);
                if needed != 0 {
                    if signed {
                        self.a.cmp(R3, Operand::Reg(R2, Shift::Asr(31)));
                    } else {
                        self.a.cmp(R3, Operand::Imm(0));
                    }
                    self.overflow_flags();
                }
                self.a.mov(guest(0), R2);
                self.a.mov(guest(2), R3);
            }
            Op::Exchange { kind, dst, src } => self.exchange(index, kind, dst, src, needed),
            Op::ConditionalMove {
                dst,
                src,
                condition,
            } => {
                let value = self.load(src, index);
                let value = self.low(value, R1);
                let cond = self.condition(condition);
                self.a.mov_cond(cond, guest(dst.index), value);
            }
            Op::SetCondition { dst, condition } => match dst {
                Value::Register(dst) => {
                    let target = guest(dst.index);
                    let cond = self.condition(condition);
                    self.a.dp(
                        Dp::Bic,
                        false,
                        target,
                        target,
                        Operand::Imm(0xff << dst.shift),
                    );
                    self.a.dp_cond(
                        cond,
                        Dp::Orr,
                        false,
                        target,
                        target,
                        Operand::Imm(1 << dst.shift),
                    );
                }
                Value::Memory { address, .. } => {
                    self.clobber();
                    let miss = self.miss(index);
                    let linear = self.access(address, 8, true, miss, R0);
                    let cond = self.condition(condition);
                    self.a.mov(R1, Operand::Imm(0));
                    self.a.mov_cond(cond, R1, Operand::Imm(1));
                    self.a.store_indexed(8, R1, R3, linear);
                }
                Value::Immediate(_) => unreachable!(),
            },
            Op::BitTest {
                src,
                index: bit,
                bits,
                kind,
            } => self.bit_test(index, src, bit, bits, kind),
            Op::BitScan { dst, src, reverse } => {
                self.clobber();
                let value = self.load(src, index);
                let value = self.low(value, R1);
                let a = &mut self.a;
                a.ldr(R12, FRAME, NZCV);
                a.cmp(value, Operand::Imm(0));
                a.dp_cond(Cond::Eq, Dp::Orr, false, R12, R12, Operand::Imm(Z_BIT));
                a.dp_cond(Cond::Ne, Dp::Bic, false, R12, R12, Operand::Imm(Z_BIT));
                a.str(R12, FRAME, NZCV);
                let target = guest(dst.index);
                a.with_cond(Cond::Ne, |a| {
                    if reverse {
                        a.clz(R2, value);
                    } else {
                        a.dp(Dp::Rsb, false, R2, value, Operand::Imm(0));
                        a.dp(Dp::And, false, R2, R2, value);
                        a.clz(R2, R2);
                    }
                    a.dp(Dp::Rsb, false, target, R2, Operand::Imm(31));
                });
                self.flags = Flags::Frame;
            }
            Op::Carry { value } => {
                self.clobber();
                self.a.ldr(R12, FRAME, NZCV);
                let op = match value {
                    Some(false) => Dp::Orr,
                    Some(true) => Dp::Bic,
                    None => Dp::Eor,
                };
                self.a.dp(op, false, R12, R12, Operand::Imm(C_BIT));
                self.store_flags();
            }
            Op::Direction { backward } => {
                self.a.ldr(R12, FRAME, EFLAGS);
                let op = if backward { Dp::Orr } else { Dp::Bic };
                self.a.dp(op, false, R12, R12, Operand::Imm(1 << 10));
                self.a.str(R12, FRAME, EFLAGS);
            }
            Op::Movs { bits, segment, .. } => {
                // Both guards pass before the store; DF selects the step.
                self.clobber();
                let miss = self.miss(index);
                let string = |base, segment| Address {
                    base: Some(base),
                    index: None,
                    scale: 0,
                    displacement: 0,
                    segment,
                };
                let linear = self.access(string(6, segment), bits, false, miss, R0);
                self.a.load_indexed(bits, false, R1, R3, linear);
                let linear = self.access(string(7, SegmentReg::Es), bits, true, miss, R0);
                self.a.store_indexed(bits, R1, R3, linear);
                let step = Operand::Imm(u32::from(bits / 8));
                self.a.ldr(R12, FRAME, EFLAGS);
                self.a.tst(R12, Operand::Imm(1 << 10));
                for reg in [guest(6), guest(7)] {
                    self.a.dp_cond(Cond::Eq, Dp::Add, false, reg, reg, step);
                    self.a.dp_cond(Cond::Ne, Dp::Sub, false, reg, reg, step);
                }
            }
            Op::Push { src, .. } => {
                let value = match self.load(src, index) {
                    Src::Imm(value) => {
                        self.a.mov_imm(R1, value);
                        R1
                    }
                    Src::Reg(reg) => reg,
                    Src::High(_) => unreachable!(),
                };
                self.clobber();
                let miss = self.miss(index);
                self.push(value, miss);
            }
            Op::Pop { dst } => {
                self.clobber();
                let miss = self.miss(index);
                self.guard(ESP, 4, false, miss);
                self.a.load_indexed(32, false, R0, R3, ESP);
                self.a.dp(Dp::Add, false, ESP, ESP, Operand::Imm(4));
                self.a.mov(guest(dst.index), R0);
            }
            Op::Leave { .. } => {
                self.clobber();
                let miss = self.miss(index);
                self.guard(EBP, 4, false, miss);
                self.a.load_indexed(32, false, R0, R3, EBP);
                self.a.dp(Dp::Add, false, ESP, EBP, Operand::Imm(4));
                self.a.mov(EBP, R0);
            }
            Op::Call { target, .. } => {
                // The target is read before the push. Frame.eip is only
                // consumed by a completed block, so storing it early is safe.
                let destination = match self.load(target, index) {
                    Src::Imm(value) => Some(value),
                    Src::Reg(reg) => {
                        self.a.str(reg, FRAME, EIP);
                        None
                    }
                    Src::High(_) => unreachable!(),
                };
                self.clobber();
                let miss = self.miss(index);
                self.a.mov_imm(R1, next);
                self.push(R1, miss);
                match destination {
                    Some(value) => self.exit_to(value),
                    None => self.exit(),
                }
                return true;
            }
            Op::Return { pop, .. } => {
                self.clobber();
                let miss = self.miss(index);
                self.guard(ESP, 4, false, miss);
                self.a.load_indexed(32, false, R0, R3, ESP);
                self.a.add_imm(ESP, ESP, 4 + u32::from(pop), R12);
                self.exit_with(R0);
                return true;
            }
            Op::IndirectJump { src } => {
                let value = self.load(src, index);
                let value = self.low(value, R0);
                self.exit_with(value);
                return true;
            }
            Op::Branch { condition, target } => {
                let Some(condition) = condition else {
                    self.exit_to(target);
                    return true;
                };
                self.spill();
                let cond = match condition {
                    BranchCondition::Flags(cc) => self.condition(cc),
                    BranchCondition::EcxZero => {
                        self.clobber();
                        self.a.cmp(guest(1), Operand::Imm(0));
                        Cond::Eq
                    }
                };
                let taken = self.a.label();
                self.a.b_cond(cond, taken);
                let flags = self.flags;
                self.exit_to(next);
                self.flags = flags;
                self.a.bind(taken);
                self.exit_to(target);
                return true;
            }
            _ => unreachable!("unsupported instruction reached lowering"),
        }
        false
    }

    fn mov(&mut self, index: usize, dst: Register, src: Value) {
        let target = guest(dst.index);
        match src {
            Value::Register(src) => {
                if src == dst {
                    return;
                }
                if dst.bits == 32 {
                    self.a.mov(target, guest(src.index));
                } else {
                    let value = self.low(register(src), R0);
                    self.write_low(dst, value);
                }
            }
            Value::Immediate(value) => {
                if dst.bits == 32 {
                    self.a.mov_imm(target, value);
                } else {
                    self.a.mov_imm(R0, value & mask(dst.bits));
                    self.write_low(dst, R0);
                }
            }
            Value::Memory { address, bits } => {
                self.clobber();
                let miss = self.miss(index);
                let linear = self.access(address, bits, false, miss, R0);
                if bits == 32 {
                    self.a.load_indexed(32, false, target, R3, linear);
                } else {
                    self.a.load_indexed(bits, false, R1, R3, linear);
                    self.write_low(dst, R1);
                }
            }
        }
    }

    /// Read-modify-write after the write guard: old value in R1, address in
    /// `linear` (R0 or a guest register), bias in R3. `rhs` is a guest
    /// register, R2 or an immediate. The instruction already spilled flags.
    fn memory_alu(
        &mut self,
        kind: Alu,
        bits: u8,
        rhs: Src,
        needed: u32,
        linear: Reg,
        keep_carry: bool,
    ) {
        let shift = 32 - bits;
        let logic = matches!(kind, Alu::And | Alu::Or | Alu::Xor);
        let op = match kind {
            Alu::Add => Dp::Add,
            Alu::Adc => Dp::Adc,
            Alu::Sbb => Dp::Sbc,
            Alu::Sub => Dp::Sub,
            Alu::And => Dp::And,
            Alu::Or => Dp::Orr,
            Alu::Xor => Dp::Eor,
            Alu::Cmp | Alu::Test => unreachable!(),
        };
        let set = !logic && needed != 0;
        if shift != 0 {
            self.a.mov(R1, Operand::Reg(R1, Shift::Lsl(shift)));
        }
        let right = match rhs {
            Src::Reg(reg) => Operand::Reg(reg, Shift::Lsl(shift)),
            Src::Imm(value) => {
                let value = (value & mask(bits)) << shift;
                if encode_imm(value).is_some() {
                    Operand::Imm(value)
                } else {
                    self.a.mov_imm(R12, value);
                    R12.into()
                }
            }
            Src::High(_) => unreachable!(),
        };
        let uses = |reg: Reg| match right {
            Operand::Reg(used, _) => used == reg,
            _ => false,
        };
        let temp = [R2, R12, R0]
            .into_iter()
            .find(|&reg| reg != linear && !uses(reg))
            .unwrap();
        if matches!(kind, Alu::Adc | Alu::Sbb) {
            self.carry_in(kind == Alu::Adc, temp);
        }
        if needed & PF_AF != 0 && !logic {
            // AF needs old ^ source before the result replaces the old value.
            self.a.dp(Dp::Eor, false, temp, R1, right);
            if shift != 0 {
                self.a.mov(temp, Operand::Reg(temp, Shift::Lsr(shift)));
            }
            self.a.str(temp, FRAME, AUXILIARY);
        }
        self.a.dp(op, set, R1, R1, right);
        if logic && needed != 0 {
            self.a.cmp(R1, Operand::Imm(0));
        }
        if shift != 0 {
            self.a.mov(R1, Operand::Reg(R1, Shift::Lsr(shift)));
        }
        if needed & PF_AF != 0 {
            self.a.str(R1, FRAME, RESULT);
            if logic {
                self.a.str(R1, FRAME, AUXILIARY);
            }
        }
        self.a.store_indexed(bits, R1, R3, linear);
        if needed != 0 || set {
            self.flags = if keep_carry {
                Flags::CpsrNzv
            } else {
                Flags::Cpsr(matches!(kind, Alu::Add | Alu::Adc))
            };
        }
    }

    fn negate(&mut self, dst: Register, needed: u32) {
        let set = needed != 0;
        if dst.bits == 32 {
            let target = guest(dst.index);
            if needed & PF_AF != 0 {
                self.a.str(target, FRAME, AUXILIARY);
            }
            self.a.dp(Dp::Rsb, set, target, target, Operand::Imm(0));
            if needed & PF_AF != 0 {
                self.a.str(target, FRAME, RESULT);
            }
        } else {
            let shift = 32 - dst.bits;
            let value = self.top(register(dst), dst.bits, R1);
            if needed & PF_AF != 0 {
                self.a.mov(R2, Operand::Reg(value, Shift::Lsr(shift)));
                self.a.str(R2, FRAME, AUXILIARY);
            }
            self.a.dp(Dp::Rsb, set, R0, value, Operand::Imm(0));
            if needed & PF_AF != 0 {
                self.a.mov(R2, Operand::Reg(R0, Shift::Lsr(shift)));
                self.a.str(R2, FRAME, RESULT);
            }
            self.write_top(dst, R0);
        }
        if set {
            self.flags = Flags::Cpsr(false);
        }
    }

    /// SHL/SHR/SAR of any operand and ROR/ROL of a 32-bit register, by an
    /// immediate or CL. A masked zero count changes neither the value nor any
    /// flag. Rotates write only CF and OF. A narrow SHL works top-aligned and
    /// SHR/SAR on the zero/sign-extended value, so ARM N/Z/C are x86 SF/ZF/CF
    /// for every count up to 31.
    fn shift(&mut self, index: usize, dst: Value, kind: u8, count: Value, needed: u32) {
        let amount = match count {
            Value::Immediate(0) => return,
            Value::Immediate(count) => Some(count as u8),
            _ => None,
        };
        let rotate = matches!(kind, 3 | 4);
        let (bits, memory) = match dst {
            Value::Register(dst) => (dst.bits, false),
            Value::Memory { bits, .. } => (bits, true),
            Value::Immediate(_) => unreachable!(),
        };
        let top = kind == 0 && bits < 32;
        if needed != 0 || memory {
            self.clobber();
        }
        // The operand: a 32-bit register in place, otherwise R1. A memory
        // operand keeps its address in `linear` and the bias in R3.
        let mut linear = R0;
        let value = match dst {
            Value::Register(dst) if bits == 32 => guest(dst.index),
            Value::Register(dst) => {
                let source = guest(dst.index);
                if top {
                    let value = self.top(register(dst), bits, R1);
                    if value != R1 {
                        self.a.mov(R1, value);
                    }
                } else {
                    self.a.extend(bits, kind == 2, R1, source, dst.shift);
                }
                R1
            }
            Value::Memory { address, .. } => {
                let miss = self.miss(index);
                linear = self.access(address, bits, true, miss, R0);
                self.a.load_indexed(bits, kind == 2, R1, R3, linear);
                if top {
                    self.a.mov(R1, Operand::Reg(R1, Shift::Lsl(32 - bits)));
                }
                R1
            }
            Value::Immediate(_) => unreachable!(),
        };
        let shift = |count: u8| match kind {
            0 => Shift::Lsl(count),
            1 => Shift::Lsr(count),
            2 => Shift::Asr(count),
            3 => Shift::Ror(count),
            _ => Shift::Ror(32 - count),
        };
        // A register count: CL & 31 in R2; ROL becomes ROR by 32 - count.
        let operand = match amount {
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
        let skip = self.a.label();
        let a = &mut self.a;
        if needed == 0 {
            // ARM shifts and rotates by zero leave the value unchanged.
            a.mov(value, operand);
        } else {
            if amount.is_none() {
                a.cmp(R2, Operand::Imm(0));
                a.b_cond(Cond::Eq, skip);
            }
            if rotate {
                a.mov(value, operand);
                a.ldr(R12, FRAME, NZCV);
                a.dp(Dp::Bic, false, R12, R12, Operand::Imm(C_BIT | V_BIT));
                // ROL: CF = bit 0, OF = bit 31 ^ CF. ROR: CF = bit 31, OF = bit 31 ^ bit 30.
                let (carry, other) = if kind == 4 {
                    (1, Shift::Lsl(31))
                } else {
                    (N_BIT, Shift::Lsl(1))
                };
                a.tst(value, Operand::Imm(carry));
                a.dp_cond(Cond::Eq, Dp::Orr, false, R12, R12, Operand::Imm(C_BIT));
                a.dp(Dp::Eor, false, R2, value, Operand::Reg(value, other));
                a.dp(Dp::And, false, R2, R2, Operand::Imm(N_BIT));
                a.dp(Dp::Orr, false, R12, R12, Operand::Reg(R2, Shift::Lsr(3)));
                a.str(R12, FRAME, NZCV);
            } else {
                a.movs(value, operand);
                // N, Z and C (= x86 CF) are set; derive OF and normalize C.
                a.mrs(R12);
                a.dp(Dp::Bic, false, R12, R12, Operand::Imm(V_BIT));
                // SHR by one: OF is the old sign, now bit bits - 2.
                let sign = 1 << (bits - 2);
                let to_v = if bits == 32 {
                    Shift::Lsr(2)
                } else {
                    Shift::Lsl(30 - bits)
                };
                match (kind, amount) {
                    (0, _) => {
                        // OF = CF ^ SF: bit 31 of nzcv ^ (nzcv << 2).
                        a.dp(Dp::Eor, false, R2, R12, Operand::Reg(R12, Shift::Lsl(2)));
                        a.dp(Dp::And, false, R2, R2, Operand::Imm(N_BIT));
                        a.dp(Dp::Orr, false, R12, R12, Operand::Reg(R2, Shift::Lsr(3)));
                    }
                    (1, Some(1)) => {
                        a.dp(Dp::And, false, R2, value, Operand::Imm(sign));
                        a.dp(Dp::Orr, false, R12, R12, Operand::Reg(R2, to_v));
                    }
                    (1, None) => {
                        a.cmp(R2, Operand::Imm(1));
                        a.dp_cond(Cond::Eq, Dp::And, false, R2, value, Operand::Imm(sign));
                        a.dp_cond(Cond::Eq, Dp::Orr, false, R12, R12, Operand::Reg(R2, to_v));
                    }
                    _ => {}
                }
                a.dp(Dp::Eor, false, R12, R12, Operand::Imm(C_BIT));
                a.str(R12, FRAME, NZCV);
                if needed & PF_AF != 0 {
                    let result = if top {
                        a.mov(R2, Operand::Reg(value, Shift::Lsr(32 - bits)));
                        R2
                    } else {
                        value
                    };
                    a.str(result, FRAME, RESULT);
                    a.str(result, FRAME, AUXILIARY);
                }
            }
        }
        match dst {
            Value::Register(dst) if bits < 32 => {
                if top {
                    self.write_top(dst, R1);
                } else {
                    self.write_low(dst, R1);
                }
            }
            Value::Memory { .. } => {
                if top {
                    self.a.mov(R1, Operand::Reg(R1, Shift::Lsr(32 - bits)));
                }
                self.a.store_indexed(bits, R1, R3, linear);
            }
            _ => {}
        }
        self.a.bind(skip);
        if needed != 0 {
            self.flags = Flags::Frame;
        }
    }

    /// SHLD/SHRD of 32-bit registers by an immediate or CL. A masked zero
    /// count changes nothing; OF changes only for a count of one, AF never.
    fn double_shift(
        &mut self,
        dst: Register,
        src: Register,
        right: bool,
        count: Value,
        needed: u32,
    ) {
        let target = guest(dst.index);
        let source = guest(src.index);
        let amount = match count {
            Value::Immediate(0) => return,
            Value::Immediate(count) => Some(count as u8),
            _ => None,
        };
        if needed != 0 {
            self.clobber();
        }
        let skip = self.a.label();
        let a = &mut self.a;
        // R3 = the source bits entering the vacated end.
        let shifted = match amount {
            Some(count) => {
                let (out, fill) = if right {
                    (Shift::Lsr(count), Shift::Lsl(32 - count))
                } else {
                    (Shift::Lsl(count), Shift::Lsr(32 - count))
                };
                a.mov(R3, Operand::Reg(source, fill));
                Operand::Reg(target, out)
            }
            None => {
                a.dp(Dp::And, needed != 0, R0, guest(1), Operand::Imm(31));
                if needed != 0 {
                    a.b_cond(Cond::Eq, skip);
                }
                // A zero count shifts the source by 32, which ARM makes zero.
                a.dp(Dp::Rsb, false, R2, R0, Operand::Imm(32));
                let (out, fill) = if right { (1, 0) } else { (0, 1) };
                a.mov(R3, Operand::RegShift(source, fill, R2));
                Operand::RegShift(target, out, R0)
            }
        };
        if needed == 0 {
            a.dp(Dp::Orr, false, target, R3, shifted);
            return;
        }
        // C is the last bit shifted out; ORRS keeps it and sets N and Z.
        a.movs(R1, shifted);
        a.dp(Dp::Orr, true, R1, R1, R3);
        a.mrs(R12);
        a.dp(Dp::Bic, false, R12, R12, Operand::Imm(V_BIT));
        a.dp(Dp::Eor, false, R12, R12, Operand::Imm(C_BIT));
        match amount {
            Some(1) => {
                a.dp(Dp::Eor, false, R2, target, R1);
                a.mov(R2, Operand::Reg(R2, Shift::Lsr(3)));
            }
            Some(_) => a.ldr(R2, FRAME, NZCV),
            None => {
                a.ldr(R2, FRAME, NZCV);
                a.cmp(R0, Operand::Imm(1));
                a.dp_cond(Cond::Eq, Dp::Eor, false, R2, target, R1);
                a.mov_cond(Cond::Eq, R2, Operand::Reg(R2, Shift::Lsr(3)));
            }
        }
        a.dp(Dp::And, false, R2, R2, Operand::Imm(V_BIT));
        a.dp(Dp::Orr, false, R12, R12, R2);
        a.str(R12, FRAME, NZCV);
        a.mov(target, R1);
        if needed & 4 != 0 {
            // The new result gives PF; keep AF = bit 4 of result ^ auxiliary.
            a.ldr(R2, FRAME, RESULT);
            a.ldr(R3, FRAME, AUXILIARY);
            a.dp(Dp::Eor, false, R2, R2, R3);
            a.dp(Dp::And, false, R2, R2, Operand::Imm(0x10));
            a.dp(Dp::Eor, false, R2, R2, R1);
            a.str(R1, FRAME, RESULT);
            a.str(R2, FRAME, AUXILIARY);
        }
        a.bind(skip);
        self.flags = Flags::Frame;
    }

    /// After CMP hi, sign/zero: EQ means no overflow. Sets only CF/OF.
    fn overflow_flags(&mut self) {
        let a = &mut self.a;
        a.ldr(R12, FRAME, NZCV);
        a.dp(Dp::Bic, false, R12, R12, Operand::Imm(C_BIT | V_BIT));
        a.dp_cond(Cond::Eq, Dp::Orr, false, R12, R12, Operand::Imm(C_BIT));
        a.dp_cond(Cond::Ne, Dp::Orr, false, R12, R12, Operand::Imm(V_BIT));
        self.store_flags();
    }

    fn exchange(&mut self, index: usize, kind: ExchangeOp, dst: Value, src: Register, needed: u32) {
        let source = guest(src.index);
        match (kind, dst) {
            (ExchangeOp::Swap, Value::Register(dst)) => {
                let target = guest(dst.index);
                if target != source {
                    self.a.mov(R0, target);
                    self.a.mov(target, source);
                    self.a.mov(source, R0);
                }
            }
            (ExchangeOp::Swap, Value::Memory { address, .. }) => {
                self.clobber();
                let miss = self.miss(index);
                let linear = self.access(address, 32, true, miss, R0);
                self.a.load_indexed(32, false, R1, R3, linear);
                self.a.store_indexed(32, source, R3, linear);
                self.a.mov(source, R1);
            }
            (ExchangeOp::Add, Value::Register(dst)) => {
                let target = guest(dst.index);
                if needed & PF_AF != 0 {
                    self.a.dp(Dp::Eor, false, R2, target, source);
                }
                self.a.mov(R1, target);
                self.a.dp(Dp::Add, true, R0, target, source);
                self.parity(needed, R0, R2);
                // The source receives the old value first, so aliases see the sum.
                self.a.mov(source, R1);
                self.a.mov(target, R0);
                self.flags = Flags::Cpsr(true);
            }
            (ExchangeOp::Add, Value::Memory { address, .. }) => {
                self.clobber();
                let miss = self.miss(index);
                let linear = self.access(address, 32, true, miss, R0);
                self.a.load_indexed(32, false, R1, R3, linear);
                if needed & PF_AF != 0 {
                    self.a.dp(Dp::Eor, false, R2, R1, source);
                }
                self.a.dp(Dp::Add, true, R12, R1, source);
                self.parity(needed, R12, R2);
                self.a.store_indexed(32, R12, R3, linear);
                self.a.mov(source, R1);
                self.flags = Flags::Cpsr(true);
            }
            (ExchangeOp::Compare, Value::Register(dst)) => {
                let target = guest(dst.index);
                let eax = guest(0);
                if needed & PF_AF != 0 {
                    self.a.dp(Dp::Eor, false, R2, eax, target);
                }
                self.a.dp(Dp::Sub, true, R0, eax, target);
                self.parity(needed, R0, R2);
                self.a.mov_cond(Cond::Ne, eax, target);
                self.a.mov_cond(Cond::Eq, target, source);
                self.flags = Flags::Cpsr(false);
            }
            (ExchangeOp::Compare, Value::Memory { address, .. }) => {
                self.clobber();
                let miss = self.miss(index);
                let linear = self.access(address, 32, true, miss, R0);
                let eax = guest(0);
                self.a.load_indexed(32, false, R1, R3, linear);
                if needed & PF_AF != 0 {
                    self.a.dp(Dp::Eor, false, R2, eax, R1);
                }
                self.a.dp(Dp::Sub, true, R12, eax, R1);
                self.parity(needed, R12, R2);
                self.a
                    .with_cond(Cond::Eq, |a| a.store_indexed(32, source, R3, linear));
                self.a.mov_cond(Cond::Ne, eax, R1);
                self.flags = Flags::Cpsr(false);
            }
            (_, Value::Immediate(_)) => unreachable!(),
        }
    }

    fn bit_test(&mut self, index: usize, src: Value, bit: Value, bits: u8, kind: u8) {
        self.clobber();
        let (value, linear) = match src {
            Value::Register(reg) => (guest(reg.index), None),
            Value::Memory { address, bits } => {
                let miss = self.miss(index);
                let linear = self.access(address, bits, kind != 0, miss, R0);
                self.a.load_indexed(bits, false, R1, R3, linear);
                (R1, Some(linear))
            }
            Value::Immediate(_) => unreachable!(),
        };
        let mask = match bit {
            Value::Immediate(bit) => Operand::Imm(1 << (bit & u32::from(bits - 1))),
            Value::Register(reg) => {
                self.a.dp(
                    Dp::And,
                    false,
                    R2,
                    guest(reg.index),
                    Operand::Imm(u32::from(bits - 1)),
                );
                self.a.mov(R12, Operand::Imm(1));
                self.a.mov(R2, Operand::RegShift(R12, 0, R2));
                R2.into()
            }
            Value::Memory { .. } => unreachable!(),
        };
        let a = &mut self.a;
        a.ldr(R12, FRAME, NZCV);
        a.tst(value, mask);
        // A clear bit means CF=0, which Frame stores as C=1.
        a.dp_cond(Cond::Eq, Dp::Orr, false, R12, R12, Operand::Imm(C_BIT));
        a.dp_cond(Cond::Ne, Dp::Bic, false, R12, R12, Operand::Imm(C_BIT));
        a.str(R12, FRAME, NZCV);
        let modify = match kind {
            1 => Some(Dp::Orr),
            2 => Some(Dp::Bic),
            3 => Some(Dp::Eor),
            _ => None,
        };
        if let Some(op) = modify {
            a.dp(op, false, value, value, mask);
            if let Some(linear) = linear {
                a.store_indexed(bits, value, R3, linear);
            }
        }
        self.flags = Flags::Frame;
    }

    /// PUSH a 32-bit value held outside the guard's registers.
    fn push(&mut self, value: Reg, miss: Label) {
        assert!(![R0, R2, R3, R12].contains(&value));
        self.a.dp(Dp::Sub, false, R0, ESP, Operand::Imm(4));
        self.guard(R0, 4, true, miss);
        self.a.store_indexed(32, value, R3, R0);
        self.a.mov(ESP, R0);
    }

    /// Continue at a static `eip`, probing its table slot inline.
    fn exit_to(&mut self, eip: u32) {
        self.spill();
        let a = &mut self.a;
        a.mov_imm(R0, eip);
        a.ldr(R1, FRAME, TABLE);
        let offset = table_slot(eip) as u32 * 8;
        if offset > 0xfff {
            a.dp(Dp::Add, false, R1, R1, Operand::Imm(offset & !0xfff));
        }
        let offset = (offset & 0xfff) as i32;
        a.ldr(R2, R1, offset);
        a.cmp(R2, R0);
        a.with_cond(Cond::Eq, |a| a.ldr(PC, R1, offset + 4));
        a.ldr(PC, FRAME, DISPATCH);
    }

    fn exit_with(&mut self, eip: Reg) {
        if eip != R0 {
            self.a.mov(R0, eip);
        }
        self.spill();
        self.a.ldr(PC, FRAME, DISPATCH);
    }

    /// Complete the block; Frame.eip already names the next instruction.
    fn exit(&mut self) {
        self.a.ldr(R0, FRAME, EIP);
        self.exit_with(R0);
    }
}
