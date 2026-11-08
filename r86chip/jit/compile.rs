//! Bounded register blocks using hardware x86 arithmetic/status and RV32 words.
//!
//! Bank-1 x1..x8 hold guest registers, x16 the software EFLAGS envelope, x17 EIP and x31 the frame
//! pointer. Arithmetic status lives in the dedicated Xr86 state described in
//! sim/rv32.rs, imported/exported with XFLOAD/XFGET. x9..x15 are scratch.
//! No generated word dereferences guest memory.
//! The dispatcher must approve the complete fetch window and event budget.
//! Unsupported instructions end the block before their effects. Memory, faults,
//! interrupt shadows and privileged operations remain interpreter boundaries.
//!
//! Undefined logic AF follows the shared r86 interpreter's zero. v86 arith.rs
//! agrees on arithmetic flags, preserved INC/DEC CF and cleared logic AF.
use crate::x86::jit::decode::{self, Alu, Op, Register, Value};
use alloc::vec::Vec;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub regs: [u32; 8],
    pub flags: u32,
    pub eip: u32,
    pub executed: u32,
}
const _: () = assert!(core::mem::size_of::<Frame>() == 44);
const _: () = assert!(core::mem::offset_of!(Frame, flags) == 32);
const _: () = assert!(core::mem::offset_of!(Frame, eip) == 36);
const _: () = assert!(core::mem::offset_of!(Frame, executed) == 40);

pub struct Block {
    pub words: Vec<u32>,
    pub instructions: u32,
    pub source_bytes: usize,
}

const L: u32 = 9;
const R: u32 = 10;
const V: u32 = 11;
const T: u32 = 12;
const FLAGS: u32 = 16;
const PC: u32 = 17;
const FRAME: u32 = 31;

struct Asm(Vec<u32>);
impl Asm {
    fn i(&mut self, op: u32, d: u32, a: u32, f: u32, n: u32) {
        self.0
            .push(op | d << 7 | f << 12 | a << 15 | (n & 4095) << 20);
    }
    fn alu(&mut self, d: u32, a: u32, b: u32, f: u32, sub: bool) {
        self.0
            .push(0x33 | d << 7 | f << 12 | a << 15 | b << 20 | u32::from(sub) << 30);
    }
    fn copy(&mut self, d: u32, a: u32) {
        self.i(0x13, d, a, 0, 0);
    }
    fn literal(&mut self, d: u32, value: u32) {
        let low = ((value as i32) << 20 >> 20) as u32;
        let high = value.wrapping_sub(low);
        if high != 0 {
            self.0.push(high | d << 7 | 0x37);
        }
        if low != 0 || high == 0 {
            self.i(0x13, d, if high == 0 { 0 } else { d }, 0, low);
        }
    }
    fn shift(&mut self, d: u32, a: u32, count: u32, right: bool) {
        self.i(0x13, d, a, if right { 5 } else { 1 }, count);
    }
    fn store(&mut self, a: u32, offset: u32) {
        self.0.push(
            0x23 | (offset & 31) << 7 | 2 << 12 | FRAME << 15 | a << 20 | (offset >> 5) << 25,
        );
    }
    fn mask(&mut self, d: u32, bits: u8) {
        if bits != 32 {
            self.shift(d, d, 32 - u32::from(bits), false);
            self.shift(d, d, 32 - u32::from(bits), true);
        }
    }
    fn value(&mut self, value: Value, d: u32) {
        match value {
            Value::Register(r) => {
                self.copy(d, u32::from(r.index) + 1);
                if r.shift != 0 {
                    self.shift(d, d, u32::from(r.shift), true);
                }
                self.mask(d, r.bits);
            }
            Value::Immediate(value) => self.literal(d, value),
            Value::Memory { .. } => unreachable!("memory must end the native block"),
        }
    }
    fn write(&mut self, reg: Register, source: u32, high_source: bool) {
        let dst = u32::from(reg.index) + 1;
        if reg.bits == 32 {
            self.copy(dst, source);
            return;
        }
        // Read the old destination container and raw source together, including
        // AL/AH aliases. XMERGE leaves both committed and pending flags alone.
        self.0.push(
            0x0b | dst << 7
                | Self::lane(reg.bits, reg.shift != 0) << 12
                | dst << 15
                | source << 20
                | (0x40 | u32::from(high_source) << 4) << 25,
        );
    }
    // XALU reads the original destination container itself, selects both lanes,
    // and merges the partial result. A register operand needs no extraction or
    // software flag sequence. Immediates are narrowed by the instruction.
    fn raw(&mut self, value: Value, temp: u32) -> (u32, bool) {
        match value {
            Value::Register(r) => (u32::from(r.index) + 1, r.shift != 0),
            Value::Immediate(value) => {
                self.literal(temp, value);
                (temp, false)
            }
            Value::Memory { .. } => unreachable!("memory ends the native block"),
        }
    }
    fn lane(bits: u8, high: bool) -> u32 {
        match bits {
            32 => 0,
            16 => 1,
            8 => {
                if high {
                    3
                } else {
                    2
                }
            }
            _ => unreachable!(),
        }
    }
    fn arithmetic(
        &mut self,
        kind: Alu,
        lhs: Value,
        rhs: Value,
        bits: u8,
        dst: Option<Register>,
        preserve_carry: bool,
    ) {
        let (a, high_a) = self.raw(lhs, L);
        let (b, high_b) = self.raw(rhs, R);
        let op = match kind {
            Alu::Add => 0,
            Alu::Or => 1,
            Alu::Adc => 2,
            Alu::Sbb => 3,
            Alu::And | Alu::Test => 4,
            Alu::Sub | Alu::Cmp => 5,
            Alu::Xor => 6,
        };
        let rd = dst.map_or(0, |r| u32::from(r.index) + 1);
        // .C publishes result and status in the same word. These blocks contain
        // only qualified, fault-free guest register operations. A future memory
        // RMW must instead stage its flags until the store is irrevocable.
        let control = op | 8 | u32::from(high_b) << 4 | u32::from(preserve_carry) << 5;
        self.0.push(
            0x0b | rd << 7 | Self::lane(bits, high_a) << 12 | a << 15 | b << 20 | control << 25,
        );
    }
    fn branch_skip(&mut self, at: usize, condition: u8) {
        let distance = (self.0.len() - at) as u32 * 4;
        assert!(distance < 4096 && distance & 3 == 0);
        self.0[at] = 0x307b // XJcc, standard B-immediate, no temporary GPR
            | u32::from(condition) << 15
            | ((distance >> 11) & 1) << 7
            | ((distance >> 1) & 15) << 8
            | ((distance >> 5) & 63) << 25
            | ((distance >> 12) & 1) << 31;
    }
}

fn supported(op: Op) -> bool {
    if op.reads_memory() || op.writes_memory() {
        return false;
    }
    matches!(
        op,
        Op::Nop
            | Op::Mov { .. }
            | Op::Compare { .. }
            | Op::IncDec { .. }
            | Op::Not { .. }
            | Op::Negate { .. }
            | Op::Lea { .. }
            | Op::Branch {
                condition: None | Some(decode::BranchCondition::Flags(_)),
                ..
            }
            | Op::Alu { .. }
    )
}

/// Compile at most `limit` instructions from an already approved fetch window.
/// The returned code expects bank-1 x31 to point to a writable `Frame`.
pub fn block(bytes: &[u8], eip: u32, limit: usize) -> Option<Block> {
    let decoded = decode::block(bytes, eip, limit.min(16));
    let mut a = Asm(Vec::new());
    for reg in 0..8 {
        a.i(0x03, reg + 1, FRAME, 2, reg * 4);
    }
    a.i(0x03, FLAGS, FRAME, 2, 32);
    a.i(0x0b, 0, FLAGS, 4, 0); // XFLOAD: initialize both status values
    let mut instructions = 0;
    let mut end = eip;
    let mut branch = false;
    for instruction in decoded.instructions {
        if !supported(instruction.op) {
            break;
        }
        match instruction.op {
            Op::Nop => (),
            Op::Mov { dst, src } => {
                let (source, high) = a.raw(src, V);
                a.write(dst, source, high);
            }
            Op::Alu { kind, dst, src } => {
                let result = (!matches!(kind, Alu::Cmp | Alu::Test)).then_some(dst);
                a.arithmetic(kind, Value::Register(dst), src, dst.bits, result, false);
            }
            Op::Compare {
                lhs,
                rhs,
                bits,
                test,
            } => {
                a.arithmetic(
                    if test { Alu::Test } else { Alu::Cmp },
                    lhs,
                    rhs,
                    bits,
                    None,
                    false,
                );
            }
            Op::IncDec { dst, decrement } => {
                a.arithmetic(
                    if decrement { Alu::Sub } else { Alu::Add },
                    Value::Register(dst),
                    Value::Immediate(1),
                    dst.bits,
                    Some(dst),
                    true,
                );
            }
            Op::Not { dst } => {
                a.value(Value::Register(dst), V);
                a.i(0x13, V, V, 4, u32::MAX);
                a.write(dst, V, false);
            }
            Op::Negate { dst } => {
                let reg = u32::from(dst.index) + 1;
                a.0.push(
                    0x0b | reg << 7
                        | Asm::lane(dst.bits, dst.shift != 0) << 12
                        | reg << 15
                        | (7 | 8) << 25,
                ); // XALU.C NEG, rs2=0
            }
            Op::Lea { dst, address } => {
                a.literal(V, address.displacement);
                if let Some(base) = address.base {
                    a.alu(V, V, u32::from(base) + 1, 0, false);
                }
                if let Some(index) = address.index {
                    let index = u32::from(index) + 1;
                    if address.scale == 0 {
                        a.alu(V, V, index, 0, false);
                    } else {
                        // SH{1,2,3}ADD: accumulator already contains base+disp.
                        // Keep the guest destination untouched until addressing
                        // is complete, including destination/base/index aliases.
                        a.0.push(
                            0x20000033
                                | V << 7
                                | u32::from(address.scale) * 2 << 12
                                | index << 15
                                | V << 20,
                        );
                    }
                }
                a.write(dst, V, false);
            }
            Op::Branch { condition, target } => {
                a.literal(PC, instruction.next_eip);
                if let Some(decode::BranchCondition::Flags(c)) = condition {
                    let skip = a.0.len();
                    a.0.push(0);
                    a.literal(PC, target);
                    a.branch_skip(skip, c ^ 1);
                } else {
                    a.literal(PC, target);
                }
                branch = true;
            }
            _ => unreachable!(),
        }
        instructions += 1;
        end = instruction.next_eip;
        if branch {
            break;
        }
    }
    if instructions == 0 {
        return None;
    }
    if !branch {
        a.literal(PC, end);
    }
    for reg in 0..8 {
        a.store(reg + 1, reg * 4);
    }
    a.i(0x0b, FLAGS, FLAGS, 5, 0); // XFGET: preserve software control/system bits
    a.store(FLAGS, 32);
    a.store(PC, 36);
    a.literal(T, instructions);
    a.store(T, 40);
    a.0.push(0x0000507b); // XLEAVE 0
    Some(Block {
        words: a.0,
        instructions,
        source_bytes: end.wrapping_sub(eip) as usize,
    })
}
