//! Bounded x86 block decoding, independent of host memory representation.
//!
//! The caller supplies bytes from an already permitted, side-effect-free fetch
//! window. A missing byte or unsupported encoding ends the block *before* that
//! instruction; decoding never performs a page walk or delivers a guest fault.
//! The interpreter handles that boundary with its ordinary fetch/fault ordering.
//! A decoded branch ends the block. No speculative target fetch is performed.
//!
//! Integer operations use 32-bit addressing with byte/high-byte and operand-size
//! overrides. Memory operands carry x86 effective addresses and segments, never
//! software-TLB entries or native pointers. The memory adapter decides whether a
//! store can commit natively or must leave before the attempted instruction.

use crate::x86::cpu::SegmentReg;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Address {
    pub base: Option<u8>,
    pub index: Option<u8>,
    pub scale: u8,
    pub displacement: u32,
    pub segment: SegmentReg,
}

impl Address {
    pub const fn stack() -> Self {
        Self {
            base: Some(4),
            index: None,
            scale: 0,
            displacement: 0,
            segment: SegmentReg::Ss,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Register {
    pub index: u8,
    pub shift: u8,
    pub bits: u8,
}

impl Register {
    fn encoded(index: u8, bits: u8) -> Self {
        Self {
            index: if bits == 8 { index & 3 } else { index },
            shift: if bits == 8 && index >= 4 { 8 } else { 0 },
            bits,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Value {
    Register(Register),
    Immediate(u32),
    Memory { address: Address, bits: u8 },
}

impl Value {
    fn register(self) -> Option<Register> {
        match self {
            Self::Register(reg) => Some(reg),
            _ => None,
        }
    }

    pub fn is_memory(self) -> bool {
        matches!(self, Self::Memory { .. })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Alu {
    Add,
    Or,
    Adc,
    Sbb,
    And,
    Sub,
    Xor,
    Cmp,
    Test,
}

impl Alu {
    fn group(index: u8) -> Option<Self> {
        Some(match index {
            0 => Self::Add,
            1 => Self::Or,
            2 => Self::Adc,
            3 => Self::Sbb,
            4 => Self::And,
            5 => Self::Sub,
            6 => Self::Xor,
            7 => Self::Cmp,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StringOp {
    Load,
    Store,
    Scan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExchangeOp {
    Swap,
    Add,
    Compare,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BranchCondition {
    Flags(u8),
    EcxZero,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Op {
    Nop,
    /// Unprefixed MMX. A 64-bit register operand names MM0..MM7; MOVD uses
    /// a 32-bit GPR. Immediate shifts use the equivalent binary opcode/count.
    Mmx {
        opcode: u8,
        reg: u8,
        rm: Value,
    },
    Direction {
        backward: bool,
    },
    SignExtendPair {
        bits: u8,
    },
    PushFlags {
        bits: u8,
    },
    PopFlags {
        bits: u8,
    },
    LoadFlagsByte,  // LAHF: arithmetic status into AH, without changing flags.
    StoreFlagsByte, // SAHF: AH into SF/ZF/AF/PF/CF; OF remains unchanged.
    Carry {
        // None complements CF; Some clears or sets it.
        value: Option<bool>,
    },
    ClearInterrupt,
    SetInterrupt,
    ReadSegment {
        dst: Value,
        segment: SegmentReg,
    },
    LoadSegment {
        segment: SegmentReg,
        src: Value,
    },
    Mov {
        dst: Register,
        src: Value,
    },
    ConditionalMove {
        dst: Register,
        src: Value,
        condition: u8,
    },
    ByteSwap {
        dst: Register,
    },
    Store {
        address: Address,
        bits: u8,
        src: Value,
    },
    MemoryAlu {
        kind: Alu,
        address: Address,
        bits: u8,
        src: Value,
        preserve_carry: bool,
    },
    Alu {
        kind: Alu,
        dst: Register,
        src: Value,
    },
    IncDec {
        dst: Register,
        decrement: bool,
    },
    Imul {
        dst: Register,
        lhs: Value,
        rhs: Value,
    },
    Multiply {
        src: Value,
        bits: u8,
        signed: bool,
    },
    Divide {
        src: Value,
        bits: u8,
        signed: bool,
    },
    BitScan {
        dst: Register,
        src: Value,
        reverse: bool,
    },
    Not {
        dst: Register,
    },
    Negate {
        dst: Register,
    },
    Shift {
        dst: Value,
        // 0: SHL/SAL, 1: SHR, 2: SAR, 3: ROR, 4: ROL, 5: RCL, 6: RCR.
        kind: u8,
        count: Value,
    },
    DoubleShift {
        dst: Register,
        src: Register,
        right: bool,
        count: Value,
    },
    Exchange {
        kind: ExchangeOp,
        dst: Value,
        src: Register,
    },
    CompareExchange8 {
        address: Address,
    },
    Extend {
        dst: Register,
        src: Value,
        source_bits: u8,
        signed: bool,
    },
    Lea {
        dst: Register,
        address: Address,
    },
    Compare {
        lhs: Value,
        rhs: Value,
        bits: u8,
        test: bool,
    },
    BitTest {
        src: Value,
        index: Value,
        bits: u8,
        kind: u8, // BT, BTS, BTR, BTC.
    },
    SetCondition {
        dst: Value,
        condition: u8,
    },
    Movs {
        bits: u8,
        repeat: bool,
        segment: SegmentReg,
    },
    String {
        kind: StringOp,
        bits: u8,
        segment: SegmentReg,
        repeat: Option<bool>, // REPE (true) or REPNE; ignored ZF for LODS/STOS.
    },
    Pop {
        dst: Register,
    },
    Leave {
        bits: u8,
    },
    Push {
        src: Value,
        bits: u8,
    },
    Call {
        target: Value,
        bits: u8,
    },
    Return {
        bits: u8,
        pop: u16,
    },
    IndirectJump {
        src: Value,
    },
    Branch {
        condition: Option<BranchCondition>,
        target: u32,
    },
}

impl Op {
    pub fn reads_memory(self) -> bool {
        match self {
            Self::Mmx { opcode, rm, .. } => !matches!(opcode, 0x7e | 0x7f) && rm.is_memory(),
            Self::Mov { src, .. }
            | Self::ConditionalMove { src, .. }
            | Self::LoadSegment { src, .. }
            | Self::Alu { src, .. }
            | Self::Extend { src, .. }
            | Self::BitTest { src, .. }
            | Self::Multiply { src, .. }
            | Self::Divide { src, .. }
            | Self::BitScan { src, .. }
            | Self::IndirectJump { src } => src.is_memory(),
            Self::Compare { lhs, rhs, .. } => lhs.is_memory() || rhs.is_memory(),
            Self::Imul { lhs, rhs, .. } => lhs.is_memory() || rhs.is_memory(),
            Self::MemoryAlu { .. } | Self::Movs { .. } | Self::CompareExchange8 { .. } => true,
            Self::String {
                kind: StringOp::Load | StringOp::Scan,
                ..
            } => true,
            Self::Shift { dst, .. } => dst.is_memory(),
            Self::Exchange { dst, .. } => dst.is_memory(),
            Self::Push { src, .. } => src.is_memory(),
            Self::Call { target, .. } => target.is_memory(),
            Self::Pop { .. } | Self::PopFlags { .. } | Self::Return { .. } | Self::Leave { .. } => {
                true
            }
            _ => false,
        }
    }

    pub fn writes_memory(self) -> bool {
        matches!(
            self,
            Self::Mmx {
                opcode: 0x7e | 0x7f,
                rm: Value::Memory { .. },
                ..
            } | Self::Shift {
                dst: Value::Memory { .. },
                ..
            } | Self::Exchange {
                dst: Value::Memory { .. },
                ..
            } | Self::SetCondition {
                dst: Value::Memory { .. },
                ..
            } | Self::Store { .. }
                | Self::ReadSegment {
                    dst: Value::Memory { .. },
                    ..
                }
                | Self::BitTest {
                    src: Value::Memory { .. },
                    kind: 1..=3,
                    ..
                }
                | Self::CompareExchange8 { .. }
                | Self::MemoryAlu { .. }
                | Self::Movs { .. }
                | Self::String {
                    kind: StringOp::Store,
                    ..
                }
                | Self::PushFlags { .. }
                | Self::Push { .. }
                | Self::Call { .. }
        )
    }

    pub fn accesses_memory(self) -> bool {
        self.reads_memory() || self.writes_memory()
    }

    pub fn can_exit(self) -> bool {
        self.accesses_memory()
            || matches!(
                self,
                Self::Mmx { .. }
                    | Self::Divide { .. }
                    | Self::ClearInterrupt
                    | Self::SetInterrupt
                    | Self::LoadSegment { .. }
            )
    }

    fn mov_to(dst: Value, src: Value) -> Self {
        match dst {
            Value::Register(dst) => Self::Mov { dst, src },
            Value::Memory { address, bits } => Self::Store { address, bits, src },
            Value::Immediate(_) => unreachable!(),
        }
    }

    fn alu_to(kind: Alu, dst: Value, src: Value) -> Self {
        match dst {
            Value::Register(dst) => Self::Alu { kind, dst, src },
            Value::Memory { address, bits } => Self::MemoryAlu {
                kind,
                address,
                bits,
                src,
                preserve_carry: false,
            },
            Value::Immediate(_) => unreachable!(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Instruction {
    pub eip: u32,
    pub next_eip: u32,
    pub op: Op,
}

#[derive(Debug)]
pub struct Block {
    pub instructions: Vec<Instruction>,
    pub byte_len: usize,
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Option<u8> {
        let value = *self.bytes.get(self.at)?;
        self.at += 1;
        Some(value)
    }

    fn immediate(&mut self, bits: u8) -> Option<u32> {
        let mut value = 0;
        for shift in (0..bits).step_by(8) {
            value |= u32::from(self.byte()?) << shift;
        }
        Some(value)
    }

    fn modrm(&mut self, bits: u8) -> Option<(Register, Register, u8)> {
        let byte = self.byte()?;
        if byte & 0xc0 != 0xc0 {
            return None;
        }
        let group = (byte >> 3) & 7;
        Some((
            Register::encoded(byte & 7, bits),
            Register::encoded(group, bits),
            group,
        ))
    }

    fn read_modrm(
        &mut self,
        bits: u8,
        segment: Option<SegmentReg>,
    ) -> Option<(Value, Register, u8)> {
        let byte = self.byte()?;
        let group = (byte >> 3) & 7;
        let reg = Register::encoded(group, bits);
        if byte & 0xc0 == 0xc0 {
            return Some((
                Value::Register(Register::encoded(byte & 7, bits)),
                reg,
                group,
            ));
        }
        let mode = byte >> 6;
        let mut base = Some(byte & 7);
        let mut index = None;
        let mut scale = 0;
        if base == Some(4) {
            let sib = self.byte()?;
            base = Some(sib & 7);
            if (sib >> 3) & 7 != 4 {
                index = Some((sib >> 3) & 7);
            }
            scale = sib >> 6;
        }
        let displacement = if mode == 0 && base == Some(5) {
            base = None;
            self.immediate(32)?
        } else if mode == 1 {
            self.byte()? as i8 as i32 as u32
        } else if mode == 2 {
            self.immediate(32)?
        } else {
            0
        };
        let segment = segment.unwrap_or(if matches!(base, Some(4 | 5)) {
            SegmentReg::Ss
        } else {
            SegmentReg::Ds
        });
        Some((
            Value::Memory {
                address: Address {
                    base,
                    index,
                    scale,
                    displacement,
                    segment,
                },
                bits,
            },
            reg,
            group,
        ))
    }

    fn instruction(&mut self, eip: u32) -> Option<Instruction> {
        let start = self.at;
        let mut bits = 32;
        let mut segment = None;
        let mut repeat = false;
        let mut repeat_equal = true;
        let mut locked = false;
        let opcode = loop {
            // Never accept a guest instruction longer than the architectural 15.
            if self.at - start == 15 {
                return None;
            }
            match self.byte()? {
                0x66 if bits == 32 => bits = 16,
                0x26 => segment = Some(SegmentReg::Es),
                0x2e => segment = Some(SegmentReg::Cs),
                0x36 => segment = Some(SegmentReg::Ss),
                0x3e => segment = Some(SegmentReg::Ds),
                0x64 => segment = Some(SegmentReg::Fs),
                0x65 => segment = Some(SegmentReg::Gs),
                0xf0 => locked = true,
                prefix @ (0xf2 | 0xf3) => {
                    repeat = true;
                    repeat_equal = prefix == 0xf3;
                }
                op => break op,
            }
        };
        if repeat && !matches!(opcode, 0xa4 | 0xa5 | 0xaa..=0xaf) {
            return None;
        }
        let op = match opcode {
            0xfc | 0xfd => Op::Direction {
                backward: opcode == 0xfd,
            },
            0x98 => Op::Extend {
                dst: Register::encoded(0, bits),
                src: Value::Register(Register::encoded(0, bits / 2)),
                source_bits: bits / 2,
                signed: true,
            },
            0x99 => Op::SignExtendPair { bits },
            0x9c => Op::PushFlags { bits },
            0x9d => Op::PopFlags { bits },
            0x9e => Op::StoreFlagsByte,
            0x9f => Op::LoadFlagsByte,
            0xf5 => Op::Carry { value: None },
            0xf8 | 0xf9 => Op::Carry {
                value: Some(opcode == 0xf9),
            },
            0xfa => Op::ClearInterrupt,
            0xfb => Op::SetInterrupt,
            0xaa..=0xaf => Op::String {
                kind: match opcode & !1 {
                    0xaa => StringOp::Store,
                    0xac => StringOp::Load,
                    _ => StringOp::Scan,
                },
                bits: if opcode & 1 == 0 { 8 } else { bits },
                repeat: repeat.then_some(repeat_equal),
                segment: if matches!(opcode, 0xac | 0xad) {
                    segment.unwrap_or(SegmentReg::Ds)
                } else {
                    SegmentReg::Es
                },
            },
            0xa4 | 0xa5 => Op::Movs {
                bits: if opcode == 0xa4 { 8 } else { bits },
                repeat,
                segment: segment.unwrap_or(SegmentReg::Ds),
            },
            0x90 => Op::Nop,
            0x91..=0x97 => Op::Exchange {
                kind: ExchangeOp::Swap,
                dst: Value::Register(Register::encoded(0, bits)),
                src: Register::encoded(opcode & 7, bits),
            },
            0xb0..=0xbf => {
                let bits = if opcode < 0xb8 { 8 } else { bits };
                Op::Mov {
                    dst: Register::encoded(opcode & 7, bits),
                    src: Value::Immediate(self.immediate(bits)?),
                }
            }
            0x88..=0x8b => {
                let bits = if opcode & 1 == 0 { 8 } else { bits };
                let (rm, reg, _) = self.read_modrm(bits, segment)?;
                if opcode & 2 == 0 {
                    Op::mov_to(rm, Value::Register(reg))
                } else {
                    Op::Mov { dst: reg, src: rm }
                }
            }
            0xa2 | 0xa3 => {
                let bits = if opcode == 0xa2 { 8 } else { bits };
                let address = Address {
                    base: None,
                    index: None,
                    scale: 0,
                    displacement: self.immediate(32)?,
                    segment: segment.unwrap_or(SegmentReg::Ds),
                };
                Op::Store {
                    address,
                    bits,
                    src: Value::Register(Register::encoded(0, bits)),
                }
            }
            0xa0 | 0xa1 => {
                let bits = if opcode == 0xa0 { 8 } else { bits };
                let address = Address {
                    base: None,
                    index: None,
                    scale: 0,
                    displacement: self.immediate(32)?,
                    segment: segment.unwrap_or(SegmentReg::Ds),
                };
                Op::Mov {
                    dst: Register::encoded(0, bits),
                    src: Value::Memory { address, bits },
                }
            }
            0x8c => {
                let (mut dst, _, group) = self.read_modrm(bits, segment)?;
                if group >= 6 {
                    return None;
                }
                // Register forms obey operand size; memory is always a word.
                if let Value::Memory { bits, .. } = &mut dst {
                    *bits = 16;
                }
                Op::ReadSegment {
                    dst,
                    segment: SegmentReg::from_modrm_reg(group),
                }
            }
            0x8e => {
                // The source is always a word, including the r32 encoding.
                // CS is invalid; SS retains its ordinary interrupt shadow.
                let (src, _, group) = self.read_modrm(16, segment)?;
                if !matches!(group, 0 | 3..=5) {
                    return None;
                }
                Op::LoadSegment {
                    segment: SegmentReg::from_modrm_reg(group),
                    src,
                }
            }
            0x8d => {
                let (Value::Memory { address, .. }, dst, _) = self.read_modrm(bits, segment)?
                else {
                    return None;
                };
                Op::Lea { dst, address }
            }
            0x58..=0x5f => Op::Pop {
                dst: Register::encoded(opcode & 7, bits),
            },
            0x50..=0x57 => Op::Push {
                src: Value::Register(Register::encoded(opcode & 7, bits)),
                bits,
            },
            0x68 | 0x6a => Op::Push {
                src: Value::Immediate(if opcode == 0x6a {
                    self.byte()? as i8 as i32 as u32
                } else {
                    self.immediate(bits)?
                }),
                bits,
            },
            0x69 | 0x6b => {
                let (lhs, dst, _) = self.read_modrm(bits, segment)?;
                Op::Imul {
                    dst,
                    lhs,
                    rhs: Value::Immediate(if opcode == 0x6b {
                        self.byte()? as i8 as i32 as u32
                    } else {
                        self.immediate(bits)?
                    }),
                }
            }
            0xe8 => {
                let relative = self.immediate(bits)?;
                let relative = if bits == 16 {
                    relative as i16 as i32
                } else {
                    relative as i32
                };
                let target = eip
                    .wrapping_add((self.at - start) as u32)
                    .wrapping_add_signed(relative)
                    & if bits == 16 { 0xffff } else { u32::MAX };
                Op::Call {
                    target: Value::Immediate(target),
                    bits,
                }
            }
            0xc3 | 0xc2 => Op::Return {
                bits,
                pop: if opcode == 0xc2 {
                    self.immediate(16)? as u16
                } else {
                    0
                },
            },
            0xc6 | 0xc7 => {
                let bits = if opcode == 0xc6 { 8 } else { bits };
                let (dst, _, group) = self.read_modrm(bits, segment)?;
                if group != 0 {
                    return None;
                }
                Op::mov_to(dst, Value::Immediate(self.immediate(bits)?))
            }
            0x00..=0x3d if opcode & 7 <= 5 => {
                let kind = Alu::group(opcode >> 3)?;
                let bits = if opcode & 1 == 0 { 8 } else { bits };
                if opcode & 7 >= 4 {
                    Op::Alu {
                        kind,
                        dst: Register::encoded(0, bits),
                        src: Value::Immediate(self.immediate(bits)?),
                    }
                } else {
                    let (rm, reg, _) = self.read_modrm(bits, segment)?;
                    if kind == Alu::Cmp && opcode & 2 == 0 {
                        Op::Compare {
                            lhs: rm,
                            rhs: Value::Register(reg),
                            bits,
                            test: false,
                        }
                    } else {
                        if opcode & 2 == 0 {
                            Op::alu_to(kind, rm, Value::Register(reg))
                        } else {
                            Op::Alu {
                                kind,
                                dst: reg,
                                src: rm,
                            }
                        }
                    }
                }
            }
            0x80 | 0x81 | 0x83 => {
                let bits = if opcode == 0x80 { 8 } else { bits };
                let (dst, _, group) = self.read_modrm(bits, segment)?;
                let kind = Alu::group(group)?;
                let immediate = if opcode == 0x83 {
                    self.byte()? as i8 as i32 as u32
                } else {
                    self.immediate(bits)?
                };
                if kind == Alu::Cmp {
                    Op::Compare {
                        lhs: dst,
                        rhs: Value::Immediate(immediate),
                        bits,
                        test: false,
                    }
                } else {
                    Op::alu_to(kind, dst, Value::Immediate(immediate))
                }
            }
            0x84 | 0x85 => {
                let bits = if opcode == 0x84 { 8 } else { bits };
                let (lhs, src, _) = self.read_modrm(bits, segment)?;
                Op::Compare {
                    lhs,
                    rhs: Value::Register(src),
                    bits,
                    test: true,
                }
            }
            0xa8 | 0xa9 => {
                let bits = if opcode == 0xa8 { 8 } else { bits };
                Op::Alu {
                    kind: Alu::Test,
                    dst: Register::encoded(0, bits),
                    src: Value::Immediate(self.immediate(bits)?),
                }
            }
            0xf6 | 0xf7 => {
                let bits = if opcode == 0xf6 { 8 } else { bits };
                let (dst, _, group) = self.read_modrm(bits, segment)?;
                match group {
                    0 => Op::Compare {
                        test: true,
                        bits,
                        lhs: dst,
                        rhs: Value::Immediate(self.immediate(bits)?),
                    },
                    2 => Op::Not {
                        dst: dst.register()?,
                    },
                    3 => Op::Negate {
                        dst: dst.register()?,
                    },
                    4 | 5 => Op::Multiply {
                        src: dst,
                        bits,
                        signed: group == 5,
                    },
                    6 | 7 => Op::Divide {
                        src: dst,
                        bits,
                        signed: group == 7,
                    },
                    _ => return None,
                }
            }
            0x40..=0x4f => Op::IncDec {
                dst: Register::encoded(opcode & 7, bits),
                decrement: opcode >= 0x48,
            },
            0xc9 => Op::Leave { bits },
            0xc0 | 0xc1 | 0xd0..=0xd3 => {
                let bits = if opcode & 1 == 0 { 8 } else { bits };
                let (dst, _, group) = self.read_modrm(bits, segment)?;
                let kind = match group {
                    0 => 4,
                    1 => 3,
                    2 => 5,
                    3 => 6,
                    4 | 6 => 0,
                    5 => 1,
                    7 => 2,
                    _ => return None,
                };
                if dst.is_memory() && matches!(kind, 3 | 4) {
                    return None;
                }
                let mut count = match opcode {
                    0xc0 | 0xc1 => Value::Immediate(u32::from(self.byte()? & 31)),
                    0xd0 | 0xd1 => Value::Immediate(1),
                    _ => Value::Register(Register::encoded(1, 8)),
                };
                // Normalize immediate carry rotates here so flag liveness also
                // recognizes whole 9/17-bit turns as no-ops.
                if kind >= 5 && bits < 32 {
                    if let Value::Immediate(ref mut count) = count {
                        *count %= u32::from(bits + 1);
                    }
                }
                Op::Shift { dst, kind, count }
            }
            0xfe | 0xff => {
                let bits = if opcode == 0xfe { 8 } else { bits };
                let (dst, _, group) = self.read_modrm(bits, segment)?;
                if opcode == 0xff && group == 4 {
                    Op::IndirectJump { src: dst }
                } else if opcode == 0xff && group == 2 {
                    Op::Call { target: dst, bits }
                } else if opcode == 0xff && group == 6 {
                    Op::Push { src: dst, bits }
                } else if group > 1 {
                    return None;
                } else {
                    match dst {
                        Value::Register(dst) => Op::IncDec {
                            dst,
                            decrement: group == 1,
                        },
                        Value::Memory { address, bits } => Op::MemoryAlu {
                            kind: if group == 1 { Alu::Sub } else { Alu::Add },
                            address,
                            bits,
                            src: Value::Immediate(1),
                            preserve_carry: true,
                        },
                        _ => unreachable!(),
                    }
                }
            }
            0x86 | 0x87 => {
                let bits = if opcode == 0x86 { 8 } else { bits };
                let (dst, src, _) = self.read_modrm(bits, segment)?;
                Op::Exchange {
                    kind: ExchangeOp::Swap,
                    dst,
                    src,
                }
            }
            0x70..=0x7f | 0xeb | 0xe9 => {
                let relative = if opcode == 0xe9 {
                    let immediate = self.immediate(bits)?;
                    if bits == 16 {
                        immediate as i16 as i32
                    } else {
                        immediate as i32
                    }
                } else {
                    self.byte()? as i8 as i32
                };
                let next = eip.wrapping_add((self.at - start) as u32);
                let target =
                    next.wrapping_add_signed(relative) & if bits == 16 { 0xffff } else { u32::MAX };
                Op::Branch {
                    condition: if opcode < 0x80 {
                        Some(BranchCondition::Flags(opcode & 15))
                    } else {
                        None
                    },
                    target,
                }
            }
            0xe3 if bits == 32 => {
                let relative = self.byte()? as i8 as i32;
                let next = eip.wrapping_add((self.at - start) as u32);
                Op::Branch {
                    condition: Some(BranchCondition::EcxZero),
                    target: next.wrapping_add_signed(relative),
                }
            }
            0x0f => match self.byte()? {
                0x77 if bits == 32 => Op::Mmx {
                    opcode: 0x77,
                    reg: 0,
                    rm: Value::Immediate(0),
                },
                opcode @ 0x71..=0x73 if bits == 32 => {
                    let (rm, _, group) = self.read_modrm(64, segment)?;
                    let reg = rm.register()?.index;
                    let opcode = match (opcode, group) {
                        (0x71..=0x73, 2) => opcode + 0x60,
                        (0x71..=0x72, 4) => opcode + 0x70,
                        (0x71..=0x73, 6) => opcode + 0x80,
                        _ => return None,
                    };
                    Op::Mmx {
                        opcode,
                        reg,
                        rm: Value::Immediate(u32::from(self.byte()?)),
                    }
                }
                opcode @ (0x60..=0x6b
                | 0x6e..=0x6f
                | 0x74..=0x76
                | 0x7e..=0x7f
                | 0xd1..=0xd3
                | 0xd5
                | 0xd8..=0xd9
                | 0xdb..=0xdd
                | 0xdf
                | 0xe1..=0xe2
                | 0xe5
                | 0xe8..=0xe9
                | 0xeb..=0xed
                | 0xef
                | 0xf1..=0xf3
                | 0xf5
                | 0xf8..=0xfa
                | 0xfc..=0xfe)
                    if bits == 32 =>
                {
                    let width = if matches!(opcode, 0x6e | 0x7e) {
                        32
                    } else {
                        64
                    };
                    let (rm, _, reg) = self.read_modrm(width, segment)?;
                    Op::Mmx { opcode, reg, rm }
                }
                second @ 0xc8..=0xcf if bits == 32 => Op::ByteSwap {
                    dst: Register::encoded(second & 7, 32),
                },
                second @ 0x40..=0x4f => {
                    let (src, dst, _) = self.read_modrm(bits, segment)?;
                    Op::ConditionalMove {
                        dst,
                        src,
                        condition: second & 15,
                    }
                }
                0xc7 => {
                    // CMPXCHG8B is memory-only and always 64 bits. Operand-size
                    // override does not select a smaller compare/exchange.
                    let (dst, _, group) = self.read_modrm(32, segment)?;
                    let Value::Memory { address, .. } = dst else {
                        return None;
                    };
                    if group != 1 {
                        return None;
                    }
                    Op::CompareExchange8 { address }
                }
                opcode @ (0xbc | 0xbd) => {
                    let (src, dst, _) = self.read_modrm(bits, segment)?;
                    Op::BitScan {
                        dst,
                        src,
                        reverse: opcode == 0xbd,
                    }
                }
                opcode @ (0xb0 | 0xb1 | 0xc0 | 0xc1) => {
                    let bits = if opcode & 1 == 0 { 8 } else { bits };
                    let (dst, src, _) = self.read_modrm(bits, segment)?;
                    Op::Exchange {
                        kind: if opcode & 0x40 == 0 {
                            ExchangeOp::Compare
                        } else {
                            ExchangeOp::Add
                        },
                        dst,
                        src,
                    }
                }
                0xaf => {
                    let (rhs, dst, _) = self.read_modrm(bits, segment)?;
                    Op::Imul {
                        dst,
                        lhs: Value::Register(dst),
                        rhs,
                    }
                }
                opcode @ (0xa3 | 0xab | 0xb3 | 0xbb) => {
                    let (src, index, _) = self.read_modrm(bits, segment)?;
                    Op::BitTest {
                        src,
                        index: Value::Register(index),
                        bits,
                        kind: (opcode >> 3) & 3,
                    }
                }
                0xba => {
                    let (src, _, group) = self.read_modrm(bits, segment)?;
                    if !(4..=7).contains(&group) {
                        return None;
                    }
                    Op::BitTest {
                        src,
                        index: Value::Immediate(u32::from(self.byte()?)),
                        bits,
                        kind: group & 3,
                    }
                }
                op @ 0x90..=0x9f => {
                    let (dst, _, _) = self.read_modrm(8, segment)?;
                    Op::SetCondition {
                        dst,
                        condition: op & 15,
                    }
                }
                op @ (0xa4 | 0xa5 | 0xac | 0xad) if bits == 32 => {
                    let (dst, src, _) = self.modrm(bits)?;
                    let count = if op & 1 == 0 {
                        Value::Immediate(u32::from(self.byte()? & 31))
                    } else {
                        Value::Register(Register::encoded(1, 8))
                    };
                    Op::DoubleShift {
                        dst,
                        src,
                        right: op & 8 != 0,
                        count,
                    }
                }
                second @ 0x80..=0x8f => {
                    let immediate = self.immediate(bits)?;
                    let relative = if bits == 16 {
                        immediate as i16 as i32
                    } else {
                        immediate as i32
                    };
                    let next = eip.wrapping_add((self.at - start) as u32);
                    let target = next.wrapping_add_signed(relative)
                        & if bits == 16 { 0xffff } else { u32::MAX };
                    Op::Branch {
                        condition: Some(BranchCondition::Flags(second & 15)),
                        target,
                    }
                }
                second @ (0xb6 | 0xb7 | 0xbe | 0xbf) => {
                    let source_bits = if second & 1 == 0 { 8 } else { 16 };
                    let (src, _, group) = self.read_modrm(source_bits, segment)?;
                    Op::Extend {
                        dst: Register::encoded(group, bits),
                        src,
                        source_bits,
                        signed: second >= 0xbe,
                    }
                }
                _ => return None,
            },
            _ => return None,
        };
        if self.at - start > 15 {
            return None;
        }
        // A single vCPU exclusively owns RAM through the native invocation.
        // No device event or helper can interleave a guarded RMW. LOCK adds no
        // host atomic operation in this machine, but still has x86 restrictions.
        // Revisit this contract if RAM gains concurrent guest/device writers.
        if locked
            && !matches!(
                op,
                Op::MemoryAlu { .. }
                    | Op::BitTest {
                        src: Value::Memory { .. },
                        kind: 1..=3,
                        ..
                    }
                    | Op::CompareExchange8 { .. }
                    | Op::Exchange {
                        dst: Value::Memory { .. },
                        ..
                    }
            )
        {
            return None;
        }
        Some(Instruction {
            eip,
            next_eip: eip.wrapping_add((self.at - start) as u32),
            op,
        })
    }
}

pub fn block(bytes: &[u8], eip: u32, limit: usize) -> Block {
    let mut reader = Reader { bytes, at: 0 };
    let mut result = Block {
        instructions: Vec::new(),
        byte_len: 0,
    };
    let mut pc = eip;
    for _ in 0..limit {
        let Some(instruction) = reader.instruction(pc) else {
            break;
        };
        if result.instructions.is_empty() {
            // Unsupported entries allocate nothing. A real block is bounded by
            // both the caller's instruction limit and its permitted byte window.
            result.instructions.reserve_exact(limit.min(bytes.len()));
        }
        result.byte_len = reader.at;
        result.instructions.push(instruction);
        pc = instruction.next_eip;
        if matches!(
            instruction.op,
            Op::Branch { .. }
                | Op::LoadSegment { .. }
                | Op::SetInterrupt
                | Op::Return { .. }
                | Op::IndirectJump { .. }
                | Op::Call { .. }
                | Op::Movs { .. }
                | Op::String {
                    repeat: Some(_),
                    ..
                }
        ) {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmx_rejects_sse_prefixes_lock_and_invalid_shift_groups() {
        for prefix in [0x66, 0xf2, 0xf3, 0xf0, 0x67] {
            for instruction in [&[0x0f, 0x6f, 0xc1][..], &[0x0f, 0x77][..]] {
                let mut bytes = vec![prefix];
                bytes.extend_from_slice(instruction);
                assert!(block(&bytes, 0, 1).instructions.is_empty());
            }
        }
        for opcode in 0x71..=0x73 {
            for group in 0..8 {
                let valid = group == 2 || group == 6 || group == 4 && opcode != 0x73;
                assert_eq!(
                    !block(&[0x0f, opcode, 0xc0 | group << 3, 255], 0, 1)
                        .instructions
                        .is_empty(),
                    valid
                );
                assert!(
                    block(&[0x0f, opcode, group << 3, 1], 0, 1)
                        .instructions
                        .is_empty()
                );
            }
        }
    }

    #[test]
    fn lock_accepts_only_supported_memory_rmw_forms() {
        for bytes in [
            &[0xf0, 0x01, 0x08][..],
            &[0xf0, 0x83, 0x28, 1],
            &[0xf0, 0xff, 0x00],
            &[0xf0, 0x87, 0x08],
            &[0xf0, 0x0f, 0xc1, 0x08],
            &[0xf0, 0x0f, 0xb1, 0x08],
        ] {
            let decoded = block(bytes, 0x100, 1);
            assert_eq!(decoded.byte_len, bytes.len(), "{bytes:02x?}");
            assert!(decoded.instructions[0].op.writes_memory());
        }
        for bytes in [
            &[0xf0, 0x01, 0xc8][..],
            &[0xf0, 0x03, 0x08],
            &[0xf0, 0x83, 0x38, 1],
            &[0xf0, 0x87, 0xc8],
            &[0xf0, 0x0f, 0xc1, 0xc8],
            &[0xf0, 0x0f, 0xb1, 0xc8],
            &[0xf0, 0x90],
        ] {
            assert!(
                block(bytes, 0x100, 1).instructions.is_empty(),
                "{bytes:02x?}"
            );
        }
    }

    #[test]
    fn lookahead_never_consumes_partial_or_unsupported_instructions() {
        for bytes in [
            &[0x90, 0xb8, 0x12][..],
            &[0x90, 0x8b, 0x05, 0x00],
            &[0x90, 0xf0, 0x01, 0xc0],
        ] {
            let decoded = block(bytes, 0xff00, 32);
            assert_eq!(decoded.byte_len, 1);
            assert_eq!(decoded.instructions.len(), 1);
        }
        assert!(block(&[0x66; 16], 0, 32).instructions.is_empty());
    }

    #[test]
    fn high_bytes_and_short_operand16_targets_keep_x86_meaning() {
        let decoded = block(&[0x88, 0xe0, 0x66, 0x74, 0xfc], 0x12340, 32);
        assert_eq!(
            decoded.instructions[0].op,
            Op::Mov {
                dst: Register::encoded(0, 8),
                src: Value::Register(Register::encoded(4, 8)),
            }
        );
        assert_eq!(
            decoded.instructions[1].op,
            Op::Branch {
                condition: Some(BranchCondition::Flags(4)),
                target: 0x2341
            }
        );
    }
}
