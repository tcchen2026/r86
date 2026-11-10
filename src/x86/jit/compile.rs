//! Direct i386 integer lowering to AArch64 with one register/flag model.
//!
//! W19..W26 are guest GPRs, W15 holds packed NZCV with x86 carry/borrow, X27
//! the budget and X28 the invocation frame. W0..W6 are temporaries; W7 belongs
//! to SoftMMU. Narrow arithmetic is shifted
//! into the high bits before ADDS/SUBS so ARM NZCV describes the x86 width; x86
//! subtraction inverts ARM's no-borrow C. PF retains the result byte; AF is
//! deferred as bit 4 of W8 XOR W17, saving an XOR for ordinary arithmetic.
//! INC/DEC preserve CF and partial writes preserve the rest of the guest GPR.
//! BSWAP, CMOVcc, LAHF/SAHF, CLC/STC/CMC, RCL/RCR and JECXZ also use this
//! resident state. tiny386 i386.c was consulted; v86 instructions_0f.rs confirms
//! CMOV's unconditional memory read. Carry rotates include CF in a 9/17/33-bit
//! ring and retain old flags until a memory store commits. JECXZ tests ECX,
//! independently of arithmetic flags; address-size overrides remain interpreted.
//!
//! Dead flag writes are eliminated between register operations. Every exit and
//! faultable memory instruction makes its incoming flags live, before selecting
//! the access lowering. SoftMMU misses return before the attempted instruction.
//! All state is committed before returning to Rust; no lazy-flag ABI leaks out.
//!
//! Entry requires 32-bit code, valid instruction bytes and branch destinations
//! inside CS, no debug step, and a budget ending at or before the next machine
//! event. During an interrupt shadow the adapter may invoke only a recognized
//! one-instruction entry with budget one, retaining the shadow/attention boundary.
//! An insufficient budget returns without executing any instruction. The runtime
//! executes the remainder through ordinary semantics.
//!
//! Semantics follow the shared interpreter; encodings are generated
//! independently. Notes on v86 (src/rust/cpu) record where it agrees or not.
//! - Arithmetic stores before committing GPRs or flags; INC/DEC preserve CF.
//!   v86 arith.rs agrees on CF; its cpu.rs RMW callbacks set flags first.
//! - PUSH evaluates its source first and commits ESP after the store (v86
//!   misc_instr.rs agrees).
//! - A masked zero shift count keeps flags; undefined shift flags follow the
//!   interpreter. v86 arith.rs agrees on zero counts but recomputes AF and keeps
//!   the input sign in SHR's OF for counts above one.
//! - SETcc changes no flags; BT changes only CF; immediate bit indices wrap at
//!   the width (v86 misc_instr.rs and instructions_0f.rs agree).
//! - MOVS commits each element; count zero and overlap follow the interpreter,
//!   as do repeat chunk size and attempted ticks (v86 string.rs agrees on the
//!   element semantics). Scalar LODS/STOS/SCAS complete the memory operation
//!   before changing the accumulator, flags or index (v86 string.rs agrees).
//! - CLD/STD update the frame's DF. CLI/POPF share privilege rules with the
//!   interpreter; enabling IF exits for attention; the adapter owns VM changes
//!   and shadow boundaries (v86 instructions.rs agrees on CPL/IOPL gating).
//! - PUSHFD clears RF/VM only in its stack image; CWD/CDQ sign-extend (v86
//!   instructions.rs agrees).
//! - MUL/IMUL produce full-width products, signed truncation and CF/OF overflow;
//!   DIV/IDIV reject zero and overflow before publishing results (v86 arith.rs
//!   agrees).
//! - BSF/BSR with a zero source keep the destination and the undefined status
//!   flags. v86 arith.rs agrees on the destination but clears CF and recomputes
//!   the other flags.
//! - NEG subtracts from zero; LEAVE commits ESP and BP/EBP only after its stack
//!   read (v86 agrees). Native LEAVE requires a flat 32-bit stack.
//! - XADD/CMPXCHG/XCHG memory forms require write permission even when CMPXCHG
//!   fails (v86 cpu.rs agrees). They store before committing GPRs or flags; XADD
//!   writes its source before its register destination so aliases receive the
//!   sum. LOCK and implicit XCHG locking rely on this machine's exclusive
//!   single-vCPU invocation, not host atomics; neither devices nor another vCPU
//!   can access RAM midway.

use super::a64::{Assembler, Cond, EmitError, Label, Reg, ZR};
use super::decode::{
    Address, Alu, Block, BranchCondition, ExchangeOp, Op, Register, StringOp, Value,
};
use super::frame::{self, AUXILIARY, BUDGET, FLAGS, FRAME, Frame, PARITY, PC, guest_reg};
use super::{memory::FaultSite, soft};
use alloc::vec::Vec;
use core::mem::offset_of;

const LHS: Reg = Reg::new(0);
const RHS: Reg = Reg::new(1);
const RESULT: Reg = Reg::new(2);
const TMP: Reg = Reg::new(5);
const TMP2: Reg = Reg::new(6);
pub(super) const STATUS: u32 = 0x8d5;

// Keep the four ARM flags packed between x86 instructions. A full arithmetic
// update takes MRS plus a carry inversion for subtraction, instead of extracting
// and reinserting four bits. PF/AF use Frame's deferred encoding; other flags stay
// in the invocation frame. Partial writers still preserve every untouched bit.
const fn native_flag_bit(x86_bit: u8) -> u8 {
    match x86_bit {
        0 => 29,
        6 => 30,
        7 => 31,
        11 => 28,
        _ => x86_bit,
    }
}

fn read_register(a: &mut Assembler, dst: Reg, src: Register) {
    if src.bits == 32 {
        a.mov(dst, guest_reg(src.index as usize), false);
    } else {
        a.bitfield(
            dst,
            guest_reg(src.index as usize),
            src.shift,
            src.shift + src.bits - 1,
            false,
            false,
        );
    }
}

fn write_register(a: &mut Assembler, dst: Register, src: Reg) {
    if dst.bits == 32 {
        a.mov(guest_reg(dst.index as usize), src, false);
    } else {
        a.bfi32(guest_reg(dst.index as usize), src, dst.shift, dst.bits);
    }
}

struct InstructionExit<'a> {
    segments: &'a core::cell::Cell<u32>,
    range: Option<super::memory::AccessRange>,
    miss: Label,
    eip: u32,
    steps_before: u16,
    repeat: Option<(u32, u32)>, // Continuation control and next EIP.
    division: bool,
}

impl InstructionExit<'_> {
    fn guard(&self) -> soft::Guard<'_> {
        soft::Guard {
            miss: self.miss,
            segments: self.segments,
            range: self.range,
        }
    }
}

pub struct CompiledBlock {
    pub words: Vec<u32>,
    pub cold_word: usize,
    pub relocations: Vec<super::a64::Relocation>,
    pub faults: Vec<FaultSite>,
    pub edges: Vec<super::links::Edge>,
}

pub struct CompiledEntry {
    pub words: Vec<u32>,
    pub dispatch_offset: usize,
    pub call_dispatch_offset: usize,
    pub return_offset: usize,
    pub recheck_offset: usize,
}

fn read_value(
    a: &mut Assembler,
    dst: Reg,
    value: Value,
    site: Option<&InstructionExit>,
    faults: &mut Vec<FaultSite>,
) {
    match value {
        Value::Register(src) => read_register(a, dst, src),
        Value::Immediate(value) => a.mov_imm(dst, value.into(), false),
        Value::Memory { address, bits } => {
            let site = site.expect("memory instruction has an exit");
            let offset = soft::load(a, dst, address, bits, site.guard());
            faults.push(FaultSite::new(
                offset,
                site.eip,
                site.steps_before.into(),
                bits / 8,
                false,
            ));
        }
    }
}

pub(super) fn flags_written(op: Op) -> u32 {
    match op {
        Op::Alu { .. }
        | Op::PopFlags { .. }
        | Op::Compare { .. }
        | Op::Negate { .. }
        | Op::String {
            kind: StringOp::Scan | StringOp::Compare,
            ..
        }
        | Op::Exchange {
            kind: ExchangeOp::Add | ExchangeOp::Compare,
            ..
        } => STATUS,
        Op::MemoryAlu { preserve_carry, .. } => STATUS & if preserve_carry { !1 } else { u32::MAX },
        Op::IncDec { .. } => STATUS & !1,
        Op::BitTest { .. } | Op::Carry { .. } => 1,
        Op::StoreFlagsByte => 0xd5,
        Op::BitScan { .. } | Op::CompareExchange8 { .. } => 0x40,
        Op::Imul { .. } | Op::Multiply { .. } => 0x801,
        Op::Shift {
            count: Value::Immediate(0),
            ..
        } => 0,
        Op::Shift { kind: 3..=6, .. } => 0x801,
        Op::Shift { .. } => STATUS,
        Op::DoubleShift { count, .. } => match count {
            Value::Immediate(0) => 0,
            Value::Immediate(1) | Value::Register(_) => STATUS & !0x10,
            _ => STATUS & !0x810,
        },
        _ => 0,
    }
}

/// Full guest GPRs are already resident. Narrow/high-byte values and faultable
/// loads need extraction into a scratch register before arithmetic can use them.
fn operand(
    a: &mut Assembler,
    scratch: Reg,
    value: Value,
    site: Option<&InstructionExit>,
    faults: &mut Vec<FaultSite>,
) -> Reg {
    if let Value::Register(Register {
        index, bits: 32, ..
    }) = value
    {
        guest_reg(index as usize)
    } else {
        read_value(a, scratch, value, site, faults);
        scratch
    }
}

/// Narrow ALU operations discard high input bits in their sign-aligned flag
/// calculation and partial destination/store commit. PF and AF only consume low
/// bits too, so AL/AX inputs need no extraction. AH still needs repositioning.
/// Multiplication, division and right shifts must retain their own input widths.
fn alu_operand(
    a: &mut Assembler,
    scratch: Reg,
    value: Value,
    site: Option<&InstructionExit>,
    faults: &mut Vec<FaultSite>,
) -> Reg {
    if let Value::Register(Register {
        index, shift: 0, ..
    }) = value
    {
        guest_reg(index as usize)
    } else {
        operand(a, scratch, value, site, faults)
    }
}

/// x86 flag liveness shared by the native backends: per instruction, the
/// written flags still observable afterwards. Exits make every flag live.
pub(super) fn flag_requirements(block: &Block) -> Vec<u32> {
    let mut live = STATUS;
    let mut needed = alloc::vec![0; block.instructions.len()];
    for (index, instruction) in block.instructions.iter().enumerate().rev() {
        let written = flags_written(instruction.op);
        needed[index] = live & written;
        live &= !written;
        // Carry is an input to the value even when all output flags are dead.
        if matches!(
            instruction.op,
            Op::Alu {
                kind: Alu::Adc | Alu::Sbb,
                ..
            } | Op::MemoryAlu {
                kind: Alu::Adc | Alu::Sbb,
                ..
            } | Op::Shift { kind: 5 | 6, .. }
                | Op::Carry { value: None }
        ) {
            live |= 1;
        }
        // CL may mask to zero, leaving every previous flag observable.
        if matches!(
            instruction.op,
            Op::Shift {
                count: Value::Register(_),
                ..
            } | Op::DoubleShift {
                count: Value::Register(_),
                ..
            }
        ) {
            live |= STATUS;
        }
        if instruction.op.can_exit() {
            live |= STATUS;
        }
        if matches!(instruction.op, Op::LoadFlagsByte) {
            live |= 0xd5;
        }
        if let Op::Branch {
            condition: Some(BranchCondition::Flags(condition)),
            ..
        }
        | Op::SetCondition { condition, .. }
        | Op::ConditionalMove { condition, .. } = instruction.op
        {
            live |= match condition >> 1 {
                0 => 0x800,
                1 => 1,
                2 => 0x40,
                3 => 0x41,
                4 => 0x80,
                5 => 4,
                6 => 0x880,
                7 => 0x8c0,
                _ => unreachable!(),
            };
        }
    }
    needed
}

fn flag_bit(a: &mut Assembler, mask: u32, bit: u8, condition: Cond) {
    if mask & (1 << bit) != 0 {
        a.cset(TMP, condition);
        a.bfi32(FLAGS, TMP, native_flag_bit(bit), 1);
    }
}

/// Old operands remain intact when AF needs them; low result bits supply PF.
/// NZCV describes the full guest width, including byte and word operations.
fn arithmetic_flags(
    a: &mut Assembler,
    mask: u32,
    sub: bool,
    logical: bool,
    (lhs, rhs, result): (Reg, Reg, Reg),
) {
    if mask & 0x8c1 == 0x8c1 {
        a.mrs_nzcv(FLAGS);
        if sub {
            a.xor_bit(FLAGS, FLAGS, 29, false);
        }
    } else if mask & 0x8c1 == 0x8c0 {
        // INC/DEC update N/Z/V together but preserve the incoming x86 carry.
        // Copying the packed result costs less than three CSET/BFI pairs.
        a.bitfield(TMP2, FLAGS, 29, 29, false, false);
        a.mrs_nzcv(FLAGS);
        a.bfi32(FLAGS, TMP2, 29, 1);
    } else {
        flag_bit(a, mask, 6, Cond::Eq);
        flag_bit(a, mask, 7, Cond::Mi);
        if logical {
            for bit in [0, 11] {
                if mask & (1 << bit) != 0 {
                    a.bfi32(FLAGS, ZR, native_flag_bit(bit), 1);
                }
            }
        } else {
            flag_bit(a, mask, 0, if sub { Cond::Cc } else { Cond::Cs });
            flag_bit(a, mask, 11, Cond::Vs);
        }
    }
    if logical {
        if mask & 0x10 != 0 {
            a.mov(
                AUXILIARY,
                if mask & 4 != 0 { result } else { PARITY },
                false,
            );
        }
    } else {
        if mask & 0x10 != 0 {
            // AF is bit 4 of AUXILIARY ^ PARITY. Sharing the result already
            // retained for PF removes its XOR from ordinary full flag writes.
            a.logical(2, AUXILIARY, lhs, rhs, false);
            if mask & 4 == 0 {
                a.logical(2, AUXILIARY, AUXILIARY, result, false);
                a.logical(2, AUXILIARY, AUXILIARY, PARITY, false);
            }
        }
    }
    if mask & 4 != 0 {
        a.mov(PARITY, result, false);
    }
}

fn materialize_parity(a: &mut Assembler, dst: Reg) {
    // Fold to four bits, then select the even-parity bit from a 16-bit table.
    // X86 branches rarely consume PF; every Rust/native boundary still does.
    a.bitfield(TMP, PARITY, 4, 31, false, false);
    a.logical(2, TMP, TMP, PARITY, false);
    a.bitfield(TMP, TMP, 0, 3, false, false);
    a.mov_imm(TMP2, 0x9669, false);
    a.shift_reg(TMP, TMP2, TMP, false, 1);
    a.bfi32(dst, TMP, 2, 1);
}

/// Materialize the currently resident flags without changing their live form.
/// PUSHF consumes the same image as a precise exit, including deferred PF/AF.
fn current_flags(a: &mut Assembler, dst: Reg) {
    a.load_store(true, dst, FRAME, offset_of!(Frame, eflags) as u32, 2);
    materialize_parity(a, dst);
    a.logical(2, TMP, AUXILIARY, PARITY, false);
    a.bitfield(TMP, TMP, 4, 4, false, false);
    a.bfi32(dst, TMP, 4, 1);
    for (source, target, width) in [(29, 0, 1), (30, 6, 2), (28, 11, 1)] {
        a.bitfield(TMP, FLAGS, source, source + width - 1, false, false);
        a.bfi32(dst, TMP, target, width);
    }
}

/// CMP x,0 and TEST/AND/OR x,x share the same status result: x determines
/// S/Z/P, while C/O/A are zero in our x86 model. A memory source still performs
/// its guarded load even if the flags are dead; faults cannot be optimized away.
fn flags_from_value(
    a: &mut Assembler,
    value: Value,
    bits: u8,
    mask: u32,
    site: Option<&InstructionExit>,
    faults: &mut Vec<FaultSite>,
) {
    if mask == 0 && !value.is_memory() {
        return;
    }
    let value = alu_operand(a, RESULT, value, site, faults);
    if mask == 0 {
        return;
    }
    let source = if bits == 32 {
        value
    } else {
        a.bitfield(TMP, value, bits, bits - 1, false, false);
        TMP
    };
    a.logical(3, ZR, source, source, false);
    arithmetic_flags(a, mask, false, true, (LHS, RHS, value));
}

fn calculate(
    a: &mut Assembler,
    kind: Alu,
    bits: u8,
    mask: u32,
    (lhs, rhs, result): (Reg, Reg, Reg),
) {
    let sub = matches!(kind, Alu::Sub | Alu::Sbb | Alu::Cmp);
    let logical = matches!(kind, Alu::And | Alu::Or | Alu::Xor | Alu::Test);
    if matches!(kind, Alu::Adc | Alu::Sbb) {
        a.bitfield(TMP, FLAGS, 29, 29, false, false);
        if sub {
            a.add_sub(ZR, ZR, TMP, false, true, true); // C = !x86.CF
        } else {
            a.add_sub_imm(ZR, TMP, 1, false, true, true); // C = x86.CF
        }
        if bits == 32 || mask == 0 {
            a.add_sub_carry(result, lhs, rhs, false, sub, mask != 0);
        } else {
            // Align the guest sign bit with ARM's. For ADC, ones below the
            // operand propagate C into its least significant bit. SBC naturally
            // borrows into that bit. Neither changes N/C/V for the guest width.
            a.bitfield(TMP, lhs, bits, bits - 1, false, false);
            a.bitfield(TMP2, rhs, bits, bits - 1, false, false);
            if !sub {
                a.mov_imm(result, (1_u64 << (32 - bits)) - 1, false);
                a.logical(1, TMP2, TMP2, result, false);
            }
            a.add_sub_carry(result, TMP, TMP2, false, sub, true);
            a.bitfield(result, result, 32 - bits, 31, false, false);
            if mask & 0x40 != 0 {
                // Padding bits can be nonzero when the narrow result is zero.
                // Correct Z without disturbing the just-computed N/C/V.
                a.mrs_nzcv(TMP);
                a.add_sub_imm(ZR, result, 0, false, true, true);
                a.cset(TMP2, Cond::Eq);
                a.bfi32(TMP, TMP2, 30, 1);
                a.msr_nzcv(TMP);
            }
        }
    } else if logical {
        let operation = match kind {
            Alu::Or => 1,
            Alu::Xor => 2,
            _ => 0,
        };
        let flags_in_and = operation == 0 && mask != 0 && bits == 32;
        a.logical(
            if flags_in_and { 3 } else { operation },
            result,
            lhs,
            rhs,
            false,
        );
        if mask != 0 && !flags_in_and {
            let source = if bits == 32 {
                result
            } else {
                a.bitfield(TMP, result, bits, bits - 1, false, false);
                TMP
            };
            a.logical(3, ZR, source, source, false);
        }
    } else if bits != 32 && mask != 0 {
        // UBFM with immr=bits is LSL #(32-bits); discarded source high bits
        // include sign extension of group-83 immediates.
        a.bitfield(TMP, lhs, bits, bits - 1, false, false);
        a.bitfield(TMP2, rhs, bits, bits - 1, false, false);
        a.add_sub(result, TMP, TMP2, false, sub, true);
        a.bitfield(result, result, 32 - bits, 31, false, false);
    } else {
        a.add_sub(result, lhs, rhs, false, sub, mask != 0);
    }
}

fn finish_alu_flags(a: &mut Assembler, kind: Alu, mask: u32, registers: (Reg, Reg, Reg)) {
    if mask != 0 {
        arithmetic_flags(
            a,
            mask,
            matches!(kind, Alu::Sub | Alu::Sbb | Alu::Cmp),
            matches!(kind, Alu::And | Alu::Or | Alu::Xor | Alu::Test),
            registers,
        );
    }
}

fn arithmetic_immediate(
    a: &mut Assembler,
    lhs: Reg,
    value: u16,
    sub: bool,
    mask: u32,
    result: Reg,
) {
    // Retain only the part of the old operand needed for AF, before a resident
    // guest destination can overwrite it. No guest fault is possible between
    // this snapshot and the arithmetic/flag commit.
    if mask & 0x10 != 0 {
        a.mov(AUXILIARY, lhs, false);
        if value & 0x10 != 0 {
            a.xor_bit(AUXILIARY, AUXILIARY, 4, false);
        }
    }
    a.add_sub_imm(result, lhs, value, false, sub, mask != 0);
    if mask != 0 {
        arithmetic_flags(a, mask & !0x10, sub, false, (lhs, ZR, result));
        if mask & 0x10 != 0 && mask & 4 == 0 {
            a.logical(2, AUXILIARY, AUXILIARY, result, false);
            a.logical(2, AUXILIARY, AUXILIARY, PARITY, false);
        }
    }
}

fn shift(a: &mut Assembler, dst: Register, kind: u8, count: Value, mask: u32) {
    if count == Value::Immediate(0) {
        return;
    }
    let done = a.label();
    read_register(a, LHS, dst);
    if kind >= 5 {
        carry_rotate_count(a, dst.bits, count);
        a.cbz(RHS, false, false, done);
        calculate_carry_rotate(a, dst.bits, kind);
        carry_rotate_flags(a, dst.bits, kind, mask);
        write_register(a, dst, RESULT);
        a.bind(done);
        return;
    }
    match count {
        Value::Immediate(value) => {
            if kind >= 3 {
                a.mov_imm(RHS, value.into(), false);
            }
        }
        Value::Register(reg) => {
            read_register(a, RHS, reg);
            a.bitfield(RHS, RHS, 0, 4, false, false);
            a.cbz(RHS, false, false, done);
        }
        Value::Memory { .. } => unreachable!("shift count is immediate or CL"),
    }
    if kind >= 3 {
        // Replicate byte/word operands through W0 so RORV naturally wraps at
        // their width. Nonzero counts divisible by 8/16 still update CF/OF.
        for size in [8, 16] {
            if dst.bits <= size {
                a.bitfield(TMP, LHS, 32 - size, 31 - size, false, false);
                a.logical(1, LHS, LHS, TMP, false);
            }
        }
        if kind == 4 {
            a.add_sub(RHS, ZR, RHS, false, true, false);
        }
        a.shift_reg(RESULT, LHS, RHS, false, 3);
        let carry_bit = if kind == 4 { 0 } else { dst.bits - 1 };
        if mask & 1 != 0 {
            a.bitfield(TMP, RESULT, carry_bit, carry_bit, false, false);
            a.bfi32(FLAGS, TMP, 29, 1);
        }
        if mask & 0x800 != 0 {
            let other_bit = if kind == 4 { 0 } else { dst.bits - 2 };
            a.bitfield(TMP, RESULT, dst.bits - 1, dst.bits - 1, false, false);
            a.bitfield(TMP2, RESULT, other_bit, other_bit, false, false);
            a.logical(2, TMP, TMP, TMP2, false);
            a.bfi32(FLAGS, TMP, 28, 1);
        }
        write_register(a, dst, RESULT);
        a.bind(done);
        return;
    }
    calculate_shift(a, dst.bits, kind, count, mask);
    shift_flags(a, dst.bits, kind, count, mask);
    write_register(a, dst, RESULT);
    a.bind(done);
}

// RHS is the effective count. Immediate counts were normalized by the decoder;
// CL is masked before modulo 9/17. Use a 64-bit ring for the carry plus operand.
fn carry_rotate_count(a: &mut Assembler, bits: u8, count: Value) {
    match count {
        Value::Immediate(value) => a.mov_imm(RHS, value.into(), false),
        Value::Register(reg) => {
            read_register(a, RHS, reg);
            a.bitfield(RHS, RHS, 0, 4, false, false);
            if bits < 32 {
                a.mov_imm(TMP, u64::from(bits + 1), false);
                a.divide64(TMP2, RHS, TMP, false);
                a.multiply_subtract64(RHS, TMP2, TMP, RHS);
            }
        }
        Value::Memory { .. } => unreachable!(),
    }
}

// LHS and RHS are consumed; RESULT retains the carry at bit `bits` until the
// store has committed. Faults therefore still observe the old packed flags.
fn calculate_carry_rotate(a: &mut Assembler, bits: u8, kind: u8) {
    a.bitfield(TMP, FLAGS, 29, 29, false, false);
    a.bitfield(TMP, TMP, 64 - bits, 63 - bits, true, false);
    a.logical(1, LHS, LHS, TMP, true);
    a.mov_imm(TMP2, u64::from(bits + 1), false);
    a.add_sub(TMP2, TMP2, RHS, false, true, false);
    let right = u8::from(kind == 6);
    a.shift_reg(RESULT, LHS, RHS, true, right);
    a.shift_reg(TMP, LHS, TMP2, true, right ^ 1);
    a.logical(1, RESULT, RESULT, TMP, true);
}

fn carry_rotate_flags(a: &mut Assembler, bits: u8, kind: u8, mask: u32) {
    a.bitfield(TMP, RESULT, bits, bits, true, false);
    if mask & 1 != 0 {
        a.bfi32(FLAGS, TMP, 29, 1);
    }
    if mask & 0x800 != 0 {
        if kind == 6 {
            a.bitfield(TMP, RESULT, bits - 2, bits - 2, false, false);
        }
        a.bitfield(TMP2, RESULT, bits - 1, bits - 1, false, false);
        a.logical(2, TMP, TMP, TMP2, false);
        a.bfi32(FLAGS, TMP, 28, 1);
    }
}

fn calculate_shift(a: &mut Assembler, bits: u8, kind: u8, count: Value, mask: u32) {
    if kind == 2 && bits != 32 {
        a.bitfield(LHS, LHS, 0, bits - 1, false, true);
    }
    // A wide left shift retains the outgoing bit, including 8/16-bit counts
    // beyond the operand width. Count <= 31 keeps this operation within 64 bits.
    let wide = kind == 0 && mask != 0;
    if let Value::Immediate(count) = count {
        let count = count as u8;
        debug_assert!((1..32).contains(&count));
        let width = if wide { 64 } else { 32 };
        let (rotate, end) = if kind == 0 {
            (width - count, width - count - 1)
        } else {
            (count, 31)
        };
        a.bitfield(RESULT, LHS, rotate, end, wide, kind == 2);
    } else {
        a.shift_reg(RESULT, LHS, RHS, wide, kind);
    }
}

fn shift_flags(a: &mut Assembler, bits: u8, kind: u8, count: Value, mask: u32) {
    if mask != 0 {
        let carry = Reg::new(3);
        let overflow = Reg::new(4);
        if kind == 0 {
            a.bitfield(carry, RESULT, bits, bits, true, false);
            a.bitfield(overflow, RESULT, bits - 1, bits - 1, false, false);
            a.logical(2, overflow, overflow, carry, false);
        } else {
            if let Value::Immediate(count) = count {
                let bit = count as u8 - 1;
                a.bitfield(carry, LHS, bit, bit, false, false);
                if kind == 1 && count == 1 {
                    a.bitfield(overflow, LHS, bits - 1, bits - 1, false, false);
                } else {
                    a.mov_imm(overflow, 0, false);
                }
            } else {
                a.add_sub_imm(TMP, RHS, 1, false, true, false);
                a.shift_reg(carry, LHS, TMP, false, 1);
                if kind == 1 {
                    a.add_sub_imm(ZR, RHS, 1, false, true, true);
                    a.cset(overflow, Cond::Eq);
                    a.bitfield(TMP, LHS, bits - 1, bits - 1, false, false);
                    a.logical(0, overflow, overflow, TMP, false);
                } else {
                    a.mov_imm(overflow, 0, false);
                }
            }
        }
        // Z/S/P describe the truncated result; AF is cleared by our x86 model.
        let sign_value = if bits != 32 {
            a.bitfield(TMP, RESULT, bits, bits - 1, false, false);
            TMP
        } else {
            RESULT
        };
        a.logical(3, ZR, sign_value, sign_value, false);
        // C/O are replaced below. A full write can capture NZCV together;
        // preserving those obsolete bits would split N/Z into CSET/BFI pairs.
        arithmetic_flags(a, mask, false, true, (LHS, RHS, RESULT));
        if mask & 1 != 0 {
            a.bfi32(FLAGS, carry, 29, 1);
        }
        if mask & 0x800 != 0 {
            a.bfi32(FLAGS, overflow, 28, 1);
        }
    }
}

/// Form a 64-bit concatenation for the 32-bit funnel shift. AF stays unchanged;
/// our existing model also preserves undefined OF when the masked count != 1.
fn double_shift(
    a: &mut Assembler,
    dst: Register,
    src: Register,
    right: bool,
    count: Value,
    mask: u32,
) {
    assert_eq!(dst.bits, 32);
    if count == Value::Immediate(0) {
        return;
    }
    let done = a.label();
    let source = PC; // PC is materialized only at dispatch/exit boundaries.
    let carry = Reg::new(3);
    let overflow = Reg::new(4);
    read_register(a, LHS, dst);
    read_register(a, source, src);
    match count {
        Value::Immediate(value) => a.mov_imm(RHS, value.into(), false),
        Value::Register(reg) => {
            read_register(a, RHS, reg);
            a.bitfield(RHS, RHS, 0, 4, false, false);
            a.cbz(RHS, false, false, done);
        }
        Value::Memory { .. } => unreachable!("double shift count is immediate or CL"),
    }
    a.bitfield(
        RESULT,
        if right { source } else { LHS },
        32,
        31,
        true,
        false,
    );
    a.logical(1, RESULT, RESULT, if right { LHS } else { source }, true);
    if right {
        a.shift_reg(RESULT, RESULT, RHS, true, 1);
        if mask & 1 != 0 {
            a.add_sub_imm(TMP, RHS, 1, false, true, false);
            a.shift_reg(carry, LHS, TMP, false, 1);
        }
    } else {
        a.mov_imm(TMP, 32, false);
        a.add_sub(TMP, TMP, RHS, false, true, false);
        a.shift_reg(RESULT, RESULT, TMP, true, 1);
        if mask & 1 != 0 {
            a.bitfield(carry, RESULT, 32, 32, true, false);
        }
    }
    if mask != 0 {
        a.logical(3, ZR, RESULT, RESULT, false);
        if mask & 4 != 0 {
            a.logical(2, AUXILIARY, AUXILIARY, PARITY, false);
        }
        arithmetic_flags(a, mask & !0x811, false, true, (LHS, RHS, RESULT));
        if mask & 4 != 0 {
            a.logical(2, AUXILIARY, AUXILIARY, PARITY, false);
        }
        if mask & 1 != 0 {
            a.bfi32(FLAGS, carry, 29, 1);
        }
        if mask & 0x800 != 0 {
            let keep_overflow = a.label();
            if matches!(count, Value::Register(_)) {
                a.add_sub_imm(ZR, RHS, 1, false, true, true);
                a.b_cond(Cond::Ne, keep_overflow);
            }
            a.logical(2, overflow, LHS, RESULT, false);
            a.bitfield(overflow, overflow, 31, 31, false, false);
            a.bfi32(FLAGS, overflow, 28, 1);
            a.bind(keep_overflow);
        }
    }
    write_register(a, dst, RESULT);
    a.bind(done);
}

fn alu(a: &mut Assembler, kind: Alu, bits: u8, mask: u32, registers: (Reg, Reg, Reg)) {
    let (lhs, rhs, result) = registers;
    let early_auxiliary = mask & 0x10 != 0
        && !matches!(kind, Alu::And | Alu::Or | Alu::Xor | Alu::Test)
        && (result == lhs || result == rhs);
    if early_auxiliary {
        // Memory operands have already completed. Save the old operand XOR
        // before an in-place register result, with no intervening fault site.
        a.logical(2, AUXILIARY, lhs, rhs, false);
    }
    calculate(a, kind, bits, mask, registers);
    finish_alu_flags(
        a,
        kind,
        if early_auxiliary { mask & !0x10 } else { mask },
        registers,
    );
    if early_auxiliary && mask & 4 == 0 {
        a.logical(2, AUXILIARY, AUXILIARY, result, false);
        a.logical(2, AUXILIARY, AUXILIARY, PARITY, false);
    }
}

fn read_stack(
    a: &mut Assembler,
    dst: Reg,
    bits: u8,
    site: &InstructionExit,
    faults: &mut Vec<FaultSite>,
    cached: bool,
) {
    if cached {
        let offset = soft::stack_load(a, dst, bits);
        faults.push(FaultSite::new(
            offset,
            site.eip,
            site.steps_before.into(),
            bits / 8,
            false,
        ));
    } else {
        soft::stack_guard(a, site.guard());
        read_value(
            a,
            dst,
            Value::Memory {
                address: Address::stack(),
                bits,
            },
            Some(site),
            faults,
        );
    }
}

fn push(
    a: &mut Assembler,
    value: Reg,
    bits: u8,
    stack_bytes: u16,
    site: &InstructionExit,
    faults: &mut Vec<FaultSite>,
    stack_offset: Option<u8>,
) {
    let offset = if let Some(displacement) = stack_offset {
        soft::stack_store(a, value, bits, displacement)
    } else {
        soft::stack_guard(a, site.guard());
        let mut address = Address::stack();
        address.displacement = 0u32.wrapping_sub(u32::from(stack_bytes));
        soft::prepare(a, address, bits, true, site.guard()).access(a, value, true)
    };
    faults.push(FaultSite::new(
        offset,
        site.eip,
        site.steps_before.into(),
        bits / 8,
        true,
    ));
    // ESP remains architecturally old if the recorded store faults.
    a.add_sub_imm(guest_reg(4), guest_reg(4), stack_bytes, false, true, false);
}

fn condition_value(a: &mut Assembler, condition: u8) {
    if condition >> 1 == 5 {
        materialize_parity(a, FLAGS);
    }
    let first = native_flag_bit(match condition >> 1 {
        0 => 11,
        1 | 3 => 0,
        2 => 6,
        4 | 6 | 7 => 7,
        5 => 2,
        _ => unreachable!(),
    });
    a.bitfield(TMP, FLAGS, first, first, false, false);
    if condition >> 1 == 3 {
        a.bitfield(TMP2, FLAGS, 30, 30, false, false);
        a.logical(1, TMP, TMP, TMP2, false);
    } else if condition >> 1 >= 6 {
        a.bitfield(TMP2, FLAGS, 28, 28, false, false);
        a.logical(2, TMP, TMP, TMP2, false);
        if condition >> 1 == 7 {
            a.bitfield(TMP2, FLAGS, 30, 30, false, false);
            a.logical(1, TMP, TMP, TMP2, false);
        }
    }
}

/// One MOVS element probes both RAM accesses before the load, then commits
/// indices after the store. Overlap follows the architectural element width.
/// Repeated MOVS uses the page loop below rather than this scalar path.
fn movs(
    a: &mut Assembler,
    bits: u8,
    segment: crate::x86::cpu::SegmentReg,
    site: &InstructionExit,
    faults: &mut Vec<FaultSite>,
    next_eip: u32,
) {
    let bytes = bits / 8;
    let source = Reg::new(3);
    let destination = Reg::new(4);
    a.load_store(true, TMP, FRAME, offset_of!(Frame, eflags) as u32, 2);
    a.tbz(TMP, 10, true, site.miss);
    let address = Address {
        base: Some(6),
        index: None,
        scale: 0,
        displacement: 0,
        segment,
    };
    soft::prepare_flat(a, address, bits, false, site.guard()).pointer(a, source);
    soft::prepare_flat(
        a,
        Address {
            base: Some(7),
            segment: crate::x86::cpu::SegmentReg::Es,
            ..address
        },
        bits,
        true,
        site.guard(),
    )
    .pointer(a, destination);
    let shift = bytes.trailing_zeros() as u8;
    faults.push(FaultSite::new(
        a.offset() as u32,
        site.eip,
        site.steps_before.into(),
        bytes,
        false,
    ));
    a.load_store(true, RESULT, source, 0, shift);
    faults.push(FaultSite::new(
        a.offset() as u32,
        site.eip,
        site.steps_before.into(),
        bytes,
        true,
    ));
    a.load_store(false, RESULT, destination, 0, shift);
    for reg in [guest_reg(6), guest_reg(7)] {
        a.add_sub_imm(reg, reg, bytes.into(), false, false, false);
    }
    #[cfg(any(test, feature = "jit-profile"))]
    for field in [
        offset_of!(Frame, memory_reads),
        offset_of!(Frame, memory_writes),
    ] {
        a.load_store(true, TMP, FRAME, field as u32, 3);
        a.add_sub_imm(TMP, TMP, 1, true, false, false);
        a.load_store(false, TMP, FRAME, field as u32, 3);
    }
    a.mov_imm(LHS, next_eip.into(), false);
}

/// The native loop and its shared interpreter tail together execute at most
/// 4096 elements per attempted instruction. Check only the current page and
/// commit each element before advancing. SCAS/CMPS may finish without
/// touching the next page even with ECX=-1. A slow next access retains the exact
/// completed count, so it neither replays RAM nor gets a second 4096-unit budget.
/// Page bounds follow tiny386's string loops. Guest tick boundaries remain r86's
/// own deterministic contract. v86 string.rs also stops SCAS on count/compare
/// before another access; its fast REP stops at a page and slow REP has no
/// 4096-element cap.
fn repeat_control(op: Op) -> Option<u32> {
    let (kind, bits, segment) = match op {
        Op::Movs {
            bits,
            repeat: true,
            segment,
        } => (0, bits, segment),
        Op::String {
            kind,
            bits,
            segment,
            repeat: Some(equal),
        } => (
            match kind {
                StringOp::Store => 1,
                StringOp::Load => 2,
                StringOp::Scan => {
                    if equal {
                        3
                    } else {
                        4
                    }
                }
                StringOp::Compare => {
                    if equal {
                        5
                    } else {
                        6
                    }
                }
            },
            bits,
            segment,
        ),
        _ => return None,
    };
    Some(kind | (u32::from(bits / 8) << 8) | ((segment as u32) << 16))
}

fn repeat_access_counts(_a: &mut Assembler, _control: u32) {
    #[cfg(any(test, feature = "jit-profile"))]
    {
        let a = _a;
        a.load_store(
            true,
            TMP,
            FRAME,
            offset_of!(Frame, repeat.completed) as u32,
            2,
        );
        a.add_sub(TMP, TMP, guest_reg(1), false, true, false);
        if _control & 255 >= 5 {
            a.add_sub(TMP, TMP, TMP, true, false, false);
        }
        for (enabled, field) in [
            (_control & 255 != 1, offset_of!(Frame, memory_reads)),
            (_control & 255 <= 1, offset_of!(Frame, memory_writes)),
        ] {
            if enabled {
                a.load_store(true, LHS, FRAME, field as u32, 3);
                a.add_sub(LHS, LHS, TMP, true, false, false);
                a.load_store(false, LHS, FRAME, field as u32, 3);
            }
        }
    }
}

fn repeat_string(
    a: &mut Assembler,
    control: u32,
    site: &InstructionExit,
    faults: &mut Vec<FaultSite>,
    next_eip: u32,
) {
    let kind = control & 255;
    let compare = kind >= 5;
    let stop_equal = matches!(kind, 4 | 6);
    let bytes = (control >> 8) as u8;
    let bits = bytes * 8;
    let segment = [
        crate::x86::cpu::SegmentReg::Es,
        crate::x86::cpu::SegmentReg::Cs,
        crate::x86::cpu::SegmentReg::Ss,
        crate::x86::cpu::SegmentReg::Ds,
        crate::x86::cpu::SegmentReg::Fs,
        crate::x86::cpu::SegmentReg::Gs,
    ][(control >> 16) as usize];
    let shift = bytes.trailing_zeros() as u8;
    let pointer = Reg::new(3);
    let units = Reg::new(4);
    let page_units = Reg::new(9); // Guard scratch, free inside the page loop.
    let source_index = guest_reg(6);
    let destination_index = guest_reg(7);
    let index = if kind == 2 {
        source_index
    } else {
        destination_index
    };
    let finished = a.label();
    // Only an exit exposes this field as a completed-element count.
    a.load_store(
        false,
        guest_reg(1),
        FRAME,
        offset_of!(Frame, repeat.completed) as u32,
        2,
    );
    a.cbz(guest_reg(1), false, false, finished);
    a.load_store(true, TMP, FRAME, offset_of!(Frame, eflags) as u32, 2);
    a.tbz(TMP, 10, true, site.miss);
    a.mov(units, guest_reg(1), false);
    a.mov_imm(
        TMP,
        crate::x86::interp::MAX_STRING_REPEAT_UNITS_PER_STEP.into(),
        false,
    );
    a.add_sub(ZR, units, TMP, false, true, true);
    let bounded = a.label();
    a.b_cond(Cond::Ls, bounded);
    a.mov(units, TMP, false);
    a.bind(bounded);
    if matches!(kind, 3 | 4) {
        read_register(
            a,
            LHS,
            Register {
                index: 0,
                shift: 0,
                bits,
            },
        );
    }
    let page = a.label();
    a.bind(page);
    let address = Address {
        base: Some(if kind == 0 || kind == 2 || compare {
            6
        } else {
            7
        }),
        index: None,
        scale: 0,
        displacement: 0,
        segment,
    };
    let access = soft::prepare_flat(a, address, bits, kind == 1, site.guard());
    access.pointer(a, pointer);
    if kind == 0 || compare {
        access.forward_units(a, TMP2);
        let destination = soft::prepare_flat(
            a,
            Address {
                base: Some(7),
                segment: crate::x86::cpu::SegmentReg::Es,
                ..address
            },
            bits,
            kind == 0,
            site.guard(),
        );
        destination.pointer(a, if compare { RESULT } else { LHS });
        destination.forward_units(a, page_units);
        a.add_sub(ZR, page_units, TMP2, false, true, true);
        let bounded = a.label();
        a.b_cond(Cond::Ls, bounded);
        a.mov(page_units, TMP2, false);
        a.bind(bounded);
    } else {
        access.forward_units(a, page_units);
    }
    a.add_sub(ZR, page_units, units, false, true, true);
    let bounded = a.label();
    a.b_cond(Cond::Ls, bounded);
    a.mov(page_units, units, false);
    a.bind(bounded);
    let again = a.label();
    let compared = if kind >= 3 { a.label() } else { finished };
    a.mov(TMP2, page_units, false);
    a.bind(again);
    faults.push(FaultSite::new(
        a.offset() as u32,
        site.eip,
        site.steps_before.into(),
        bytes,
        kind == 1,
    ));
    if kind == 1 {
        a.load_store(false, guest_reg(0), pointer, 0, shift);
    } else if compare {
        a.load_store(true, LHS, pointer, 0, shift);
        faults.push(FaultSite::new(
            a.offset() as u32,
            site.eip,
            site.steps_before.into(),
            bytes,
            false,
        ));
        a.load_store(true, RHS, RESULT, 0, shift);
        a.add_sub_imm(RESULT, RESULT, bytes.into(), true, false, false);
        a.add_sub(ZR, LHS, RHS, false, true, true);
    } else {
        a.load_store(true, RHS, pointer, 0, shift);
        if kind == 0 {
            faults.push(FaultSite::new(
                a.offset() as u32,
                site.eip,
                site.steps_before.into(),
                bytes,
                true,
            ));
            a.load_store(false, RHS, LHS, 0, shift);
            a.add_sub_imm(LHS, LHS, bytes.into(), true, false, false);
        } else {
            let accumulator = Register {
                index: 0,
                shift: 0,
                bits,
            };
            if kind == 2 {
                write_register(a, accumulator, RHS);
            } else {
                // Both operands are zero-extended to their x86 width. Only ZF
                // is consumed inside this already proven page, so equality
                // needs neither narrow-width shifting nor full flag packing.
                a.add_sub(ZR, LHS, RHS, false, true, true);
            }
        }
    }
    a.add_sub_imm(pointer, pointer, bytes.into(), true, false, false);
    a.add_sub_imm(page_units, page_units, 1, false, true, false);
    if kind >= 3 {
        a.b_cond(if stop_equal { Cond::Eq } else { Cond::Ne }, compared);
    }
    a.cbz(page_units, false, true, again);
    if kind >= 3 {
        a.bind(compared);
    }
    // The certified page chunk has no guest fault or event boundary. Preserve
    // sequential accesses (including overlapping MOVS), then commit its indices
    // and count before checking another page or finishing this attempted tick.
    if kind >= 3 {
        a.add_sub(TMP2, TMP2, page_units, false, true, false);
    }
    a.add_sub(guest_reg(1), guest_reg(1), TMP2, false, true, false);
    a.add_sub(units, units, TMP2, false, true, false);
    a.add_shifted(index, index, TMP2, shift, false);
    if kind == 0 || compare {
        a.add_shifted(source_index, source_index, TMP2, shift, false);
    }
    if kind >= 3 {
        alu(a, Alu::Cmp, bits, STATUS, (LHS, RHS, RESULT));
        a.tbz(FLAGS, 30, stop_equal, finished);
    }
    a.cbz(units, false, false, finished);
    a.b(page);
    a.bind(finished);
    repeat_access_counts(a, control);
    a.mov_imm(LHS, next_eip.into(), false);
    let next = a.label();
    a.cbz(guest_reg(1), false, false, next);
    if kind >= 3 {
        a.tbz(FLAGS, 30, stop_equal, next);
    }
    a.mov_imm(LHS, site.eip.into(), false);
    a.bind(next);
}

fn emit_progress(a: &mut Assembler, _block: &Block, count: u16) {
    // Entry reserved the whole block. Refund only instructions that did not
    // complete; the ordinary full-block path needs no further arithmetic.
    let refund = _block.instructions.len() as u16 - count;
    if refund != 0 {
        a.add_sub_imm(BUDGET, BUDGET, refund, true, false, false);
    }
    #[cfg(any(test, feature = "jit-profile"))]
    {
        let block = _block;
        let prefix = &block.instructions[..usize::from(count)];
        for (offset, accesses) in [
            (
                offset_of!(Frame, memory_reads),
                prefix
                    .iter()
                    .filter(|i| {
                        i.op.reads_memory()
                            && !matches!(
                                i.op,
                                Op::Movs { .. }
                                    | Op::String {
                                        repeat: Some(_),
                                        ..
                                    }
                            )
                    })
                    .map(|i| {
                        if matches!(
                            i.op,
                            Op::String {
                                kind: StringOp::Compare,
                                ..
                            }
                        ) {
                            2
                        } else {
                            1
                        }
                    })
                    .sum::<usize>(),
            ),
            (
                offset_of!(Frame, memory_writes),
                prefix
                    .iter()
                    .filter(|i| {
                        i.op.writes_memory()
                            && !matches!(
                                i.op,
                                Op::Movs { .. }
                                    | Op::String {
                                        repeat: Some(_),
                                        ..
                                    }
                            )
                    })
                    .count(),
            ),
            (
                offset_of!(Frame, loaded_segments),
                prefix
                    .iter()
                    .filter(|i| matches!(i.op, Op::LoadSegment { .. } | Op::PopSegment { .. }))
                    .count(),
            ),
        ] {
            if accesses != 0 {
                a.load_store(true, LHS, FRAME, offset as u32, 3);
                a.add_sub_imm(LHS, LHS, accesses as u16, true, false, false);
                a.load_store(false, LHS, FRAME, offset as u32, 3);
            }
        }
    }
}

fn prologue(a: &mut Assembler) {
    frame::emit_entry(a);
    // Only exits write a nonzero kind, and each immediately returns to Rust.
    // Linked blocks need no repeated clearing of this invocation-wide field.
    a.load_store(false, ZR, FRAME, offset_of!(Frame, exit_kind) as u32, 2);
    soft::emit_entry(a);
    a.load_store(true, FLAGS, FRAME, offset_of!(Frame, eflags) as u32, 2);
    a.load_store(true, PC, FRAME, offset_of!(Frame, eip) as u32, 2);
    unpack_flags(a);
}

/// FLAGS initially holds ordinary EFLAGS; establish our live NZCV/PF/AF form.
fn unpack_flags(a: &mut Assembler) {
    a.mov(AUXILIARY, FLAGS, false);
    a.bitfield(PARITY, FLAGS, 2, 2, false, false);
    a.mov_imm(TMP, 1, false);
    a.logical(2, PARITY, PARITY, TMP, false);
    for (source, target, width) in [(0, 29, 1), (6, 30, 2), (11, 28, 1)] {
        a.bitfield(TMP, FLAGS, source, source + width - 1, false, false);
        a.bfi32(FLAGS, TMP, target, width);
    }
}

/// One callable entry, two dispatchers and one return sequence per arena. Blocks enter
/// with resident guest state and use Frame's published stub addresses. Keeping
/// these sequences shared avoids duplicating them in every short x86 block.
/// On the firmware development run this removed about 160 MiB of cumulative
/// emission and reduced cold boot from 30.6 to 27.6 seconds, with identical guest
/// steps. Entry, dispatch and return addresses have one arena lifetime.
pub fn entry() -> Result<CompiledEntry, EmitError> {
    let mut a = Assembler::new();
    prologue(&mut a);
    a.load_store(true, LHS, FRAME, offset_of!(Frame, native_entry) as u32, 3);
    a.br(LHS);
    let done = a.label();
    let dispatch_offset = a.offset();
    super::links::emit_dispatch(&mut a, done);
    // Keep CALL targets at a separate indirect branch site from jumps and
    // unmatched returns, without adding instructions to individual edges.
    let call_dispatch_offset = a.offset();
    super::links::emit_dispatch(&mut a, done);
    a.bind(done);
    let return_offset = a.offset();
    current_flags(&mut a, LHS);
    a.load_store(false, LHS, FRAME, offset_of!(Frame, eflags) as u32, 2);
    a.load_store(false, PC, FRAME, offset_of!(Frame, eip) as u32, 2);
    // Commit progress once. X27 accounts for every completed block and the exact
    // prefix before a miss; unattempted instructions consumed no guest tick.
    a.load_store(true, LHS, FRAME, offset_of!(Frame, remaining) as u32, 3);
    a.add_sub(LHS, LHS, BUDGET, true, true, false);
    a.load_store(false, LHS, FRAME, offset_of!(Frame, completed) as u32, 3);
    a.load_store(true, RHS, FRAME, offset_of!(Frame, tsc) as u32, 3);
    a.add_sub(RHS, RHS, LHS, true, false, false);
    a.load_store(false, RHS, FRAME, offset_of!(Frame, tsc) as u32, 3);
    frame::emit_return(&mut a);
    let recheck_offset = a.offset();
    soft::emit_recheck(&mut a);
    Ok(CompiledEntry {
        words: a.finish()?,
        dispatch_offset,
        call_dispatch_offset,
        return_offset,
        recheck_offset,
    })
}

/// SUBS X27, X27, #1 at the start of a one-instruction block. The host may
/// recognize this header for an interrupt-shadow invocation with budget one;
/// an unrecognized entry simply declines that specialization.
#[cfg(all(target_arch = "aarch64", feature = "host", target_os = "linux"))]
pub(super) const ONE_INSTRUCTION_ENTRY: u32 = 0xf100_077b;

/// `block_facts` (flat segment qualifications) are
/// proven by the adapter for every entry into this
/// code, including linked edges. They remain constant throughout a native
/// invocation: a segment load exits if these qualifications change. Pass zero
/// when the adapter supplies no such specialization.
pub fn block(
    block: &Block,
    page: Option<super::links::Page>,
    block_facts: u32,
) -> Result<CompiledBlock, EmitError> {
    assert!(!block.instructions.is_empty() && block.instructions.len() < 4096);
    let count = block.instructions.len() as u16;
    let needed = flag_requirements(block);
    let ranges = super::memory::access_ranges(block, frame::PAGE_BIAS_SLOTS);
    let memory_count = block
        .instructions
        .iter()
        .filter(|i| i.op.can_exit())
        .count();
    let mut a = Assembler::with_capacity(64 + usize::from(count) * 24, 8 + usize::from(count) * 4);
    let mut faults = Vec::with_capacity(memory_count * 2);
    let mut edges = Vec::with_capacity(if page.is_some() { 3 } else { 0 });
    let mut exits = Vec::with_capacity(memory_count);
    let mut attention_exits = Vec::new();
    let done = a.label();
    let budget_exit = a.label();
    a.add_sub_imm(BUDGET, BUDGET, count, true, true, true);
    a.b_cond(Cond::Cc, budget_exit);
    let segments = core::cell::Cell::new(block_facts);
    let mut stack_end = 0;
    let mut stack_remaining = 0;
    for (index, instruction) in block.instructions.iter().enumerate() {
        let site = instruction.op.can_exit().then(|| InstructionExit {
            segments: &segments,
            range: ranges[index],
            miss: a.label(),
            eip: instruction.eip,
            steps_before: index as u16,
            repeat: repeat_control(instruction.op).map(|control| (control, instruction.next_eip)),
            division: matches!(instruction.op, Op::Divide { .. }),
        });
        if matches!(instruction.op, Op::Movs { .. } | Op::String { .. }) {
            segments.set(block_facts);
        }
        if index >= stack_end {
            if let Some(range) = super::memory::stack_range(block, index) {
                soft::stack_range(
                    &mut a,
                    range.bytes,
                    range.write,
                    site.as_ref().unwrap().guard(),
                );
                stack_end = range.end;
                stack_remaining = range.bytes;
            }
        }
        let stack_cached = index < stack_end;
        let push_offset = if stack_cached {
            super::memory::stack_width(instruction.op, true).map(|width| {
                stack_remaining -= width;
                stack_remaining
            })
        } else {
            None
        };
        match instruction.op {
            Op::Nop => {}
            Op::Mmx { opcode, reg, rm } => {
                let site = site.as_ref().unwrap();
                soft::mmx_available(&mut a, site.miss);
                if matches!(opcode, 0x7e | 0x7f) {
                    // Qualify the complete write before reading its source.
                    // In particular, a split qword exits before either dword.
                    let access = if let Value::Memory { address, bits } = rm {
                        Some(soft::prepare(&mut a, address, bits, true, site.guard()))
                    } else {
                        None
                    };
                    soft::read_mmx(&mut a, LHS, reg);
                    match rm {
                        Value::Memory { bits, .. } => {
                            let offset = access.unwrap().access(&mut a, LHS, true);
                            faults.push(FaultSite::new(
                                offset,
                                site.eip,
                                site.steps_before.into(),
                                bits / 8,
                                true,
                            ));
                            soft::mmx(&mut a, opcode, reg, ZR);
                        }
                        Value::Register(dst) if opcode == 0x7e => {
                            write_register(&mut a, dst, LHS);
                            soft::mmx(&mut a, opcode, reg, ZR);
                        }
                        Value::Register(dst) => soft::mmx(&mut a, 0x6f, dst.index, LHS),
                        Value::Immediate(_) => unreachable!(),
                    }
                } else {
                    match rm {
                        Value::Register(src) if src.bits == 64 => {
                            soft::read_mmx(&mut a, LHS, src.index)
                        }
                        _ => read_value(&mut a, LHS, rm, Some(site), &mut faults),
                    }
                    soft::mmx(&mut a, opcode, reg, LHS);
                }
            }
            Op::LoadSegment { segment, src } => {
                read_value(&mut a, LHS, src, site.as_ref(), &mut faults);
                soft::load_segment(&mut a, segment, site.as_ref().unwrap().miss);
            }
            Op::PopSegment { segment, bits } => {
                let site = site.as_ref().unwrap();
                // All operand sizes read a selector word. Descriptor validation
                // and any accessed-bit write precede the ESP commit.
                read_stack(&mut a, LHS, 16, site, &mut faults, false);
                soft::load_segment(&mut a, segment, site.miss);
                a.add_sub_imm(
                    guest_reg(4),
                    guest_reg(4),
                    u16::from(bits / 8),
                    false,
                    false,
                    false,
                );
            }
            Op::PushSegment { segment, bits } => {
                a.load_store(
                    true,
                    RESULT,
                    FRAME,
                    (offset_of!(Frame, selectors) + segment as usize * 2) as u32,
                    1,
                );
                // A dword slot reserves four bytes but stores only the selector.
                push(
                    &mut a,
                    RESULT,
                    16,
                    u16::from(bits / 8),
                    site.as_ref().unwrap(),
                    &mut faults,
                    None,
                );
            }
            Op::ReadSegment { dst, segment } => {
                // Segment loads refresh these explicit selectors. r32 is
                // zero-extended, r16 preserves its high half, memory stores 16
                // bits (v86 instructions.rs agrees).
                let access = if let Value::Memory { address, .. } = dst {
                    Some(soft::prepare(
                        &mut a,
                        address,
                        16,
                        true,
                        site.as_ref().unwrap().guard(),
                    ))
                } else {
                    None
                };
                a.load_store(
                    true,
                    RESULT,
                    FRAME,
                    (offset_of!(Frame, selectors) + segment as usize * 2) as u32,
                    1,
                );
                match dst {
                    Value::Register(dst) => write_register(&mut a, dst, RESULT),
                    Value::Memory { .. } => {
                        let offset = access.unwrap().access(&mut a, RESULT, true);
                        faults.push(FaultSite::new(
                            offset,
                            instruction.eip,
                            index as u32,
                            2,
                            true,
                        ));
                    }
                    Value::Immediate(_) => unreachable!(),
                }
            }
            Op::Direction { backward } => {
                a.load_store(true, TMP, FRAME, offset_of!(Frame, eflags) as u32, 2);
                a.mov_imm(TMP2, u64::from(backward), false);
                a.bfi32(TMP, TMP2, 10, 1);
                a.load_store(false, TMP, FRAME, offset_of!(Frame, eflags) as u32, 2);
            }
            Op::LoadFlagsByte => {
                current_flags(&mut a, RESULT);
                write_register(
                    &mut a,
                    Register {
                        index: 0,
                        shift: 8,
                        bits: 8,
                    },
                    RESULT,
                );
            }
            Op::StoreFlagsByte => {
                read_register(
                    &mut a,
                    RESULT,
                    Register {
                        index: 0,
                        shift: 8,
                        bits: 8,
                    },
                );
                a.mov(AUXILIARY, RESULT, false);
                a.bitfield(PARITY, RESULT, 2, 2, false, false);
                a.mov_imm(TMP, 1, false);
                a.logical(2, PARITY, PARITY, TMP, false);
                for (source, target, width) in [(0, 29, 1), (6, 30, 2)] {
                    a.bitfield(TMP, RESULT, source, source + width - 1, false, false);
                    a.bfi32(FLAGS, TMP, target, width);
                }
            }
            Op::Carry { value } => {
                if let Some(value) = value {
                    a.mov_imm(TMP, u64::from(value), false);
                } else {
                    a.bitfield(TMP, FLAGS, 29, 29, false, false);
                    a.mov_imm(TMP2, 1, false);
                    a.logical(2, TMP, TMP, TMP2, false);
                }
                a.bfi32(FLAGS, TMP, 29, 1);
            }
            Op::ByteSwap { dst } => {
                let reg = guest_reg(dst.index as usize);
                a.reverse_bytes32(reg, reg);
            }
            Op::ConditionalMove {
                dst,
                src,
                condition,
            } => {
                // Even a false condition reads the source and can fault.
                read_value(&mut a, RESULT, src, site.as_ref(), &mut faults);
                condition_value(&mut a, condition);
                a.add_sub_imm(ZR, TMP, 0, false, true, true);
                a.select32(
                    RESULT,
                    RESULT,
                    guest_reg(dst.index as usize),
                    if condition & 1 == 0 {
                        Cond::Ne
                    } else {
                        Cond::Eq
                    },
                );
                write_register(&mut a, dst, RESULT);
            }
            Op::SignExtendPair { bits } => {
                a.bitfield(RESULT, guest_reg(0), bits - 1, bits - 1, false, true);
                write_register(
                    &mut a,
                    Register {
                        index: 2,
                        shift: 0,
                        bits,
                    },
                    RESULT,
                );
            }
            Op::PushFlags { bits } => {
                current_flags(&mut a, RESULT);
                // Native entry excludes VM86. PUSHFD still clears RF/VM in
                // its stored image, without changing the live flags.
                if bits == 32 {
                    a.bfi32(RESULT, ZR, 16, 2);
                }
                push(
                    &mut a,
                    RESULT,
                    bits,
                    u16::from(bits / 8),
                    site.as_ref().unwrap(),
                    &mut faults,
                    push_offset,
                );
            }
            Op::PopFlags { bits } => {
                let site = site.as_ref().unwrap();
                soft::stack_guard(&mut a, site.guard());
                read_value(
                    &mut a,
                    RHS,
                    Value::Memory {
                        address: Address {
                            base: Some(4),
                            index: None,
                            scale: 0,
                            displacement: 0,
                            segment: crate::x86::cpu::SegmentReg::Ss,
                        },
                        bits,
                    },
                    Some(site),
                    &mut faults,
                );
                // Debug stepping remains an interpreter boundary. This RAM
                // read has no side effect, and ESP/flags have not committed.
                a.tbz(RHS, 8, true, site.miss);
                a.load_store(true, LHS, FRAME, offset_of!(Frame, eflags) as u32, 2);
                a.load_store(true, TMP, FRAME, offset_of!(Frame, popf_mask) as u32, 2);
                if bits == 16 {
                    a.bitfield(TMP, TMP, 0, 15, false, false);
                } else {
                    a.bfi32(RHS, ZR, 16, 1); // POPFD clears RF, irrespective of its image.
                }
                // The mask includes every arithmetic flag, so the old frame's
                // stale status bits are entirely replaced. Privileged control
                // bits keep the value established at entry or a previous POPF.
                a.logical(2, RESULT, LHS, RHS, false);
                a.logical(0, RESULT, RESULT, TMP, false);
                a.logical(2, FLAGS, LHS, RESULT, false);
                a.mov_imm(TMP, 2, false);
                a.logical(1, FLAGS, FLAGS, TMP, false);
                a.logical(0, RESULT, RESULT, FLAGS, false); // Bits that became set.
                a.load_store(false, FLAGS, FRAME, offset_of!(Frame, eflags) as u32, 2);
                unpack_flags(&mut a);
                a.add_sub_imm(
                    guest_reg(4),
                    guest_reg(4),
                    u16::from(bits / 8),
                    false,
                    false,
                    false,
                );
                // An enabled IRQ must be observed immediately after POPF,
                // even when CLI earlier in this chain disabled it only briefly.
                let attention = a.label();
                a.tbz(RESULT, 9, true, attention);
                attention_exits.push((attention, instruction.next_eip, index as u16 + 1));
            }
            Op::ClearInterrupt => {
                // POPF and CLI use the same CPL <= IOPL qualification outside
                // VM86. PVI/privilege faults stay with the shared interpreter.
                a.load_store(true, TMP, FRAME, offset_of!(Frame, popf_mask) as u32, 2);
                a.tbz(TMP, 9, false, site.as_ref().unwrap().miss);
                a.load_store(true, TMP, FRAME, offset_of!(Frame, eflags) as u32, 2);
                a.bfi32(TMP, ZR, 9, 1);
                a.load_store(false, TMP, FRAME, offset_of!(Frame, eflags) as u32, 2);
            }
            Op::SetInterrupt => {
                // As with CLI, the POPF mask proves CPL <= IOPL outside VM86.
                // PVI and privilege faults retain shared interpreter semantics.
                // Only an IF 0->1 change inhibits IRQs. v86 instructions.rs
                // runs another instruction even when IF was already set.
                a.load_store(true, TMP, FRAME, offset_of!(Frame, popf_mask) as u32, 2);
                a.tbz(TMP, 9, false, site.as_ref().unwrap().miss);
                a.load_store(true, TMP, FRAME, offset_of!(Frame, eflags) as u32, 2);
                a.bitfield(TMP2, TMP, 9, 9, false, false);
                a.mov_imm(LHS, 1, false);
                a.bfi32(TMP, LHS, 9, 1);
                a.load_store(false, TMP, FRAME, offset_of!(Frame, eflags) as u32, 2);
                // Kind 5 is attention with IF already set; kind 6 also installs
                // the shadow. Neither permits another native instruction.
                a.mov_imm(LHS, 6, false);
                a.add_sub(LHS, LHS, TMP2, false, true, false);
                a.load_store(false, LHS, FRAME, offset_of!(Frame, exit_kind) as u32, 2);
            }
            Op::String {
                repeat: Some(_), ..
            }
            | Op::Movs { repeat: true, .. } => repeat_string(
                &mut a,
                repeat_control(instruction.op).unwrap(),
                site.as_ref().unwrap(),
                &mut faults,
                instruction.next_eip,
            ),
            Op::String {
                kind,
                bits,
                segment,
                repeat: None,
            } => {
                let site = site.as_ref().unwrap();
                let index_reg = if matches!(kind, StringOp::Load | StringOp::Compare) {
                    6
                } else {
                    7
                };
                let address = Address {
                    base: Some(index_reg),
                    index: None,
                    scale: 0,
                    displacement: 0,
                    segment,
                };
                let accumulator = Register {
                    index: 0,
                    shift: 0,
                    bits,
                };
                if kind == StringOp::Store {
                    let offset = soft::prepare(&mut a, address, bits, true, site.guard()).access(
                        &mut a,
                        guest_reg(0),
                        true,
                    );
                    faults.push(FaultSite::new(
                        offset,
                        site.eip,
                        site.steps_before.into(),
                        bits / 8,
                        true,
                    ));
                } else {
                    read_value(
                        &mut a,
                        RHS,
                        Value::Memory { address, bits },
                        Some(site),
                        &mut faults,
                    );
                    if kind == StringOp::Compare {
                        a.mov(LHS, RHS, false);
                        read_value(
                            &mut a,
                            RHS,
                            Value::Memory {
                                address: Address {
                                    base: Some(7),
                                    segment: crate::x86::cpu::SegmentReg::Es,
                                    ..address
                                },
                                bits,
                            },
                            Some(site),
                            &mut faults,
                        );
                        alu(&mut a, Alu::Cmp, bits, needed[index], (LHS, RHS, RESULT));
                    } else if kind == StringOp::Load {
                        write_register(&mut a, accumulator, RHS);
                    } else {
                        read_register(&mut a, LHS, accumulator);
                        alu(&mut a, Alu::Cmp, bits, needed[index], (LHS, RHS, RESULT));
                    }
                }
                // No guest register or flag changes precede a faultable access.
                // The index wraps at address width in either direction.
                a.load_store(true, TMP, FRAME, offset_of!(Frame, eflags) as u32, 2);
                a.bitfield(TMP, TMP, 10, 10, false, false);
                a.mov_imm(TMP2, u64::from(bits / 8), false);
                a.add_shifted(TMP, ZR, TMP, (bits / 8).trailing_zeros() as u8 + 1, false);
                a.add_sub(TMP, TMP2, TMP, false, true, false);
                a.add_sub(
                    guest_reg(index_reg as usize),
                    guest_reg(index_reg as usize),
                    TMP,
                    false,
                    false,
                    false,
                );
                if kind == StringOp::Compare {
                    a.add_sub(guest_reg(7), guest_reg(7), TMP, false, false, false);
                }
            }
            Op::Movs { bits, segment, .. } => movs(
                &mut a,
                bits,
                segment,
                site.as_ref().unwrap(),
                &mut faults,
                instruction.next_eip,
            ),
            Op::Mov { dst, src } => {
                if dst.bits == 32 {
                    // A guarded LDR commits only at the actual access, so its
                    // destination can be the resident guest register directly.
                    read_value(
                        &mut a,
                        guest_reg(dst.index as usize),
                        src,
                        site.as_ref(),
                        &mut faults,
                    );
                } else {
                    read_value(&mut a, RESULT, src, site.as_ref(), &mut faults);
                    write_register(&mut a, dst, RESULT);
                }
            }
            Op::Store { address, bits, src } => {
                let site = site.as_ref().unwrap();
                let access = soft::prepare(&mut a, address, bits, true, site.guard());
                // STRB/STRH ignore the source's high bits; only AH..DH need
                // extraction. Preparing an address never changes guest GPRs.
                let source = if let Value::Register(Register {
                    index, shift: 0, ..
                }) = src
                {
                    guest_reg(index as usize)
                } else {
                    read_value(&mut a, RESULT, src, Some(site), &mut faults);
                    RESULT
                };
                let offset = access.access(&mut a, source, true);
                faults.push(FaultSite::new(
                    offset,
                    site.eip,
                    site.steps_before.into(),
                    bits / 8,
                    true,
                ));
            }
            Op::MemoryAlu {
                kind,
                address,
                bits,
                src,
                ..
            } => {
                let site = site.as_ref().unwrap();
                let access = soft::prepare(&mut a, address, bits, true, site.guard());
                let read_offset = access.access(&mut a, LHS, false);
                // RMW needs write permission at its load. No guest state is changed.
                faults.push(FaultSite::new(
                    read_offset,
                    site.eip,
                    site.steps_before.into(),
                    bits / 8,
                    true,
                ));
                let rhs = alu_operand(&mut a, RHS, src, Some(site), &mut faults);
                calculate(&mut a, kind, bits, needed[index], (LHS, rhs, RESULT));
                let store_offset = access.access(&mut a, RESULT, true);
                faults.push(FaultSite::new(
                    store_offset,
                    site.eip,
                    site.steps_before.into(),
                    bits / 8,
                    true,
                ));
                // STR preserves NZCV. Publish x86 flags only after its commit.
                finish_alu_flags(&mut a, kind, needed[index], (LHS, rhs, RESULT));
            }
            Op::Alu {
                kind: Alu::Xor,
                dst,
                src: Value::Register(src),
            } if dst == src => {
                // X86 zero idiom: the result and all status flags are known.
                // Preserve partial-register bits and any flags whose later
                // consumers need a different writer, without reading operands.
                write_register(&mut a, dst, ZR);
                let mask = needed[index];
                if mask & 0x8c1 == 0x8c1 {
                    a.mov_imm(FLAGS, 1 << 30, false);
                } else {
                    for bit in [0, 7, 11] {
                        if mask & (1 << bit) != 0 {
                            a.bfi32(FLAGS, ZR, native_flag_bit(bit), 1);
                        }
                    }
                    if mask & 0x40 != 0 {
                        a.mov_imm(TMP, 1, false);
                        a.bfi32(FLAGS, TMP, 30, 1);
                    }
                }
                if mask & 4 != 0 {
                    a.mov(PARITY, ZR, false);
                }
                if mask & 0x10 != 0 {
                    a.mov(AUXILIARY, PARITY, false);
                }
            }
            Op::Alu { kind, dst, src }
                if (kind == Alu::Cmp && src == Value::Immediate(0))
                    || (matches!(kind, Alu::Test | Alu::And | Alu::Or)
                        && src == Value::Register(dst)) =>
            {
                flags_from_value(
                    &mut a,
                    Value::Register(dst),
                    dst.bits,
                    needed[index],
                    site.as_ref(),
                    &mut faults,
                );
            }
            Op::Alu {
                kind,
                dst,
                src: Value::Immediate(value @ 0..4096),
            } if dst.bits == 32 && matches!(kind, Alu::Add | Alu::Sub | Alu::Cmp) => {
                let target = guest_reg(dst.index as usize);
                let result = if kind != Alu::Cmp { target } else { RESULT };
                arithmetic_immediate(
                    &mut a,
                    target,
                    value as u16,
                    kind != Alu::Add,
                    needed[index],
                    result,
                );
                if kind != Alu::Cmp && result != target {
                    a.mov(target, result, false);
                }
            }
            Op::Alu { kind, dst, src } => {
                let lhs = alu_operand(&mut a, LHS, Value::Register(dst), None, &mut faults);
                let rhs = alu_operand(&mut a, RHS, src, site.as_ref(), &mut faults);
                let write = !matches!(kind, Alu::Cmp | Alu::Test);
                let result = if write && dst.bits == 32 {
                    guest_reg(dst.index as usize)
                } else {
                    RESULT
                };
                alu(&mut a, kind, dst.bits, needed[index], (lhs, rhs, result));
                if write && (dst.bits != 32 || result != guest_reg(dst.index as usize)) {
                    write_register(&mut a, dst, result);
                }
            }
            Op::IncDec { dst, decrement } => {
                if dst.bits == 32 {
                    let target = guest_reg(dst.index as usize);
                    arithmetic_immediate(&mut a, target, 1, decrement, needed[index], target);
                } else {
                    let lhs = alu_operand(&mut a, LHS, Value::Register(dst), None, &mut faults);
                    a.mov_imm(RHS, 1, false);
                    alu(
                        &mut a,
                        if decrement { Alu::Sub } else { Alu::Add },
                        dst.bits,
                        needed[index],
                        (lhs, RHS, RESULT),
                    );
                    write_register(&mut a, dst, RESULT);
                }
            }
            Op::Imul { dst, lhs, rhs } => {
                // Read both inputs before touching a destination alias or flags.
                read_value(&mut a, LHS, lhs, site.as_ref(), &mut faults);
                read_value(&mut a, RHS, rhs, site.as_ref(), &mut faults);
                if dst.bits == 16 {
                    a.bitfield(LHS, LHS, 0, 15, false, true);
                    a.bitfield(RHS, RHS, 0, 15, false, true);
                }
                a.multiply_long(RESULT, LHS, RHS, true);
                if needed[index] != 0 {
                    a.bitfield(TMP, RESULT, 0, dst.bits - 1, true, true);
                    a.add_sub(ZR, RESULT, TMP, true, true, true);
                    flag_bit(&mut a, needed[index], 0, Cond::Ne);
                    flag_bit(&mut a, needed[index], 11, Cond::Ne);
                }
                write_register(&mut a, dst, RESULT);
            }
            Op::Divide { src, bits, signed } => {
                let site = site.as_ref().unwrap();
                read_value(&mut a, RHS, src, Some(site), &mut faults);
                a.cbz(RHS, false, false, site.miss);
                if bits == 8 {
                    a.bitfield(LHS, guest_reg(0), 0, 15, true, signed);
                } else {
                    read_register(
                        &mut a,
                        LHS,
                        Register {
                            index: 0,
                            shift: 0,
                            bits,
                        },
                    );
                    if bits == 32 {
                        a.add_shifted(LHS, LHS, guest_reg(2), 32, true);
                    } else {
                        read_register(
                            &mut a,
                            TMP,
                            Register {
                                index: 2,
                                shift: 0,
                                bits,
                            },
                        );
                        a.add_shifted(LHS, LHS, TMP, 16, true);
                        if signed {
                            a.bitfield(LHS, LHS, 0, 31, true, true);
                        }
                    }
                }
                if signed {
                    a.bitfield(RHS, RHS, 0, bits - 1, true, true);
                }
                a.divide64(RESULT, LHS, RHS, signed);
                a.bitfield(TMP, RESULT, 0, bits - 1, true, signed);
                a.add_sub(ZR, TMP, RESULT, true, true, true);
                a.b_cond(Cond::Ne, site.miss);
                // A64's MIN/-1 result also fails the narrow quotient check.
                // No guest register/flag changes precede either #DE condition.
                a.multiply_subtract64(TMP2, RESULT, RHS, LHS);
                write_register(
                    &mut a,
                    Register {
                        index: 0,
                        shift: 0,
                        bits,
                    },
                    RESULT,
                );
                write_register(
                    &mut a,
                    Register {
                        index: if bits == 8 { 0 } else { 2 },
                        shift: if bits == 8 { 8 } else { 0 },
                        bits,
                    },
                    TMP2,
                );
            }
            Op::BitScan { dst, src, reverse } => {
                read_value(&mut a, RHS, src, site.as_ref(), &mut faults);
                let zero = a.label();
                let complete = a.label();
                a.cbz(RHS, false, false, zero);
                if reverse {
                    a.leading_zeros32(RESULT, RHS);
                    a.mov_imm(TMP, 31, false);
                    a.add_sub(RESULT, TMP, RESULT, false, true, false);
                } else {
                    a.reverse_bits32(RESULT, RHS);
                    a.leading_zeros32(RESULT, RESULT);
                }
                write_register(&mut a, dst, RESULT);
                if needed[index] & 0x40 != 0 {
                    a.bfi32(FLAGS, ZR, 30, 1);
                }
                a.b(complete);
                a.bind(zero);
                if needed[index] & 0x40 != 0 {
                    a.mov_imm(TMP, 1, false);
                    a.bfi32(FLAGS, TMP, 30, 1);
                }
                a.bind(complete);
            }
            Op::Multiply { src, bits, signed } => {
                // Both inputs precede either output, including AH/AX/EAX/EDX
                // aliases. A faultable source cannot change outputs or flags.
                let accumulator = Register {
                    index: 0,
                    shift: 0,
                    bits,
                };
                let lhs = operand(&mut a, LHS, Value::Register(accumulator), None, &mut faults);
                let rhs = operand(&mut a, RHS, src, site.as_ref(), &mut faults);
                if signed && bits != 32 {
                    a.bitfield(lhs, lhs, 0, bits - 1, false, true);
                    a.bitfield(rhs, rhs, 0, bits - 1, false, true);
                }
                a.multiply_long(RESULT, lhs, rhs, signed);
                if needed[index] != 0 {
                    a.bitfield(TMP, RESULT, 0, bits - 1, true, signed);
                    a.add_sub(ZR, RESULT, TMP, true, true, true);
                    flag_bit(&mut a, needed[index], 0, Cond::Ne);
                    flag_bit(&mut a, needed[index], 11, Cond::Ne);
                }
                if bits == 8 {
                    a.bfi32(guest_reg(0), RESULT, 0, 16);
                } else {
                    write_register(&mut a, accumulator, RESULT);
                    a.bitfield(TMP, RESULT, bits, bits * 2 - 1, true, false);
                    write_register(
                        &mut a,
                        Register {
                            index: 2,
                            ..accumulator
                        },
                        TMP,
                    );
                }
            }
            Op::Not { dst } => {
                read_register(&mut a, LHS, dst);
                a.mov_imm(RHS, u32::MAX.into(), false);
                a.logical(2, RESULT, LHS, RHS, false);
                write_register(&mut a, dst, RESULT);
            }
            Op::Negate { dst } => {
                let rhs = alu_operand(&mut a, RHS, Value::Register(dst), None, &mut faults);
                alu(&mut a, Alu::Sub, dst.bits, needed[index], (ZR, rhs, RESULT));
                write_register(&mut a, dst, RESULT);
            }
            Op::Leave { bits } => {
                let site = site.as_ref().unwrap();
                soft::stack_guard(&mut a, site.guard());
                read_value(
                    &mut a,
                    RESULT,
                    Value::Memory {
                        address: Address {
                            base: Some(5),
                            ..Address::stack()
                        },
                        bits,
                    },
                    Some(site),
                    &mut faults,
                );
                // Use the old frame pointer for ESP before replacing BP/EBP.
                // A fault above leaves both registers and all flags untouched.
                a.add_sub_imm(
                    guest_reg(4),
                    guest_reg(5),
                    u16::from(bits / 8),
                    false,
                    false,
                    false,
                );
                write_register(
                    &mut a,
                    Register {
                        index: 5,
                        shift: 0,
                        bits,
                    },
                    RESULT,
                );
            }
            Op::Shift { dst, kind, count } => match dst {
                Value::Register(dst) => shift(&mut a, dst, kind, count, needed[index]),
                // A count-zero memory shift still reads but does not write.
                // Shared semantics handle read-only pages and precise faults.
                Value::Memory { .. } if count == Value::Immediate(0) => {
                    a.b(site.as_ref().unwrap().miss);
                }
                Value::Memory { address, bits } => {
                    let site = site.as_ref().unwrap();
                    assert!(kind < 3 || kind >= 5);
                    // Our shared model still reads memory at count zero but
                    // performs no store. Let it handle that boundary, including
                    // read-only pages, before requesting native RMW permission.
                    if matches!(count, Value::Register(_)) {
                        if kind >= 5 {
                            carry_rotate_count(&mut a, bits, count);
                        } else {
                            read_value(&mut a, RHS, count, Some(site), &mut faults);
                            a.bitfield(RHS, RHS, 0, 4, false, false);
                        }
                        a.cbz(RHS, false, false, site.miss);
                    }
                    let access = soft::prepare(&mut a, address, bits, true, site.guard());
                    let read_offset = access.access(&mut a, LHS, false);
                    faults.push(FaultSite::new(
                        read_offset,
                        site.eip,
                        site.steps_before.into(),
                        bits / 8,
                        true,
                    ));
                    if kind >= 5 {
                        carry_rotate_count(&mut a, bits, count);
                        calculate_carry_rotate(&mut a, bits, kind);
                    } else {
                        calculate_shift(&mut a, bits, kind, count, needed[index]);
                    }
                    let store_offset = access.access(&mut a, RESULT, true);
                    faults.push(FaultSite::new(
                        store_offset,
                        site.eip,
                        site.steps_before.into(),
                        bits / 8,
                        true,
                    ));
                    // Preserve all old guest flags until the RMW store commits.
                    if kind >= 5 {
                        carry_rotate_flags(&mut a, bits, kind, needed[index]);
                    } else {
                        shift_flags(&mut a, bits, kind, count, needed[index]);
                    }
                }
                Value::Immediate(_) => unreachable!(),
            },
            Op::DoubleShift {
                dst,
                src,
                right,
                count,
            } => double_shift(&mut a, dst, src, right, count, needed[index]),
            Op::CompareExchange8 { address } => {
                // The write-qualified RAM guard precedes the read; a failed
                // compare still stores the old qword. Publish ZF and EDX:EAX
                // only after that store, preserving every other status flag.
                // v86 instructions_0f.rs agrees on the write check and ZF-only
                // flags, but skips the failed writeback and sets ZF first.
                let site = site.as_ref().unwrap();
                let access = soft::prepare(&mut a, address, 64, true, site.guard());
                let offset = access.access(&mut a, LHS, false);
                faults.push(FaultSite::new(
                    offset,
                    site.eip,
                    site.steps_before.into(),
                    8,
                    true,
                ));
                a.add_shifted(RHS, guest_reg(0), guest_reg(2), 32, true);
                a.add_sub(ZR, LHS, RHS, true, true, true);
                let unequal = a.label();
                let store = a.label();
                let done = a.label();
                a.b_cond(Cond::Ne, unequal);
                a.add_shifted(RESULT, guest_reg(3), guest_reg(1), 32, true);
                a.b(store);
                a.bind(unequal);
                a.mov(RESULT, LHS, true);
                a.bind(store);
                let offset = access.access(&mut a, RESULT, true);
                faults.push(FaultSite::new(
                    offset,
                    site.eip,
                    site.steps_before.into(),
                    8,
                    true,
                ));
                if needed[index] & 0x40 != 0 {
                    a.cset(TMP, Cond::Eq);
                    a.bfi32(FLAGS, TMP, 30, 1);
                }
                a.b_cond(Cond::Eq, done);
                a.mov(guest_reg(0), LHS, false);
                a.bitfield(guest_reg(2), LHS, 32, 63, true, false);
                a.bind(done);
            }
            Op::Exchange { kind, dst, src } => {
                let bits = src.bits;
                let access = if let Value::Memory { address, .. } = dst {
                    let site = site.as_ref().unwrap();
                    let access = soft::prepare(&mut a, address, bits, true, site.guard());
                    let offset = access.access(&mut a, LHS, false);
                    faults.push(FaultSite::new(
                        offset,
                        site.eip,
                        site.steps_before.into(),
                        bits / 8,
                        true,
                    ));
                    Some(access)
                } else {
                    read_register(
                        &mut a,
                        LHS,
                        match dst {
                            Value::Register(reg) => reg,
                            _ => unreachable!(),
                        },
                    );
                    None
                };
                read_register(&mut a, RHS, src);
                let mask = needed[index];
                let saved_source = Reg::new(3);
                let stored = match kind {
                    ExchangeOp::Swap => RHS,
                    ExchangeOp::Add => {
                        calculate(&mut a, Alu::Add, bits, mask, (LHS, RHS, RESULT));
                        RESULT
                    }
                    ExchangeOp::Compare => {
                        a.mov(saved_source, RHS, false);
                        a.mov(RHS, LHS, false);
                        read_register(
                            &mut a,
                            LHS,
                            Register {
                                index: 0,
                                shift: 0,
                                bits,
                            },
                        );
                        // Equality controls the write even when output flags
                        // are dead. Narrow subtraction must truncate first.
                        calculate(&mut a, Alu::Cmp, bits, mask | 0x40, (LHS, RHS, RESULT));
                        if access.is_some() {
                            let selected = a.label();
                            a.cbz(RESULT, false, false, selected);
                            a.mov(saved_source, RHS, false); // Failed compare still writes the old value.
                            a.bind(selected);
                        }
                        saved_source
                    }
                };
                if let Some(access) = access {
                    let offset = access.access(&mut a, stored, true);
                    let site = site.as_ref().unwrap();
                    faults.push(FaultSite::new(
                        offset,
                        site.eip,
                        site.steps_before.into(),
                        bits / 8,
                        true,
                    ));
                }
                match kind {
                    ExchangeOp::Swap | ExchangeOp::Add => {
                        if kind == ExchangeOp::Add {
                            finish_alu_flags(&mut a, Alu::Add, mask, (LHS, RHS, RESULT));
                        }
                        write_register(&mut a, src, LHS);
                        if let Value::Register(dst) = dst {
                            write_register(&mut a, dst, stored);
                        }
                    }
                    ExchangeOp::Compare => {
                        finish_alu_flags(&mut a, Alu::Cmp, mask, (LHS, RHS, RESULT));
                        let failed = a.label();
                        let finished = a.label();
                        a.cbz(RESULT, false, true, failed);
                        if let Value::Register(dst) = dst {
                            write_register(&mut a, dst, saved_source);
                        }
                        a.b(finished);
                        a.bind(failed);
                        write_register(
                            &mut a,
                            Register {
                                index: 0,
                                shift: 0,
                                bits,
                            },
                            RHS,
                        );
                        a.bind(finished);
                    }
                }
            }
            Op::Extend {
                dst,
                src,
                source_bits,
                signed,
            } => {
                let target = if dst.bits == 32 {
                    guest_reg(dst.index as usize)
                } else {
                    RESULT
                };
                read_value(&mut a, target, src, site.as_ref(), &mut faults);
                if signed {
                    a.bitfield(target, target, 0, source_bits - 1, false, true);
                }
                if dst.bits != 32 {
                    write_register(&mut a, dst, target);
                }
            }
            Op::Lea { dst, address } => {
                // Address formation adds displacement before the scaled index;
                // a large displacement also occupies its destination before the
                // base is read. Write directly only if these intermediate writes
                // cannot destroy an input. Zero displacement uses one addition.
                let early_alias = address.displacement != 0
                    && (address.index == Some(dst.index)
                        || (address.base == Some(dst.index)
                            && address.displacement >= 4096
                            && address.displacement.wrapping_neg() >= 4096));
                let resident = guest_reg(dst.index as usize);
                let target = if dst.bits == 32 && !early_alias {
                    resident
                } else {
                    RESULT
                };
                let offset = super::memory::emit_address(&mut a, target, address);
                if dst.bits != 32 || offset != resident {
                    write_register(&mut a, dst, offset);
                }
            }
            Op::Compare {
                lhs,
                rhs,
                bits,
                test,
            } if (test && lhs == rhs) || (!test && rhs == Value::Immediate(0)) => {
                flags_from_value(&mut a, lhs, bits, needed[index], site.as_ref(), &mut faults);
            }
            Op::Compare {
                lhs,
                rhs,
                bits,
                test,
            } => {
                let lhs = alu_operand(&mut a, LHS, lhs, site.as_ref(), &mut faults);
                let rhs = alu_operand(&mut a, RHS, rhs, site.as_ref(), &mut faults);
                alu(
                    &mut a,
                    if test { Alu::Test } else { Alu::Cmp },
                    bits,
                    needed[index],
                    (lhs, rhs, RESULT),
                );
            }
            Op::BitTest {
                src,
                index: bit,
                bits,
                kind,
            } => {
                read_value(&mut a, RHS, bit, None, &mut faults);
                let access = if let Value::Memory { address, .. } = src {
                    let site = site.as_ref().unwrap();
                    let access = if matches!(bit, Value::Register(_)) {
                        let displacement = Reg::new(3);
                        a.bitfield(
                            displacement,
                            RHS,
                            bits.trailing_zeros() as u8,
                            bits - 1,
                            false,
                            true,
                        );
                        a.add_shifted(
                            displacement,
                            ZR,
                            displacement,
                            bits.trailing_zeros() as u8 - 3,
                            false,
                        );
                        soft::prepare_offset(
                            &mut a,
                            address,
                            bits,
                            kind != 0,
                            site.guard(),
                            displacement,
                        )
                    } else {
                        soft::prepare(&mut a, address, bits, kind != 0, site.guard())
                    };
                    let offset = access.access(&mut a, LHS, false);
                    faults.push(FaultSite::new(
                        offset,
                        site.eip,
                        site.steps_before.into(),
                        bits / 8,
                        kind != 0,
                    ));
                    Some(access)
                } else {
                    read_value(&mut a, LHS, src, None, &mut faults);
                    None
                };
                a.bitfield(RHS, RHS, 0, bits.trailing_zeros() as u8 - 1, false, false);
                if kind != 0 {
                    a.mov_imm(TMP, 1, false);
                    a.shift_reg(TMP, TMP, RHS, false, 0);
                    if kind == 2 {
                        a.clear_bits32(RESULT, LHS, TMP);
                    } else {
                        a.logical(if kind == 1 { 1 } else { 2 }, RESULT, LHS, TMP, false);
                    }
                    if let Some(access) = access {
                        let site = site.as_ref().unwrap();
                        let offset = access.access(&mut a, RESULT, true);
                        faults.push(FaultSite::new(
                            offset,
                            site.eip,
                            site.steps_before.into(),
                            bits / 8,
                            true,
                        ));
                    } else if let Value::Register(dst) = src {
                        write_register(&mut a, dst, RESULT);
                    }
                }
                if needed[index] & 1 != 0 {
                    a.shift_reg(TMP, LHS, RHS, false, 1);
                    a.bfi32(FLAGS, TMP, 29, 1);
                }
            }
            Op::SetCondition { dst, condition } => {
                let access = if let Value::Memory { address, .. } = dst {
                    Some(soft::prepare(
                        &mut a,
                        address,
                        8,
                        true,
                        site.as_ref().unwrap().guard(),
                    ))
                } else {
                    None
                };
                condition_value(&mut a, condition);
                if condition & 1 != 0 {
                    a.mov_imm(TMP2, 1, false);
                    a.logical(2, TMP, TMP, TMP2, false);
                }
                if let Value::Register(dst) = dst {
                    write_register(&mut a, dst, TMP);
                } else {
                    let offset = access.unwrap().access(&mut a, TMP, true);
                    let site = site.as_ref().unwrap();
                    faults.push(FaultSite::new(
                        offset,
                        site.eip,
                        site.steps_before.into(),
                        1,
                        true,
                    ));
                }
            }
            Op::Pop { dst } => {
                let target = if dst.bits == 32 {
                    guest_reg(dst.index as usize)
                } else {
                    RESULT
                };
                read_stack(
                    &mut a,
                    target,
                    dst.bits,
                    site.as_ref().unwrap(),
                    &mut faults,
                    stack_cached,
                );
                // POP ESP replaces the increment with its loaded value. For
                // POP SP, increment first, then merge the low half as usual.
                if dst.bits != 32 || dst.index != 4 {
                    a.add_sub_imm(
                        guest_reg(4),
                        guest_reg(4),
                        u16::from(dst.bits / 8),
                        false,
                        false,
                        false,
                    );
                }
                if dst.bits != 32 {
                    write_register(&mut a, dst, RESULT);
                }
            }
            Op::Push { src, bits } => {
                let value = match src {
                    Value::Register(register) if register.shift == 0 => {
                        guest_reg(register.index as usize)
                    }
                    _ => {
                        read_value(&mut a, RESULT, src, site.as_ref(), &mut faults);
                        RESULT
                    }
                };
                push(
                    &mut a,
                    value,
                    bits,
                    u16::from(bits / 8),
                    site.as_ref().unwrap(),
                    &mut faults,
                    push_offset,
                );
            }
            Op::Call { target, bits } => {
                let target_reg = Reg::new(14);
                if !matches!(target, Value::Immediate(_)) {
                    read_value(&mut a, target_reg, target, site.as_ref(), &mut faults);
                }
                a.mov_imm(RESULT, instruction.next_eip.into(), false);
                push(
                    &mut a,
                    RESULT,
                    bits,
                    u16::from(bits / 8),
                    site.as_ref().unwrap(),
                    &mut faults,
                    push_offset,
                );
                if !matches!(target, Value::Immediate(_)) {
                    a.mov(LHS, target_reg, false);
                }
            }
            Op::Return { bits, pop } => {
                read_stack(
                    &mut a,
                    LHS,
                    bits,
                    site.as_ref().unwrap(),
                    &mut faults,
                    stack_cached,
                );
                let adjust = u32::from(pop) + u32::from(bits / 8);
                if adjust < 4096 {
                    a.add_sub_imm(
                        guest_reg(4),
                        guest_reg(4),
                        adjust as u16,
                        false,
                        false,
                        false,
                    );
                } else {
                    a.mov_imm(TMP, adjust.into(), false);
                    a.add_sub(guest_reg(4), guest_reg(4), TMP, false, false, false);
                }
            }
            Op::IndirectJump { src } => read_value(&mut a, LHS, src, site.as_ref(), &mut faults),
            Op::Branch { .. } => {}
        }
        if matches!(instruction.op, Op::Movs { .. } | Op::String { .. }) {
            segments.set(block_facts);
        }
        if let Some(site) = site {
            exits.push(site);
        }
    }
    let last = block.instructions.last().unwrap();
    // Dynamic transfers already produced LHS; preserve it before diagnostic
    // progress counters can reuse temporaries. Static edges set PC below.
    if matches!(
        last.op,
        Op::Return { .. }
            | Op::IndirectJump { .. }
            | Op::Movs { .. }
            | Op::String {
                repeat: Some(_),
                ..
            }
            | Op::Call {
                target: Value::Register(_) | Value::Memory { .. },
                ..
            }
    ) {
        a.mov(PC, LHS, false);
    }
    if matches!(last.op, Op::LoadSegment { .. } | Op::PopSegment { .. }) {
        a.mov(TMP2, LHS, false);
    }
    emit_progress(&mut a, block, count);
    let continuation = matches!(last.op, Op::Call { .. }).then(|| a.label());
    if let Some(continuation) = continuation {
        // CALL's successful stack store leaves its full next EIP in RESULT.
        // Progress accounting preserves it, including diagnostic counters.
        super::links::push_return(&mut a, RESULT, continuation);
    }
    if matches!(last.op, Op::Return { .. }) {
        super::links::cached_return(&mut a);
    }
    match last.op {
        Op::SetInterrupt => {
            a.mov_imm(PC, last.next_eip.into(), false);
            a.b(done);
        }
        Op::LoadSegment { .. } | Op::PopSegment { .. } => {
            // Dynamic segment state is refreshed; a changed specialization
            // must retire old links in the adapter before another block runs.
            a.mov_imm(PC, last.next_eip.into(), false);
            a.tbz(TMP2, 1, true, done);
            emit_edge(&mut a, page, Some(last.next_eip), false, done, &mut edges);
        }
        Op::Branch {
            condition: Some(condition),
            target,
        } => {
            let taken = a.label();
            match condition {
                BranchCondition::EcxZero => a.cbz(guest_reg(1), false, false, taken),
                BranchCondition::Flags(condition) => {
                    let native =
                        block.instructions.len().checked_sub(2).and_then(|i| {
                            comparison_condition(block.instructions[i].op, condition)
                        });
                    if let Some(native) = native {
                        a.b_cond(native, taken);
                    } else {
                        match condition >> 1 {
                            // Packed flag bits can branch without extraction. BE/A can
                            // test the adjacent C/Z pair without normalizing a boolean.
                            0 | 1 | 2 | 4 => {
                                let bit = match condition >> 1 {
                                    0 => 28,
                                    1 => 29,
                                    2 => 30,
                                    _ => 31,
                                };
                                a.tbz(FLAGS, bit, condition & 1 == 0, taken);
                            }
                            3 => {
                                a.bitfield(TMP, FLAGS, 29, 30, false, false);
                                a.cbz(TMP, false, condition & 1 == 0, taken);
                            }
                            _ => {
                                condition_value(&mut a, condition);
                                a.cbz(TMP, false, condition & 1 == 0, taken);
                            }
                        }
                    }
                }
            }
            if let Some(offset) = page.and_then(|page| page.entry_offset(target)) {
                // Keep the local taken-edge trampoline until publication. A
                // nearby compiled destination can then be reached in one branch.
                edges.push(super::links::Edge {
                    native_offset: a.offset() as u32 - 4,
                    target_offset: offset,
                });
            }
            emit_edge(&mut a, page, Some(last.next_eip), false, done, &mut edges);
            a.bind(taken);
            emit_edge(&mut a, page, Some(target), false, done, &mut edges);
        }
        Op::Branch {
            condition: None,
            target,
        } => {
            emit_edge(&mut a, page, Some(target), false, done, &mut edges);
        }
        Op::Call {
            target: Value::Immediate(target),
            ..
        } => emit_edge(&mut a, page, Some(target), true, done, &mut edges),
        Op::Return { .. }
        | Op::IndirectJump { .. }
        | Op::Call { .. }
        | Op::Movs { .. }
        | Op::String {
            repeat: Some(_), ..
        } => {
            emit_edge(&mut a, page, None, continuation.is_some(), done, &mut edges);
        }
        _ => {
            emit_edge(&mut a, page, Some(last.next_eip), false, done, &mut edges);
        }
    }
    if let Some(continuation) = continuation {
        a.bind(continuation);
        emit_edge(&mut a, page, Some(last.next_eip), false, done, &mut edges);
    }
    let cold_word = a.offset() / 4;
    a.bind(budget_exit);
    // SUBS may have wrapped for a short budget. Restore it before returning;
    // no instruction in this block has been attempted.
    a.add_sub_imm(BUDGET, BUDGET, count, true, false, false);
    // Direct edges do not materialize an otherwise unused PC between blocks.
    // Every precise exit names its own instruction, including a budget failure.
    a.mov_imm(PC, block.instructions[0].eip.into(), false);
    a.mov_imm(LHS, 2, false);
    a.load_store(false, LHS, FRAME, offset_of!(Frame, exit_kind) as u32, 2);
    a.b(done);
    for (label, next_eip, completed) in attention_exits {
        a.bind(label);
        a.mov_imm(PC, next_eip.into(), false);
        emit_progress(&mut a, block, completed);
        a.mov_imm(LHS, 5, false);
        a.load_store(false, LHS, FRAME, offset_of!(Frame, exit_kind) as u32, 2);
        a.b(done);
    }
    // Range members can reuse an earlier guard and have no exit of their own.
    // Refill tails also branch to instruction exits, so emit them before
    // checking references. Native fault records above remain independent.
    a.flush_tails();
    for site in &exits {
        if !a.is_referenced(site.miss) {
            continue;
        }
        a.bind(site.miss);
        a.mov_imm(PC, site.eip.into(), false);
        emit_progress(&mut a, block, site.steps_before);
        let kind = if let Some((control, next_eip)) = site.repeat {
            repeat_access_counts(&mut a, control);
            a.load_store(
                true,
                TMP,
                FRAME,
                offset_of!(Frame, repeat.completed) as u32,
                2,
            );
            a.add_sub(TMP, TMP, guest_reg(1), false, true, false);
            a.load_store(
                false,
                TMP,
                FRAME,
                offset_of!(Frame, repeat.completed) as u32,
                2,
            );
            a.mov_imm(TMP, control.into(), false);
            a.load_store(
                false,
                TMP,
                FRAME,
                offset_of!(Frame, repeat.control) as u32,
                2,
            );
            a.mov_imm(TMP, next_eip.into(), false);
            a.load_store(
                false,
                TMP,
                FRAME,
                offset_of!(Frame, repeat.next_eip) as u32,
                2,
            );
            3
        } else if site.division {
            4
        } else {
            1
        };
        a.mov_imm(LHS, kind, false);
        a.load_store(false, LHS, FRAME, offset_of!(Frame, exit_kind) as u32, 2);
        a.b(done);
    }
    a.bind(done);
    a.load_store(true, LHS, FRAME, offset_of!(Frame, return_entry) as u32, 3);
    a.br(LHS);
    let (words, relocations) = a.finish_split(cold_word)?;
    for fault in &mut faults {
        fault.steps_adjust -= i32::from(count);
    }
    Ok(CompiledBlock {
        words,
        cold_word,
        relocations,
        faults,
        edges,
    })
}

/// CMP/TEST leave their ARM conditions intact through flag packing and the
/// block's progress update. An adjacent Jcc can use those conditions directly,
/// avoiding the dependency through MRS and packed FLAGS. Full guest flags are
/// still published for either successor and every precise exit. CMP x,0 uses
/// the logical lowering above, whose C is zero rather than inverted borrow.
fn comparison_condition(op: Op, condition: u8) -> Option<Cond> {
    let borrow = match op {
        Op::Compare { test, rhs, .. } => !test && rhs != Value::Immediate(0),
        Op::Alu {
            kind: Alu::Cmp,
            src,
            ..
        } => src != Value::Immediate(0),
        Op::Alu {
            kind: Alu::Test, ..
        } => false,
        _ => return None,
    };
    Some(match condition {
        0 => Cond::Vs,
        1 => Cond::Vc,
        2 => {
            if borrow {
                Cond::Cc
            } else {
                Cond::Cs
            }
        }
        3 => {
            if borrow {
                Cond::Cs
            } else {
                Cond::Cc
            }
        }
        4 => Cond::Eq,
        5 => Cond::Ne,
        6 => {
            if borrow {
                Cond::Ls
            } else {
                Cond::Eq
            }
        }
        7 => {
            if borrow {
                Cond::Hi
            } else {
                Cond::Ne
            }
        }
        8 => Cond::Mi,
        9 => Cond::Pl,
        12 => Cond::Lt,
        13 => Cond::Ge,
        14 => Cond::Le,
        15 => Cond::Gt,
        _ => return None, // PF is outside NZCV.
    })
}

fn emit_edge(
    a: &mut Assembler,
    page: Option<super::links::Page>,
    target: Option<u32>,
    call: bool,
    miss: Label,
    edges: &mut Vec<super::links::Edge>,
) {
    if let Some(offset) = page.and_then(|page| target.and_then(|eip| page.entry_offset(eip))) {
        let unresolved = a.label();
        edges.push(super::links::Edge {
            native_offset: a.offset() as u32,
            target_offset: offset,
        });
        // A call's successor is the cached continuation and hardware return PC.
        // The opcode must also survive patching to the published budget guard.
        if call {
            a.bl(unresolved);
        } else {
            a.b(unresolved);
        }
        a.tail(super::cold::Path::Edge {
            entry: unresolved,
            target: target.unwrap(),
            miss,
        });
    } else {
        if let Some(target) = target {
            a.mov_imm(PC, target.into(), false);
        }
        a.load_store(
            true,
            LHS,
            FRAME,
            if call {
                offset_of!(Frame, call_dispatch_entry)
            } else {
                offset_of!(Frame, dispatch_entry)
            } as u32,
            3,
        );
        if call {
            a.blr(LHS);
        } else {
            a.br(LHS);
        }
    }
}

#[cfg(all(test, feature = "host", target_os = "linux", target_arch = "aarch64"))]
mod tests;
