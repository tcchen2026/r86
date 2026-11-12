//! Basic blocks for the resident bank-1 ABI: x8..x15 = EAX..EDI, x16 = next EIP.
//! No Cpu layout or frame pointer enters generated code. XTICK reserves a whole
//! block; runtime uses the per-instruction native offsets to refund an unfinished
//! suffix on a service/fallback. Unsupported instructions execute in the shared
//! interpreter. The caller supplies an approved, single-page CS32 fetch window.
use crate::x86::cpu::SegmentReg;
use crate::x86::jit::decode::{self, Address, Alu, BranchCondition, Op, Register, Value};
use alloc::vec::Vec;

pub const MISS: u32 = 1;
pub const SERVICE: u32 = 2;
pub const INTERPRET: u32 = 3;
pub const TICK: u32 = 0xfff;
pub const PC: u32 = 16;
/// An unexecuted source-EIP word precedes XTICK for constant-time expiry.
pub const ENTRY_OFFSET: u32 = 4;
const A: u32 = 1;
const L: u32 = 2;
const R: u32 = 3;
const V: u32 = 4;
const T: u32 = 5;
const TARGET: u32 = 6;
const RETURN: u32 = 7;
const ESP: u32 = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Site {
    pub offset: u32,
    pub eip: u32,
    pub index: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Branch,
    Unsupported(u8),
    Window,
    Limit,
}
#[derive(Clone, Copy, Debug)]
pub struct Link {
    pub start: u32,
    pub eip: u32,
}
pub struct Block {
    pub words: Vec<u32>,
    pub instructions: u32,
    pub source_bytes: usize,
    pub sites: Vec<Site>,
    pub links: Vec<Link>,
    pub end: End,
}
impl Block {
    pub fn site(&self, offset: u32) -> Site {
        let at = self.sites.partition_point(|site| site.offset <= offset);
        self.sites[at.saturating_sub(1)]
    }
}

pub fn opcode(bytes: &[u8]) -> u8 {
    opcode_bytes(bytes).first().copied().unwrap_or(0)
}
pub fn opcode_bytes(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| {
            !matches!(
                b,
                0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 | 0x66 | 0x67 | 0xf0 | 0xf2 | 0xf3
            )
        })
        .unwrap_or(bytes.len());
    &bytes[start..]
}
fn guest(r: Register) -> u32 {
    u32::from(r.index) + 8
}
fn small(value: u32) -> bool {
    (-2048..=2047).contains(&(value as i32))
}

struct Asm {
    words: Vec<u32>,
    links: Vec<Link>,
    source: u32,
    source_page: u32,
}
impl Asm {
    fn i(&mut self, op: u32, rd: u32, rs1: u32, f: u32, imm: u32) {
        self.words
            .push(op | rd << 7 | f << 12 | rs1 << 15 | (imm & 4095) << 20);
    }
    fn r(&mut self, op: u32, f: u32, k: u32, rd: u32, rs1: u32, rs2: u32) {
        self.words
            .push(op | rd << 7 | f << 12 | rs1 << 15 | rs2 << 20 | k << 25);
    }
    fn s(&mut self, op: u32, f: u32, rs1: u32, rs2: u32, imm: u32) {
        self.words.push(
            op | (imm & 31) << 7 | f << 12 | rs1 << 15 | rs2 << 20 | ((imm >> 5) & 127) << 25,
        );
    }
    fn copy(&mut self, dst: u32, src: u32) {
        if dst != src {
            self.i(0x13, dst, src, 0, 0);
        }
    }
    fn literal(&mut self, dst: u32, value: u32) {
        let low = ((value as i32) << 20 >> 20) as u32;
        let high = value.wrapping_sub(low);
        if high != 0 {
            self.words.push(high | dst << 7 | 0x37);
        }
        if low != 0 || high == 0 {
            self.i(0x13, dst, if high == 0 { 0 } else { dst }, 0, low);
        }
    }
    fn add(&mut self, dst: u32, lhs: u32, rhs: u32) {
        self.r(0x33, 0, 0, dst, lhs, rhs);
    }
    fn add_imm(&mut self, dst: u32, lhs: u32, imm: u32) {
        if small(imm) {
            self.i(0x13, dst, lhs, 0, imm);
        } else {
            self.literal(T, imm);
            self.add(dst, lhs, T);
        }
    }
    fn lane(bits: u8, high: bool) -> u32 {
        match bits {
            32 => 0,
            16 => 1,
            8 => 2 + u32::from(high),
            _ => unreachable!(),
        }
    }
    fn write(&mut self, dst: Register, src: u32, high: bool) {
        if dst.bits == 32 {
            self.copy(guest(dst), src);
        } else {
            self.r(
                0x0b,
                Self::lane(dst.bits, dst.shift != 0),
                0x40 | (u32::from(high) << 4),
                guest(dst),
                guest(dst),
                src,
            );
        }
    }
    fn raw(&mut self, value: Value, temp: u32) -> (u32, bool) {
        match value {
            Value::Register(r) => (guest(r), r.shift != 0),
            Value::Immediate(n) => {
                self.literal(temp, n);
                (temp, false)
            }
            Value::Memory { address, bits } => {
                self.load(address, bits, false, false, temp);
                (temp, false)
            }
        }
    }
    fn value(&mut self, value: Value, dst: u32) {
        let (src, high) = self.raw(value, dst);
        self.copy(dst, src);
        if let Value::Register(r) = value {
            if high {
                self.i(0x13, dst, dst, 5, 8);
            }
            if r.bits != 32 {
                let shift = 32 - u32::from(r.bits);
                self.i(0x13, dst, dst, 1, shift);
                self.i(0x13, dst, dst, 5, shift);
            }
        }
    }
    fn address(&mut self, address: Address) -> (u32, u32) {
        if address.index.is_none() && small(address.displacement) {
            return (
                address.base.map_or(0, |r| u32::from(r) + 8),
                address.displacement,
            );
        }
        if let Some(base) = address.base {
            self.add_imm(A, u32::from(base) + 8, address.displacement);
        } else {
            self.literal(A, address.displacement);
        }
        if let Some(index) = address.index {
            let index = u32::from(index) + 8;
            if address.scale == 0 {
                self.add(A, A, index);
            } else {
                self.r(0x33, u32::from(address.scale) * 2, 0x10, A, index, A);
            }
        }
        (A, 0)
    }
    fn offset(&mut self, address: Address) -> u32 {
        let (base, imm) = self.address(address);
        if imm == 0 {
            base
        } else {
            self.add_imm(A, base, imm);
            A
        }
    }
    fn load(&mut self, address: Address, bits: u8, signed: bool, write: bool, dst: u32) {
        let width = match bits {
            8 => 0,
            16 => 1,
            32 => 2,
            _ => unreachable!(),
        };
        if matches!(address.segment, SegmentReg::Ds | SegmentReg::Ss) {
            let (base, imm) = self.address(address);
            let f = if write {
                [6, 7, 3][width as usize]
            } else if width == 2 || signed {
                width
            } else {
                width + 4
            };
            self.i(
                if address.segment == SegmentReg::Ss {
                    0x47
                } else {
                    0x07
                },
                dst,
                base,
                f,
                imm,
            );
        } else {
            let base = self.offset(address);
            self.r(
                0x5b,
                width,
                address.segment as u32 | (u32::from(write) << 3),
                dst,
                base,
                0,
            );
            if signed && bits != 32 {
                self.extend(dst, bits, true);
            }
        }
    }
    fn store(&mut self, address: Address, bits: u8, src: u32) {
        let width = match bits {
            8 => 0,
            16 => 1,
            32 => 2,
            _ => unreachable!(),
        };
        if matches!(address.segment, SegmentReg::Ds | SegmentReg::Ss) {
            let (base, imm) = self.address(address);
            self.s(
                if address.segment == SegmentReg::Ss {
                    0x77
                } else {
                    0x27
                },
                width,
                base,
                src,
                imm,
            );
        } else {
            let base = self.offset(address);
            self.r(0x5b, width + 4, address.segment as u32, 0, base, src);
        }
    }
    fn extend(&mut self, reg: u32, bits: u8, signed: bool) {
        let shift = 32 - u32::from(bits);
        self.i(0x13, reg, reg, 1, shift);
        self.i(0x13, reg, reg, 5, shift | if signed { 0x400 } else { 0 });
    }
    fn arithmetic(
        &mut self,
        kind: Alu,
        lhs: u32,
        bits: u8,
        high: bool,
        rhs: Value,
        dst: u32,
        carry: bool,
    ) {
        let op = match kind {
            Alu::Add => 0,
            Alu::Or => 1,
            Alu::Adc => 2,
            Alu::Sbb => 3,
            Alu::And | Alu::Test => 4,
            Alu::Sub | Alu::Cmp => 5,
            Alu::Xor => 6,
        };
        if bits == 32 {
            if carry && rhs == Value::Immediate(1) && matches!(kind, Alu::Add | Alu::Sub) {
                self.i(
                    0x1b,
                    dst,
                    lhs,
                    7,
                    if kind == Alu::Add { 1 } else { u32::MAX },
                );
                return;
            }
            if let Value::Immediate(imm) = rhs {
                if small(imm) && !carry {
                    self.i(0x1b, dst, lhs, op, imm);
                    return;
                }
            }
        }
        let (rhs, high_rhs) = self.raw(rhs, R);
        self.r(
            0x0b,
            Self::lane(bits, high),
            op | 8 | (u32::from(high_rhs) << 4) | (u32::from(carry) << 5),
            dst,
            lhs,
            rhs,
        );
    }
    fn leave(&mut self, reason: u32) {
        self.i(0x7b, 0, 0, 5, reason);
        if reason == MISS {
            // XINFO reports this unexecuted word. High bits qualify the source
            // linear page; low bits are zero for an indirect/unpatchable exit.
            self.words.push(self.source_page);
        }
    }
    fn exit(&mut self, target: u32, reason: u32) {
        let start = self.words.len();
        if reason == MISS && target == self.source {
            self.words
                .push(jump(i64::from(ENTRY_OFFSET) - start as i64 * 4).unwrap());
            return;
        }
        self.literal(PC, target);
        if reason == MISS {
            self.links.push(Link {
                start: start as u32 * 4,
                eip: target,
            });
            self.i(0x6b, 0, PC, 0, 0);
        }
        self.leave(reason);
        if reason == MISS {
            // Direct patch address = saved PC minus this short byte distance.
            let distance = (self.words.len() - 1 - start) as u32 * 4;
            assert!(distance < 4096);
            *self.words.last_mut().unwrap() |= distance;
        }
    }
    fn branch(&mut self, at: usize, condition: u8) {
        let off = (self.words.len() - at) as u32 * 4;
        assert!(off < 4096);
        self.words[at] = 0x307b
            | u32::from(condition) << 15
            | ((off >> 12) & 1) << 31
            | ((off >> 11) & 1) << 7
            | ((off >> 5) & 63) << 25
            | ((off >> 1) & 15) << 8;
    }
}

fn supported(op: Op) -> bool {
    match op {
        Op::Nop
        | Op::Mov { .. }
        | Op::Store { .. }
        | Op::Alu { .. }
        | Op::Compare { .. }
        | Op::IncDec { .. }
        | Op::MemoryAlu { .. }
        | Op::Lea { .. }
        | Op::Extend { .. }
        | Op::Not { .. }
        | Op::Negate { .. }
        | Op::SetCondition { .. } => true,
        Op::Shift {
            dst: Value::Register(Register { bits: 32, .. }),
            kind: 0..=4,
            ..
        } => true,
        Op::Push { bits: 32, .. }
        | Op::Pop {
            dst: Register { bits: 32, .. },
        }
        | Op::Call { bits: 32, .. }
        | Op::Return { bits: 32, .. } => true,
        Op::IndirectJump {
            src: Value::Register(Register { bits: 32, .. }) | Value::Memory { bits: 32, .. },
        } => true,
        Op::Branch {
            condition: None | Some(BranchCondition::Flags(_)),
            ..
        } => true,
        _ => false,
    }
}

/// A native JAL link is valid only after runtime has qualified its target page
/// and code version. The caller retains the entire code generation until reset.
pub fn jump(offset: i64) -> Option<u32> {
    if !(-1048576..1048576).contains(&offset) || offset & 3 != 0 {
        return None;
    }
    let n = offset as u32;
    Some(
        0x6f | ((n >> 20) & 1) << 31
            | ((n >> 1) & 1023) << 21
            | ((n >> 11) & 1) << 20
            | ((n >> 12) & 255) << 12,
    )
}

pub fn block(bytes: &[u8], eip: u32, cs_base: u32, limit: usize) -> Option<Block> {
    let limit = limit.min(4095);
    let decoded = decode::block(bytes, eip, limit);
    let mut a = Asm {
        words: alloc::vec![eip, 0],
        links: Vec::new(),
        source: eip,
        source_page: cs_base.wrapping_add(eip) & !4095,
    };
    let mut sites = alloc::vec![Site {
        offset: ENTRY_OFFSET,
        eip,
        index: 0
    }];
    let mut instructions = 0;
    let mut end_eip = eip;
    let mut end = End::Window;
    let mut terminal = false;
    let mut return_stub = None;
    for instruction in decoded.instructions {
        if !supported(instruction.op) {
            end = End::Unsupported(opcode(&bytes[instruction.eip.wrapping_sub(eip) as usize..]));
            break;
        }
        sites.push(Site {
            offset: a.words.len() as u32 * 4,
            eip: instruction.eip,
            index: instructions,
        });
        match instruction.op {
            Op::Nop => (),
            Op::Mov {
                dst,
                src: Value::Memory { address, bits },
            } if dst.bits == 32 => a.load(address, bits, false, false, guest(dst)),
            Op::Mov { dst, src } => {
                let (src, high) = a.raw(src, V);
                a.write(dst, src, high);
            }
            Op::Store { address, bits, src } => {
                let (src, high) = a.raw(src, V);
                let source = if high {
                    a.i(0x13, V, src, 5, 8);
                    V
                } else {
                    src
                };
                a.store(address, bits, source);
            }
            Op::Alu { kind, dst, src } => a.arithmetic(
                kind,
                guest(dst),
                dst.bits,
                dst.shift != 0,
                src,
                if matches!(kind, Alu::Cmp | Alu::Test) {
                    0
                } else {
                    guest(dst)
                },
                false,
            ),
            Op::Compare {
                lhs,
                rhs,
                bits,
                test,
            } => {
                let (left, high) = a.raw(lhs, L);
                a.arithmetic(
                    if test { Alu::Test } else { Alu::Cmp },
                    left,
                    bits,
                    high,
                    rhs,
                    0,
                    false,
                );
            }
            Op::MemoryAlu {
                kind,
                address,
                bits,
                src,
                preserve_carry,
            } => {
                a.load(address, bits, false, true, L);
                a.arithmetic(kind, L, bits, false, src, V, preserve_carry);
                // The write-intent load proved this exact store; no proof or
                // context operation can intervene, so committing flags is safe.
                a.store(address, bits, V);
            }
            Op::IncDec { dst, decrement } => a.arithmetic(
                if decrement { Alu::Sub } else { Alu::Add },
                guest(dst),
                dst.bits,
                dst.shift != 0,
                Value::Immediate(1),
                guest(dst),
                true,
            ),
            Op::Not { dst } => {
                // Plain RV arithmetic leaves guest status unchanged. XMERGE
                // preserves the other lanes, including NOT AH/CH/DH/BH.
                a.i(0x13, V, guest(dst), 4, u32::MAX);
                a.write(dst, V, dst.shift != 0);
            }
            Op::Negate { dst } => {
                a.arithmetic(Alu::Sub, 0, dst.bits, false, Value::Register(dst), V, false);
                a.write(dst, V, false);
            }
            Op::Extend {
                dst,
                src: Value::Memory { address, .. },
                source_bits,
                signed,
            } => {
                let result = if dst.bits == 32 { guest(dst) } else { V };
                a.load(address, source_bits, signed, false, result);
                if dst.bits != 32 {
                    a.write(dst, V, false);
                }
            }
            Op::Extend {
                dst,
                src,
                source_bits,
                signed,
            } => {
                a.value(src, V);
                a.extend(V, source_bits, signed);
                a.write(dst, V, false);
            }
            Op::Lea { dst, address } => {
                let src = a.offset(address);
                a.write(dst, src, false);
            }
            Op::Shift {
                dst: Value::Register(dst),
                kind,
                count,
            } => {
                let kind = [0, 1, 2, 4, 3][kind as usize]; // decoder ROR/ROL order
                match count {
                    Value::Immediate(n) => a.r(0x3b, kind, 1, guest(dst), guest(dst), n & 31),
                    Value::Register(reg) => a.r(0x3b, kind, 0, guest(dst), guest(dst), guest(reg)),
                    _ => unreachable!(),
                }
            }
            Op::SetCondition {
                dst: Value::Register(dst),
                condition,
            } => {
                a.i(0x0b, V, u32::from(condition), 7, 0);
                a.write(dst, V, false);
            }
            Op::SetCondition {
                dst: Value::Memory { address, bits },
                condition,
            } => {
                a.i(0x0b, V, u32::from(condition), 7, 0);
                a.store(address, bits, V);
            }
            Op::Push { src, .. } => {
                let (source, _) = a.raw(src, V);
                a.s(0x77, 6, ESP, source, (-4i32) as u32);
            }
            Op::Pop { dst } => {
                a.load(Address::stack(), 32, false, false, guest(dst));
                if dst.index != 4 {
                    a.add_imm(ESP, ESP, 4);
                }
            }
            Op::Call { target, .. } => {
                if !matches!(target, Value::Immediate(_)) {
                    a.value(target, TARGET); // capture even CALL ESP before the push
                }
                a.literal(RETURN, instruction.next_eip);
                let push = a.words.len();
                a.s(0x77, 7, ESP, RETURN, 0);
                return_stub = Some((push, instruction.next_eip));
                if let Value::Immediate(target) = target {
                    a.exit(target, MISS);
                } else {
                    a.copy(PC, TARGET);
                    a.i(0x6b, 0, PC, 0, 0);
                    a.leave(MISS);
                }
                terminal = true;
            }
            Op::Return { pop, .. } => {
                a.load(Address::stack(), 32, false, false, PC);
                a.add_imm(ESP, ESP, 4 + u32::from(pop));
                a.i(0x6b, 0, PC, 0, 1);
                a.leave(MISS);
                terminal = true;
            }
            Op::IndirectJump { src } => {
                a.value(src, PC);
                a.i(0x6b, 0, PC, 0, 0);
                a.leave(MISS);
                terminal = true;
            }
            Op::Branch { condition, target } => {
                if let Some(BranchCondition::Flags(c)) = condition {
                    let at = a.words.len();
                    a.words.push(0);
                    a.exit(instruction.next_eip, MISS);
                    a.branch(at, c);
                }
                a.exit(target, MISS);
                terminal = true;
            }
            _ => unreachable!(),
        }
        instructions += 1;
        end_eip = instruction.next_eip;
        if terminal {
            end = End::Branch;
            break;
        }
    }
    if instructions == 0 {
        return None;
    }
    let source_bytes = end_eip.wrapping_sub(eip) as usize;
    if !terminal {
        if instructions as usize == limit {
            end = End::Limit;
        } else if source_bytes < bytes.len() {
            end = End::Unsupported(opcode(&bytes[source_bytes..]));
        }
        sites.push(Site {
            offset: a.words.len() as u32 * 4,
            eip: end_eip,
            index: instructions,
        });
        a.exit(
            end_eip,
            if matches!(end, End::Unsupported(_)) {
                INTERPRET
            } else {
                MISS
            },
        );
    }
    if let Some((push, return_eip)) = return_stub {
        let distance = a.words.len() - push;
        assert!(distance < 2048);
        a.words[push] = 0x77
            | ((distance as u32 & 31) << 7)
            | 7 << 12
            | ESP << 15
            | RETURN << 20
            | (distance as u32 >> 5) << 25;
        a.exit(return_eip, MISS);
    }
    a.words[1] = 0x6b | 4 << 12 | instructions << 20;
    Some(Block {
        words: a.words,
        instructions,
        source_bytes,
        sites,
        links: a.links,
        end,
    })
}
