//! ARMv6K (A32) emission for the ARM11 MPCore. Only ARMv6K encodings are
//! accepted: no MOVW/MOVT, BFI/UBFX, CBZ, RBIT, SDIV/UDIV or Thumb-2. Wide
//! constants use MOV/ORR chains or a PC-relative literal pool after the body.

use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reg(pub u8);

pub const R0: Reg = Reg(0);
pub const R1: Reg = Reg(1);
pub const R2: Reg = Reg(2);
pub const R3: Reg = Reg(3);
pub const R12: Reg = Reg(12);
pub const SP: Reg = Reg(13);
pub const LR: Reg = Reg(14);
pub const PC: Reg = Reg(15);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Cond {
    Eq = 0,
    Ne = 1,
    Cs = 2,
    Cc = 3,
    Mi = 4,
    Pl = 5,
    Vs = 6,
    Vc = 7,
    Hi = 8,
    Ls = 9,
    Ge = 10,
    Lt = 11,
    Gt = 12,
    Le = 13,
    Al = 14,
}

impl Cond {
    pub fn invert(self) -> Self {
        match self {
            Self::Eq => Self::Ne,
            Self::Ne => Self::Eq,
            Self::Cs => Self::Cc,
            Self::Cc => Self::Cs,
            Self::Mi => Self::Pl,
            Self::Pl => Self::Mi,
            Self::Vs => Self::Vc,
            Self::Vc => Self::Vs,
            Self::Hi => Self::Ls,
            Self::Ls => Self::Hi,
            Self::Ge => Self::Lt,
            Self::Lt => Self::Ge,
            Self::Gt => Self::Le,
            Self::Le => Self::Gt,
            Self::Al => unreachable!("AL has no inverse"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Dp {
    And = 0,
    Eor = 1,
    Sub = 2,
    Rsb = 3,
    Add = 4,
    Adc = 5,
    Sbc = 6,
    Tst = 8,
    Teq = 9,
    Cmp = 10,
    Cmn = 11,
    Orr = 12,
    Mov = 13,
    Bic = 14,
    Mvn = 15,
}

impl Dp {
    fn compares(self) -> bool {
        matches!(self, Self::Tst | Self::Teq | Self::Cmp | Self::Cmn)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Shift {
    Lsl(u8),
    Lsr(u8),
    Asr(u8),
    Ror(u8),
}

impl Shift {
    const NONE: Self = Self::Lsl(0);

    fn bits(self) -> u32 {
        let (kind, amount) = match self {
            Self::Lsl(n) => (0, n),
            Self::Lsr(n) => (1, n),
            Self::Asr(n) => (2, n),
            Self::Ror(n) => (3, n),
        };
        // LSR/ASR #32 use the zero encoding; ROR #0 would mean RRX.
        assert!(
            match self {
                Self::Lsl(n) => n < 32,
                Self::Lsr(n) | Self::Asr(n) => (1..=32).contains(&n),
                Self::Ror(n) => (1..32).contains(&n),
            } || self == Self::NONE
        );
        u32::from(amount & 31) << 7 | kind << 5
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Operand {
    Imm(u32),
    Reg(Reg, Shift),
    /// Register shifted by the low byte of another register.
    RegShift(Reg, u8, Reg),
}

impl From<Reg> for Operand {
    fn from(reg: Reg) -> Self {
        Self::Reg(reg, Shift::NONE)
    }
}

/// ARM modified immediate: an 8-bit value rotated right by an even amount.
pub fn encode_imm(value: u32) -> Option<u32> {
    (0..16).find_map(|rotate| {
        let unrotated = value.rotate_left(rotate * 2);
        (unrotated < 256).then_some(rotate << 8 | unrotated)
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Label(usize);

enum Fixup {
    Branch { at: usize, label: Label },
    Literal { at: usize, value: u32 },
}

pub struct Assembler {
    words: Vec<u32>,
    labels: Vec<Option<usize>>,
    fixups: Vec<Fixup>,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    pub fn new() -> Self {
        Self {
            words: Vec::new(),
            labels: Vec::new(),
            fixups: Vec::new(),
        }
    }

    pub fn offset(&self) -> usize {
        self.words.len()
    }

    pub fn emit(&mut self, word: u32) {
        self.words.push(word);
    }

    pub fn label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() - 1)
    }

    pub fn bind(&mut self, label: Label) {
        assert!(self.labels[label.0].is_none(), "label bound twice");
        self.labels[label.0] = Some(self.words.len());
    }

    /// Resolve branches and append the literal pool. LDR literals reach
    /// +4095 bytes, which bounds a block body to about 1000 words.
    pub fn finish(mut self) -> Option<Vec<u32>> {
        let mut pool: Vec<u32> = Vec::new();
        for fixup in core::mem::take(&mut self.fixups) {
            match fixup {
                Fixup::Branch { at, label } => {
                    let target = self.labels[label.0].expect("unbound label");
                    let delta = target as i32 - at as i32 - 2;
                    self.words[at] |= (delta as u32) & 0x00ff_ffff;
                }
                Fixup::Literal { at, value } => {
                    let index = pool.iter().position(|&v| v == value).unwrap_or_else(|| {
                        pool.push(value);
                        pool.len() - 1
                    });
                    let bytes = (self.words.len() + index - at - 2) * 4;
                    if bytes > 4095 {
                        return None;
                    }
                    self.words[at] |= bytes as u32;
                }
            }
        }
        self.words.extend(pool);
        Some(self.words)
    }

    fn dp_word(cond: Cond, op: Dp, s: bool, rn: Reg, rd: Reg, operand: Operand) -> u32 {
        let s = s || op.compares();
        let base = (cond as u32) << 28
            | (op as u32) << 21
            | u32::from(s) << 20
            | u32::from(rn.0) << 16
            | u32::from(rd.0) << 12;
        match operand {
            Operand::Imm(value) => {
                base | 1 << 25 | encode_imm(value).expect("unencodable ARM immediate")
            }
            Operand::Reg(rm, shift) => base | shift.bits() | u32::from(rm.0),
            Operand::RegShift(rm, kind, rs) => {
                base | u32::from(rs.0) << 8 | u32::from(kind) << 5 | 1 << 4 | u32::from(rm.0)
            }
        }
    }

    pub fn dp_cond(
        &mut self,
        cond: Cond,
        op: Dp,
        s: bool,
        rd: Reg,
        rn: Reg,
        operand: impl Into<Operand>,
    ) {
        let word = Self::dp_word(cond, op, s, rn, rd, operand.into());
        self.emit(word);
    }

    pub fn dp(&mut self, op: Dp, s: bool, rd: Reg, rn: Reg, operand: impl Into<Operand>) {
        self.dp_cond(Cond::Al, op, s, rd, rn, operand);
    }

    pub fn mov(&mut self, rd: Reg, operand: impl Into<Operand>) {
        self.dp(Dp::Mov, false, rd, Reg(0), operand);
    }

    pub fn mov_cond(&mut self, cond: Cond, rd: Reg, operand: impl Into<Operand>) {
        self.dp_cond(cond, Dp::Mov, false, rd, Reg(0), operand);
    }

    pub fn movs(&mut self, rd: Reg, operand: impl Into<Operand>) {
        self.dp(Dp::Mov, true, rd, Reg(0), operand);
    }

    pub fn mvn(&mut self, rd: Reg, operand: impl Into<Operand>) {
        self.dp(Dp::Mvn, false, rd, Reg(0), operand);
    }

    pub fn cmp(&mut self, rn: Reg, operand: impl Into<Operand>) {
        self.dp(Dp::Cmp, true, Reg(0), rn, operand);
    }

    pub fn tst(&mut self, rn: Reg, operand: impl Into<Operand>) {
        self.dp(Dp::Tst, true, Reg(0), rn, operand);
    }

    /// Materialize any 32-bit value: one or two data-processing words when
    /// possible, otherwise a literal load.
    pub fn mov_imm(&mut self, rd: Reg, value: u32) {
        self.mov_imm_cond(Cond::Al, rd, value);
    }

    pub fn mov_imm_cond(&mut self, cond: Cond, rd: Reg, value: u32) {
        if encode_imm(value).is_some() {
            self.mov_cond(cond, rd, Operand::Imm(value));
        } else if encode_imm(!value).is_some() {
            self.dp_cond(cond, Dp::Mvn, false, rd, Reg(0), Operand::Imm(!value));
        } else if let Some((high, low)) = split_imm(value) {
            self.mov_cond(cond, rd, Operand::Imm(high));
            self.dp_cond(cond, Dp::Orr, false, rd, rd, Operand::Imm(low));
        } else {
            self.fixups.push(Fixup::Literal {
                at: self.words.len(),
                value,
            });
            // LDR rd, [PC, #+offset]; the offset is filled by finish().
            self.emit((cond as u32) << 28 | 0x059f_0000 | u32::from(rd.0) << 12);
        }
    }

    /// rd = rn + value, using scratch only when value is not a short immediate.
    pub fn add_imm(&mut self, rd: Reg, rn: Reg, value: u32, scratch: Reg) {
        if value == 0 {
            if rd != rn {
                self.mov(rd, rn);
            }
        } else if encode_imm(value).is_some() {
            self.dp(Dp::Add, false, rd, rn, Operand::Imm(value));
        } else if encode_imm(value.wrapping_neg()).is_some() {
            self.dp(Dp::Sub, false, rd, rn, Operand::Imm(value.wrapping_neg()));
        } else if let Some((high, low)) = split_imm(value) {
            self.dp(Dp::Add, false, rd, rn, Operand::Imm(high));
            self.dp(Dp::Add, false, rd, rd, Operand::Imm(low));
        } else {
            assert!(scratch != rn);
            self.mov_imm(scratch, value);
            self.dp(Dp::Add, false, rd, rn, scratch);
        }
    }

    fn transfer(&mut self, load: bool, byte: bool, rt: Reg, rn: Reg, offset: i32) {
        assert!((-4095..=4095).contains(&offset));
        self.emit(
            0xe500_0000
                | u32::from(offset >= 0) << 23
                | u32::from(byte) << 22
                | u32::from(load) << 20
                | u32::from(rn.0) << 16
                | u32::from(rt.0) << 12
                | offset.unsigned_abs(),
        );
    }

    pub fn ldr(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.transfer(true, false, rt, rn, offset);
    }

    pub fn str(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.transfer(false, false, rt, rn, offset);
    }

    /// LDRD rt, rt+1, [rn, #offset]; rt even, offset 0..=255, doubleword
    /// aligned so ARMv6's legacy alignment mode also accepts it.
    pub fn ldrd(&mut self, rt: Reg, rn: Reg, offset: u32) {
        assert!(rt.0.is_multiple_of(2) && rt.0 < 14 && offset < 256 && offset.is_multiple_of(8));
        self.emit(
            0xe1c0_00d0
                | u32::from(rn.0) << 16
                | u32::from(rt.0) << 12
                | (offset >> 4) << 8
                | (offset & 15),
        );
    }

    /// Register-offset access [rn, rm]: bits 8/16/32, optional sign extension.
    pub fn load_indexed(&mut self, bits: u8, signed: bool, rt: Reg, rn: Reg, rm: Reg) {
        let regs = u32::from(rn.0) << 16 | u32::from(rt.0) << 12 | u32::from(rm.0);
        self.emit(match (bits, signed) {
            (32, _) => 0xe790_0000 | regs,
            (8, false) => 0xe7d0_0000 | regs,
            (16, false) => 0xe190_00b0 | regs,
            (8, true) => 0xe190_00d0 | regs,
            (16, true) => 0xe190_00f0 | regs,
            _ => unreachable!(),
        });
    }

    pub fn store_indexed(&mut self, bits: u8, rt: Reg, rn: Reg, rm: Reg) {
        let regs = u32::from(rn.0) << 16 | u32::from(rt.0) << 12 | u32::from(rm.0);
        self.emit(match bits {
            32 => 0xe780_0000 | regs,
            8 => 0xe7c0_0000 | regs,
            16 => 0xe180_00b0 | regs,
            _ => unreachable!(),
        });
    }

    pub fn ldm(&mut self, rn: Reg, regs: u16) {
        self.emit(0xe890_0000 | u32::from(rn.0) << 16 | u32::from(regs));
    }

    pub fn stm(&mut self, rn: Reg, regs: u16) {
        self.emit(0xe880_0000 | u32::from(rn.0) << 16 | u32::from(regs));
    }

    pub fn push(&mut self, regs: u16) {
        self.emit(0xe92d_0000 | u32::from(regs));
    }

    pub fn pop(&mut self, regs: u16) {
        self.emit(0xe8bd_0000 | u32::from(regs));
    }

    pub fn b(&mut self, label: Label) {
        self.b_cond(Cond::Al, label);
    }

    pub fn b_cond(&mut self, cond: Cond, label: Label) {
        self.fixups.push(Fixup::Branch {
            at: self.words.len(),
            label,
        });
        self.emit((cond as u32) << 28 | 0x0a00_0000);
    }

    pub fn bx(&mut self, rm: Reg) {
        self.emit(0xe12f_ff10 | u32::from(rm.0));
    }

    pub fn mrs(&mut self, rd: Reg) {
        self.emit(0xe10f_0000 | u32::from(rd.0) << 12);
    }

    /// Write CPSR N/Z/C/V only.
    pub fn msr_flags(&mut self, rm: Reg) {
        self.emit(0xe128_f000 | u32::from(rm.0));
    }

    pub fn msr_flags_imm(&mut self, value: u32) {
        self.emit(0xe328_f000 | encode_imm(value).expect("flag immediate"));
    }

    pub fn mul(&mut self, rd: Reg, rm: Reg, rs: Reg) {
        self.emit(0xe000_0090 | u32::from(rd.0) << 16 | u32::from(rs.0) << 8 | u32::from(rm.0));
    }

    /// (hi:lo) = rm * rs, signed or unsigned. hi, lo and rm must differ.
    pub fn mull(&mut self, signed: bool, lo: Reg, hi: Reg, rm: Reg, rs: Reg) {
        assert!(lo != hi && lo != rm && hi != rm);
        self.emit(
            if signed { 0xe0c0_0090 } else { 0xe080_0090 }
                | u32::from(hi.0) << 16
                | u32::from(lo.0) << 12
                | u32::from(rs.0) << 8
                | u32::from(rm.0),
        );
    }

    pub fn clz(&mut self, rd: Reg, rm: Reg) {
        self.emit(0xe16f_0f10 | u32::from(rd.0) << 12 | u32::from(rm.0));
    }

    pub fn rev(&mut self, rd: Reg, rm: Reg) {
        self.emit(0xe6bf_0f30 | u32::from(rd.0) << 12 | u32::from(rm.0));
    }

    /// UXTB/UXTH/SXTB/SXTH with an optional rotation of 8, 16 or 24 bits.
    pub fn extend(&mut self, bits: u8, signed: bool, rd: Reg, rm: Reg, rotate: u8) {
        assert!(rotate.is_multiple_of(8) && rotate < 32);
        let base = match (bits, signed) {
            (8, false) => 0xe6ef_0070,
            (16, false) => 0xe6ff_0070,
            (8, true) => 0xe6af_0070,
            (16, true) => 0xe6bf_0070,
            _ => unreachable!(),
        };
        self.emit(base | u32::from(rd.0) << 12 | u32::from(rotate / 8) << 10 | u32::from(rm.0));
    }

    /// UXTAB: rd = rn + zero_extend(rm[7:0]).
    pub fn uxtab(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.emit(0xe6e0_0070 | u32::from(rn.0) << 16 | u32::from(rd.0) << 12 | u32::from(rm.0));
    }

    /// PKHBT: rd = rn[15:0] | rm[31:16].
    pub fn pkhbt(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.emit(0xe680_0010 | u32::from(rn.0) << 16 | u32::from(rd.0) << 12 | u32::from(rm.0));
    }

    /// PKHTB: rd = rn[31:16] | (rm ASR #16)[15:0].
    pub fn pkhtb16(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.emit(
            0xe680_0050 | u32::from(rn.0) << 16 | u32::from(rd.0) << 12 | 16 << 7 | u32::from(rm.0),
        );
    }

    /// Emit `f` under `cond`. It may use literals but no branches.
    pub fn with_cond(&mut self, cond: Cond, f: impl FnOnce(&mut Self)) {
        let start = self.words.len();
        let fixups = self.fixups.len();
        f(self);
        assert!(
            self.fixups[fixups..]
                .iter()
                .all(|fixup| matches!(fixup, Fixup::Literal { .. }))
        );
        for word in &mut self.words[start..] {
            assert_eq!(*word >> 28, Cond::Al as u32);
            *word = (*word & 0x0fff_ffff) | (cond as u32) << 28;
        }
    }
}

fn split_imm(value: u32) -> Option<(u32, u32)> {
    // Two disjoint rotated bytes: try every even rotation for the low part.
    (0..16).find_map(|rotate| {
        let low = value & 0xffu32.rotate_right(rotate * 2);
        let high = value & !low;
        (low != 0 && encode_imm(high).is_some() && encode_imm(low).is_some()).then_some((high, low))
    })
}

pub const fn regs(list: &[u8]) -> u16 {
    let mut mask = 0;
    let mut i = 0;
    while i < list.len() {
        mask |= 1 << list[i];
        i += 1;
    }
    mask
}
