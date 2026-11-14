//! Thumb-2 (T32) emission for Armv7-M and Armv8-M Mainline cores such as the
//! Cortex-M33. Only base encodings are produced: no A32, no DSP extension
//! (PKHBT, UXTAB), no floating point. Every instruction is 32 bits wide except
//! IT and BX, and the buffer holds halfwords. Wide constants use MOVW/MOVT, so
//! there is no literal pool and `finish` cannot fail. A conditional instruction
//! is an IT block of one instruction; see [`Assembler::with_cond`].
//!
//! `tests` pins every emitter to `llvm-mc -triple=thumbv8m.main-none-eabi`.

use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reg(pub u8);

pub const R0: Reg = Reg(0);
pub const R1: Reg = Reg(1);
pub const R2: Reg = Reg(2);
pub const R3: Reg = Reg(3);
pub const R12: Reg = Reg(12);
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dp {
    And,
    Eor,
    Sub,
    Rsb,
    Add,
    Adc,
    Sbc,
    Tst,
    Teq,
    Cmp,
    Cmn,
    Orr,
    Mov,
    Bic,
    Mvn,
}

impl Dp {
    /// T32 opcode, whether the result is discarded (Rd = 15, S set), and
    /// whether there is no first operand (Rn = 15).
    fn fields(self) -> (u32, bool, bool) {
        match self {
            Self::And => (0, false, false),
            Self::Tst => (0, true, false),
            Self::Bic => (1, false, false),
            Self::Orr => (2, false, false),
            Self::Mov => (2, false, true),
            Self::Mvn => (3, false, true),
            Self::Eor => (4, false, false),
            Self::Teq => (4, true, false),
            Self::Add => (8, false, false),
            Self::Cmn => (8, true, false),
            Self::Adc => (10, false, false),
            Self::Sbc => (11, false, false),
            Self::Sub => (13, false, false),
            Self::Cmp => (13, true, false),
            Self::Rsb => (14, false, false),
        }
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

    /// (type, imm5). LSR/ASR #32 use the zero encoding; ROR #0 would be RRX.
    fn fields(self) -> (u32, u32) {
        let (kind, amount) = match self {
            Self::Lsl(n) => (0, n),
            Self::Lsr(n) => (1, n),
            Self::Asr(n) => (2, n),
            Self::Ror(n) => (3, n),
        };
        assert!(match self {
            Self::Lsl(n) => n < 32,
            Self::Lsr(n) | Self::Asr(n) => (1..=32).contains(&n),
            Self::Ror(n) => (1..32).contains(&n),
        });
        (kind, u32::from(amount & 31))
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Operand {
    Imm(u32),
    Reg(Reg, Shift),
}

impl From<Reg> for Operand {
    fn from(reg: Reg) -> Self {
        Self::Reg(reg, Shift::NONE)
    }
}

/// T32 modified immediate: an 8-bit value, that byte repeated in two or four
/// byte lanes, or 1bcdefgh rotated right by 8..=31. Returns i:imm3:imm8.
pub fn encode_imm(value: u32) -> Option<u32> {
    let low = value & 0xff;
    let high = (value >> 8) & 0xff;
    if value < 256 {
        Some(value)
    } else if value == (low | low << 16) {
        Some(0x100 | low)
    } else if value == (high << 8 | high << 24) {
        Some(0x200 | high)
    } else if value == low * 0x0101_0101 {
        Some(0x300 | low)
    } else {
        (8..32).find_map(|rotate| {
            let unrotated = value.rotate_left(rotate);
            (0x80..=0xff)
                .contains(&unrotated)
                .then_some(rotate << 7 | (unrotated & 0x7f))
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Label(usize);

struct Fixup {
    at: usize,
    label: Label,
    cond: Cond,
}

pub struct Assembler {
    code: Vec<u16>,
    labels: Vec<Option<usize>>,
    fixups: Vec<Fixup>,
    /// Applied to every instruction emitted inside `with_cond`.
    cond: Option<Cond>,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

/// A general operand register: R0..R12 or LR.
fn r(reg: Reg) -> u32 {
    assert!(
        reg.0 <= 12 || reg.0 == 14,
        "SP and PC are not general T32 operands"
    );
    u32::from(reg.0)
}

/// B.W (T4): the byte offset from the branch address plus four is
/// S:I1:I2:imm10:imm11:'0', with J = NOT(I XOR S).
fn branch(delta: i32) -> (u16, u16) {
    assert!(
        (-(1 << 24)..1 << 24).contains(&delta) && delta & 1 == 0,
        "branch out of range"
    );
    let d = delta as u32;
    let (s, i1, i2) = ((d >> 24) & 1, (d >> 23) & 1, (d >> 22) & 1);
    (
        (0xf000 | s << 10 | ((d >> 12) & 0x3ff)) as u16,
        (0x9000 | (i1 ^ s ^ 1) << 13 | (i2 ^ s ^ 1) << 11 | ((d >> 1) & 0x7ff)) as u16,
    )
}

/// B<cond>.W (T3): the offset is S:J2:J1:imm6:imm11:'0'.
fn branch_cond(cond: Cond, delta: i32) -> (u16, u16) {
    assert!(
        (-(1 << 20)..1 << 20).contains(&delta) && delta & 1 == 0,
        "conditional branch out of range"
    );
    let d = delta as u32;
    let (s, j2, j1) = ((d >> 20) & 1, (d >> 19) & 1, (d >> 18) & 1);
    (
        (0xf000 | s << 10 | (cond as u32) << 6 | ((d >> 12) & 0x3f)) as u16,
        (0x8000 | j1 << 13 | j2 << 11 | ((d >> 1) & 0x7ff)) as u16,
    )
}

impl Assembler {
    pub fn new() -> Self {
        Self {
            code: Vec::new(),
            labels: Vec::new(),
            fixups: Vec::new(),
            cond: None,
        }
    }

    /// Halfwords emitted so far.
    pub fn offset(&self) -> usize {
        self.code.len()
    }

    pub fn label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() - 1)
    }

    pub fn bind(&mut self, label: Label) {
        assert!(self.cond.is_none(), "no label inside with_cond");
        assert!(self.labels[label.0].is_none(), "label bound twice");
        self.labels[label.0] = Some(self.code.len());
    }

    /// Resolve branches and return the halfwords.
    pub fn finish(mut self) -> Vec<u16> {
        for Fixup { at, label, cond } in core::mem::take(&mut self.fixups) {
            let target = self.labels[label.0].expect("unbound label");
            let delta = (target as i32 - at as i32 - 2) * 2;
            let (first, second) = if cond == Cond::Al {
                branch(delta)
            } else {
                branch_cond(cond, delta)
            };
            self.code[at] = first;
            self.code[at + 1] = second;
        }
        self.code
    }

    /// One 32-bit instruction, preceded by its IT inside `with_cond`.
    fn wide(&mut self, first: u32, second: u32) {
        debug_assert!(first <= 0xffff && second <= 0xffff);
        if let Some(cond) = self.cond {
            self.code.push(0xbf08 | (cond as u16) << 4);
        }
        self.code.push(first as u16);
        self.code.push(second as u16);
    }

    /// Emit `f` with every instruction conditional on `cond`. Each one gets
    /// its own IT, so later conditions see flags an earlier one changed, as
    /// in A32. No branches or labels inside.
    pub fn with_cond(&mut self, cond: Cond, f: impl FnOnce(&mut Self)) {
        assert!(self.cond.is_none() && cond != Cond::Al);
        self.cond = Some(cond);
        f(self);
        self.cond = None;
    }

    pub fn dp(&mut self, op: Dp, s: bool, rd: Reg, rn: Reg, operand: impl Into<Operand>) {
        let (code, compare, unary) = op.fields();
        let s = u32::from(s || compare);
        let rd = if compare { 15 } else { r(rd) };
        let rn = if unary { 15 } else { r(rn) };
        match operand.into() {
            Operand::Imm(value) => {
                let imm = encode_imm(value).expect("unencodable T32 immediate");
                self.wide(
                    0xf000 | (imm >> 11) << 10 | code << 5 | s << 4 | rn,
                    ((imm >> 8) & 7) << 12 | rd << 8 | (imm & 0xff),
                );
            }
            Operand::Reg(rm, shift) => {
                let (kind, amount) = shift.fields();
                self.wide(
                    0xea00 | code << 5 | s << 4 | rn,
                    (amount >> 2) << 12 | rd << 8 | (amount & 3) << 6 | kind << 4 | r(rm),
                );
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
        let operand = operand.into();
        self.with_cond(cond, |a| a.dp(op, s, rd, rn, operand));
    }

    pub fn mov(&mut self, rd: Reg, operand: impl Into<Operand>) {
        self.dp(Dp::Mov, false, rd, R0, operand);
    }

    pub fn mov_cond(&mut self, cond: Cond, rd: Reg, operand: impl Into<Operand>) {
        self.dp_cond(cond, Dp::Mov, false, rd, R0, operand);
    }

    pub fn movs(&mut self, rd: Reg, operand: impl Into<Operand>) {
        self.dp(Dp::Mov, true, rd, R0, operand);
    }

    pub fn mvn(&mut self, rd: Reg, operand: impl Into<Operand>) {
        self.dp(Dp::Mvn, false, rd, R0, operand);
    }

    pub fn cmp(&mut self, rn: Reg, operand: impl Into<Operand>) {
        self.dp(Dp::Cmp, true, R0, rn, operand);
    }

    pub fn cmn(&mut self, rn: Reg, operand: impl Into<Operand>) {
        self.dp(Dp::Cmn, true, R0, rn, operand);
    }

    pub fn tst(&mut self, rn: Reg, operand: impl Into<Operand>) {
        self.dp(Dp::Tst, true, R0, rn, operand);
    }

    /// MOVW (0xf240) or MOVT (0xf2c0): imm16 is imm4:i:imm3:imm8.
    fn move16(&mut self, op: u32, rd: Reg, imm: u32) {
        self.wide(
            op | ((imm >> 11) & 1) << 10 | imm >> 12,
            ((imm >> 8) & 7) << 12 | r(rd) << 8 | (imm & 0xff),
        );
    }

    /// Materialize any 32-bit value in one or two instructions.
    pub fn mov_imm(&mut self, rd: Reg, value: u32) {
        if encode_imm(value).is_some() {
            self.mov(rd, Operand::Imm(value));
        } else if encode_imm(!value).is_some() {
            self.mvn(rd, Operand::Imm(!value));
        } else {
            self.move16(0xf240, rd, value & 0xffff);
            if value >> 16 != 0 {
                self.move16(0xf2c0, rd, value >> 16);
            }
        }
    }

    /// ADDW (0xf200) or SUBW (0xf2a0): a plain 12-bit immediate i:imm3:imm8.
    fn plain12(&mut self, op: u32, rd: Reg, rn: Reg, imm: u32) {
        self.wide(
            op | (imm >> 11) << 10 | r(rn),
            ((imm >> 8) & 7) << 12 | r(rd) << 8 | (imm & 0xff),
        );
    }

    /// rd = rn + value; `scratch` is used only when no immediate form fits.
    pub fn add_imm(&mut self, rd: Reg, rn: Reg, value: u32, scratch: Reg) {
        let negated = value.wrapping_neg();
        if value == 0 {
            if rd != rn {
                self.mov(rd, rn);
            }
        } else if encode_imm(value).is_some() {
            self.dp(Dp::Add, false, rd, rn, Operand::Imm(value));
        } else if encode_imm(negated).is_some() {
            self.dp(Dp::Sub, false, rd, rn, Operand::Imm(negated));
        } else if value < 4096 {
            self.plain12(0xf200, rd, rn, value);
        } else if negated < 4096 {
            self.plain12(0xf2a0, rd, rn, negated);
        } else {
            assert!(scratch != rn);
            self.mov_imm(scratch, value);
            self.dp(Dp::Add, false, rd, rn, scratch);
        }
    }

    /// BFI (first = 0xf360 | rn) or BFC (first = 0xf36f): bits msb..=lsb.
    fn insert(&mut self, first: u32, rd: Reg, lsb: u8, width: u8) {
        assert!(width != 0 && u32::from(lsb) + u32::from(width) <= 32);
        let (lsb, msb) = (u32::from(lsb), u32::from(lsb + width - 1));
        self.wide(first, (lsb >> 2) << 12 | r(rd) << 8 | (lsb & 3) << 6 | msb);
    }

    /// rd[lsb + width - 1 : lsb] = rn[width - 1 : 0]; other bits are kept.
    pub fn bfi(&mut self, rd: Reg, rn: Reg, lsb: u8, width: u8) {
        self.insert(0xf360 | r(rn), rd, lsb, width);
    }

    /// Clear rd[lsb + width - 1 : lsb].
    pub fn bfc(&mut self, rd: Reg, lsb: u8, width: u8) {
        self.insert(0xf36f, rd, lsb, width);
    }

    /// rd = rn shifted by the low byte of rm. kind: 0 LSL, 1 LSR, 2 ASR,
    /// 3 ROR. With `s`, N, Z and the carry-out are set as for an A32 MOVS
    /// with a register-shifted operand; a zero count changes neither.
    pub fn shift_reg(&mut self, kind: u8, s: bool, rd: Reg, rn: Reg, rm: Reg) {
        assert!(kind < 4);
        self.wide(
            0xfa00 | u32::from(kind) << 5 | u32::from(s) << 4 | r(rn),
            0xf000 | r(rd) << 8 | r(rm),
        );
    }

    fn transfer(&mut self, first: u32, rt: u32, rn: Reg, offset: i32) {
        assert!((0..4096).contains(&offset));
        self.wide(first | r(rn), rt << 12 | offset as u32);
    }

    /// LDR rt, [rn, #offset]. Loading PC branches to a Thumb address: the
    /// word must have bit 0 set, and inside `with_cond` it must come last.
    pub fn ldr(&mut self, rt: Reg, rn: Reg, offset: i32) {
        let rt = if rt == PC { 15 } else { r(rt) };
        self.transfer(0xf8d0, rt, rn, offset);
    }

    pub fn str(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.transfer(0xf8c0, r(rt), rn, offset);
    }

    /// LDRD rt, rt2, [rn, #offset]: rt from the lower address. The offset is
    /// a multiple of four below 1024 and the address must be word aligned.
    pub fn ldrd(&mut self, rt: Reg, rt2: Reg, rn: Reg, offset: u32) {
        assert!(rt != rt2 && offset < 1024 && offset.is_multiple_of(4));
        self.wide(0xe9d0 | r(rn), r(rt) << 12 | r(rt2) << 8 | offset >> 2);
    }

    /// Register-offset access [rn, rm]: bits 8/16/32, optional sign extension.
    pub fn load_indexed(&mut self, bits: u8, signed: bool, rt: Reg, rn: Reg, rm: Reg) {
        let first = match (bits, signed) {
            (32, _) => 0xf850,
            (8, false) => 0xf810,
            (16, false) => 0xf830,
            (8, true) => 0xf910,
            (16, true) => 0xf930,
            _ => unreachable!(),
        };
        self.wide(first | r(rn), r(rt) << 12 | r(rm));
    }

    pub fn store_indexed(&mut self, bits: u8, rt: Reg, rn: Reg, rm: Reg) {
        let first = match bits {
            32 => 0xf840,
            8 => 0xf800,
            16 => 0xf820,
            _ => unreachable!(),
        };
        self.wide(first | r(rn), r(rt) << 12 | r(rm));
    }

    /// LDMIA rn, {regs} without writeback: at least two of R0..R12 and LR.
    pub fn ldm(&mut self, rn: Reg, regs: u16) {
        assert!(regs & 0xa000 == 0 && regs.count_ones() >= 2);
        self.wide(0xe890 | r(rn), u32::from(regs));
    }

    pub fn stm(&mut self, rn: Reg, regs: u16) {
        assert!(regs & 0xa000 == 0 && regs.count_ones() >= 2);
        self.wide(0xe880 | r(rn), u32::from(regs));
    }

    /// PUSH {regs}: R0..R12 and LR.
    pub fn push(&mut self, regs: u16) {
        assert!(regs & 0xa000 == 0 && regs.count_ones() >= 2);
        self.wide(0xe92d, u32::from(regs));
    }

    /// POP {regs}: R0..R12 and either LR or PC. Popping PC returns.
    pub fn pop(&mut self, regs: u16) {
        assert!(regs & 0x2000 == 0 && regs & 0xc000 != 0xc000 && regs.count_ones() >= 2);
        self.wide(0xe8bd, u32::from(regs));
    }

    pub fn b(&mut self, label: Label) {
        self.b_cond(Cond::Al, label);
    }

    pub fn b_cond(&mut self, cond: Cond, label: Label) {
        assert!(self.cond.is_none(), "no branch inside with_cond");
        self.fixups.push(Fixup {
            at: self.code.len(),
            label,
            cond,
        });
        self.code.extend([0, 0]);
    }

    /// BX rm: branch to the Thumb address in rm (bit 0 set).
    pub fn bx(&mut self, rm: Reg) {
        assert!(self.cond.is_none(), "no branch inside with_cond");
        self.code.push(0x4700 | (r(rm) as u16) << 3);
    }

    /// MRS rd, APSR.
    pub fn mrs(&mut self, rd: Reg) {
        self.wide(0xf3ef, 0x8000 | r(rd) << 8);
    }

    /// MSR APSR_nzcvq, rm: write N/Z/C/V (and Q).
    pub fn msr_flags(&mut self, rm: Reg) {
        self.wide(0xf380 | r(rm), 0x8800);
    }

    /// (hi:lo) = rn * rm, signed or unsigned.
    pub fn mull(&mut self, signed: bool, lo: Reg, hi: Reg, rn: Reg, rm: Reg) {
        assert!(lo != hi);
        self.wide(
            if signed { 0xfb80 } else { 0xfba0 } | r(rn),
            r(lo) << 12 | r(hi) << 8 | r(rm),
        );
    }

    /// rd = ra - rn * rm.
    pub fn mls(&mut self, rd: Reg, rn: Reg, rm: Reg, ra: Reg) {
        self.wide(0xfb00 | r(rn), r(ra) << 12 | r(rd) << 8 | 0x10 | r(rm));
    }

    /// rd = rn / rm, truncated toward zero; rm == 0 gives 0 when the divide
    /// trap is off, so callers test the divisor first.
    pub fn sdiv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.wide(0xfb90 | r(rn), 0xf0f0 | r(rd) << 8 | r(rm));
    }

    pub fn udiv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.wide(0xfbb0 | r(rn), 0xf0f0 | r(rd) << 8 | r(rm));
    }

    pub fn clz(&mut self, rd: Reg, rm: Reg) {
        self.wide(0xfab0 | r(rm), 0xf080 | r(rd) << 8 | r(rm));
    }

    pub fn rev(&mut self, rd: Reg, rm: Reg) {
        self.wide(0xfa90 | r(rm), 0xf080 | r(rd) << 8 | r(rm));
    }

    /// UXTB/UXTH/SXTB/SXTH with an optional rotation of 8, 16 or 24 bits.
    pub fn extend(&mut self, bits: u8, signed: bool, rd: Reg, rm: Reg, rotate: u8) {
        assert!(rotate.is_multiple_of(8) && rotate < 32);
        let first = match (bits, signed) {
            (16, true) => 0xfa0f,
            (16, false) => 0xfa1f,
            (8, true) => 0xfa4f,
            (8, false) => 0xfa5f,
            _ => unreachable!(),
        };
        self.wide(
            first,
            0xf080 | r(rd) << 8 | u32::from(rotate / 8) << 4 | r(rm),
        );
    }
}

/// Register list mask for LDM/STM/PUSH/POP.
pub const fn regs(list: &[u8]) -> u16 {
    let mut mask = 0;
    let mut i = 0;
    while i < list.len() {
        mask |= 1 << list[i];
        i += 1;
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emitted(f: impl FnOnce(&mut Assembler)) -> Vec<u16> {
        let mut a = Assembler::new();
        f(&mut a);
        a.finish()
    }

    /// Expected halfwords are llvm-mc's for the instruction in each comment
    /// (`-triple=thumbv8m.main-none-eabi -show-encoding`, no extensions).
    #[test]
    fn encodings_match_llvm_mc() {
        type Case = (fn(&mut Assembler), &'static [u16]);
        let cases: &[Case] = &[
            // adds.w r5, r4, r3, lsl #26
            (
                |a| {
                    a.dp(
                        Dp::Add,
                        true,
                        Reg(5),
                        Reg(4),
                        Operand::Reg(Reg(3), Shift::Lsl(26)),
                    )
                },
                &[0xeb14, 0x6583],
            ),
            // adds.w r5, r4, r3, lsr #32
            (
                |a| {
                    a.dp(
                        Dp::Add,
                        true,
                        Reg(5),
                        Reg(4),
                        Operand::Reg(Reg(3), Shift::Lsr(32)),
                    )
                },
                &[0xeb14, 0x0513],
            ),
            // adds.w r5, r4, #0xff00
            (
                |a| a.dp(Dp::Add, true, Reg(5), Reg(4), Operand::Imm(0xff00)),
                &[0xf514, 0x457f],
            ),
            // subs.w r0, r4, r5, lsl #24
            (
                |a| {
                    a.dp(
                        Dp::Sub,
                        true,
                        R0,
                        Reg(4),
                        Operand::Reg(Reg(5), Shift::Lsl(24)),
                    )
                },
                &[0xebb4, 0x6005],
            ),
            // adcs r7, r7, #0x80
            (
                |a| a.dp(Dp::Adc, true, Reg(7), Reg(7), Operand::Imm(0x80)),
                &[0xf157, 0x0780],
            ),
            // sbcs.w r7, r7, r6
            (
                |a| a.dp(Dp::Sbc, true, Reg(7), Reg(7), Reg(6)),
                &[0xeb77, 0x0706],
            ),
            // rsbs.w r4, r4, #0
            (
                |a| a.dp(Dp::Rsb, true, Reg(4), Reg(4), Operand::Imm(0)),
                &[0xf1d4, 0x0400],
            ),
            // rsb.w r2, r0, #32
            (
                |a| a.dp(Dp::Rsb, false, R2, R0, Operand::Imm(32)),
                &[0xf1c0, 0x0220],
            ),
            // eor r12, r12, #0x20000000
            (
                |a| a.dp(Dp::Eor, false, R12, R12, Operand::Imm(0x2000_0000)),
                &[0xf08c, 0x5c00],
            ),
            // bic r12, r12, #0x30000000
            (
                |a| a.dp(Dp::Bic, false, R12, R12, Operand::Imm(0x3000_0000)),
                &[0xf02c, 0x5c40],
            ),
            // orr.w r12, r12, r2, lsr #3
            (
                |a| a.dp(Dp::Orr, false, R12, R12, Operand::Reg(R2, Shift::Lsr(3))),
                &[0xea4c, 0x0cd2],
            ),
            // and r2, r5, #31
            (
                |a| a.dp(Dp::And, false, R2, Reg(5), Operand::Imm(31)),
                &[0xf005, 0x021f],
            ),
            // bic r4, r4, #0xff00
            (
                |a| a.dp(Dp::Bic, false, Reg(4), Reg(4), Operand::Imm(0xff00)),
                &[0xf424, 0x447f],
            ),
            // add.w r1, r1, #0xf000
            (
                |a| a.dp(Dp::Add, false, R1, R1, Operand::Imm(0xf000)),
                &[0xf501, 0x4170],
            ),
            // cmp.w r12, r2, lsl #12
            (
                |a| a.cmp(R12, Operand::Reg(R2, Shift::Lsl(12))),
                &[0xebbc, 0x3f02],
            ),
            // cmp.w r4, #0
            (|a| a.cmp(Reg(4), Operand::Imm(0)), &[0xf1b4, 0x0f00]),
            // cmp.w r4, #0x80000000
            (
                |a| a.cmp(Reg(4), Operand::Imm(0x8000_0000)),
                &[0xf1b4, 0x4f00],
            ),
            // cmn.w r1, #1
            (|a| a.cmn(R1, Operand::Imm(1)), &[0xf111, 0x0f01]),
            // tst.w r12, #1024
            (|a| a.tst(R12, Operand::Imm(1 << 10)), &[0xf41c, 0x6f80]),
            // mov.w r4, r5
            (|a| a.mov(Reg(4), Reg(5)), &[0xea4f, 0x0405]),
            // mov.w lr, r0
            (|a| a.mov(LR, R0), &[0xea4f, 0x0e00]),
            // lsr.w r0, r4, #8
            (
                |a| a.mov(R0, Operand::Reg(Reg(4), Shift::Lsr(8))),
                &[0xea4f, 0x2014],
            ),
            // lsl.w r2, r2, #19
            (
                |a| a.mov(R2, Operand::Reg(R2, Shift::Lsl(19))),
                &[0xea4f, 0x42c2],
            ),
            // asr.w r6, r4, #31
            (
                |a| a.mov(Reg(6), Operand::Reg(Reg(4), Shift::Asr(31))),
                &[0xea4f, 0x76e4],
            ),
            // ror.w r4, r4, #27
            (
                |a| a.mov(Reg(4), Operand::Reg(Reg(4), Shift::Ror(27))),
                &[0xea4f, 0x64f4],
            ),
            // lsls.w r1, r1, #3
            (
                |a| a.movs(R1, Operand::Reg(R1, Shift::Lsl(3))),
                &[0xea5f, 0x01c1],
            ),
            // mvn.w r0, r4
            (|a| a.mvn(R0, Reg(4)), &[0xea6f, 0x0004]),
            // mov.w r0, #1
            (|a| a.mov(R0, Operand::Imm(1)), &[0xf04f, 0x0001]),
            // movw r5, #0x1234
            (|a| a.mov_imm(Reg(5), 0x1234), &[0xf241, 0x2534]),
            // movw r0, #0x5678; movt r0, #0x1234
            (
                |a| a.mov_imm(R0, 0x1234_5678),
                &[0xf245, 0x6078, 0xf2c1, 0x2034],
            ),
            // mvn.w r4, #0xff
            (|a| a.mov_imm(Reg(4), 0xffff_ff00), &[0xf06f, 0x04ff]),
            // mov.w r1, #0xff000000
            (|a| a.mov_imm(R1, 0xff00_0000), &[0xf04f, 0x417f]),
            // add.w r8, r8, #8
            (|a| a.add_imm(Reg(8), Reg(8), 8, R12), &[0xf108, 0x0808]),
            // addw r0, r7, #0x123
            (|a| a.add_imm(R0, Reg(7), 0x123, R12), &[0xf207, 0x1023]),
            // subw r0, r7, #0x123
            (
                |a| a.add_imm(R0, Reg(7), 0xffff_fedd, R12),
                &[0xf2a7, 0x1023],
            ),
            // sub.w r0, r7, #16
            (
                |a| a.add_imm(R0, Reg(7), 0xffff_fff0, R12),
                &[0xf1a7, 0x0010],
            ),
            // movw r12, #0x5678; movt r12, #0x1234; add.w r0, r7, r12
            (
                |a| a.add_imm(R0, Reg(7), 0x1234_5678, R12),
                &[0xf245, 0x6c78, 0xf2c1, 0x2c34, 0xeb07, 0x000c],
            ),
            // bfi r4, r0, #8, #8
            (|a| a.bfi(Reg(4), R0, 8, 8), &[0xf360, 0x240f]),
            // bfi r7, r1, #0, #16
            (|a| a.bfi(Reg(7), R1, 0, 16), &[0xf361, 0x070f]),
            // bfc r6, #0, #16
            (|a| a.bfc(Reg(6), 0, 16), &[0xf36f, 0x060f]),
            // lsls.w r1, r4, r2
            (|a| a.shift_reg(0, true, R1, Reg(4), R2), &[0xfa14, 0xf102]),
            // lsr.w r3, r5, r2
            (|a| a.shift_reg(1, false, R3, Reg(5), R2), &[0xfa25, 0xf302]),
            // asrs.w r4, r4, r2
            (
                |a| a.shift_reg(2, true, Reg(4), Reg(4), R2),
                &[0xfa54, 0xf402],
            ),
            // ror.w r11, r11, r0
            (
                |a| a.shift_reg(3, false, Reg(11), Reg(11), R0),
                &[0xfa6b, 0xfb00],
            ),
            // uxtb.w r0, r7, ror #8
            (|a| a.extend(8, false, R0, Reg(7), 8), &[0xfa5f, 0xf097]),
            // sxth.w r1, r4
            (|a| a.extend(16, true, R1, Reg(4), 0), &[0xfa0f, 0xf184]),
            // sxtb.w r1, r5
            (|a| a.extend(8, true, R1, Reg(5), 0), &[0xfa4f, 0xf185]),
            // uxth.w r4, r4
            (
                |a| a.extend(16, false, Reg(4), Reg(4), 0),
                &[0xfa1f, 0xf484],
            ),
            // clz r2, r1
            (|a| a.clz(R2, R1), &[0xfab1, 0xf281]),
            // rev.w r9, r9
            (|a| a.rev(Reg(9), Reg(9)), &[0xfa99, 0xf989]),
            // mls r2, r0, r1, r4
            (|a| a.mls(R2, R0, R1, Reg(4)), &[0xfb00, 0x4211]),
            // umull r2, r3, r4, r1
            (|a| a.mull(false, R2, R3, Reg(4), R1), &[0xfba4, 0x2301]),
            // smull r2, r3, r0, r12
            (|a| a.mull(true, R2, R3, R0, R12), &[0xfb80, 0x230c]),
            // sdiv r0, r4, r1
            (|a| a.sdiv(R0, Reg(4), R1), &[0xfb94, 0xf0f1]),
            // udiv r0, r4, r1
            (|a| a.udiv(R0, Reg(4), R1), &[0xfbb4, 0xf0f1]),
            // ldr.w r12, [lr, #36]
            (|a| a.ldr(R12, LR, 36), &[0xf8de, 0xc024]),
            // str.w r0, [lr, #52]
            (|a| a.str(R0, LR, 52), &[0xf8ce, 0x0034]),
            // ldr.w r2, [r1, #0xff8]
            (|a| a.ldr(R2, R1, 0xff8), &[0xf8d1, 0x2ff8]),
            // ldr.w pc, [lr, #72]
            (|a| a.ldr(PC, LR, 72), &[0xf8de, 0xf048]),
            // ldrd r2, r3, [lr, #64]
            (|a| a.ldrd(R2, R3, LR, 64), &[0xe9de, 0x2310]),
            // ldr.w r1, [r3, r0]
            (|a| a.load_indexed(32, false, R1, R3, R0), &[0xf853, 0x1000]),
            // ldrb.w r1, [r3, r8]
            (
                |a| a.load_indexed(8, false, R1, R3, Reg(8)),
                &[0xf813, 0x1008],
            ),
            // ldrh.w r1, [r3, r0]
            (|a| a.load_indexed(16, false, R1, R3, R0), &[0xf833, 0x1000]),
            // ldrsb.w r11, [r3, r4]
            (
                |a| a.load_indexed(8, true, Reg(11), R3, Reg(4)),
                &[0xf913, 0xb004],
            ),
            // ldrsh.w r1, [r3, r0]
            (|a| a.load_indexed(16, true, R1, R3, R0), &[0xf933, 0x1000]),
            // str.w r4, [r3, r0]
            (|a| a.store_indexed(32, Reg(4), R3, R0), &[0xf843, 0x4000]),
            // strb.w r1, [r3, r0]
            (|a| a.store_indexed(8, R1, R3, R0), &[0xf803, 0x1000]),
            // strh.w r4, [r3, r8]
            (
                |a| a.store_indexed(16, Reg(4), R3, Reg(8)),
                &[0xf823, 0x4008],
            ),
            // ldm.w lr, {r4-r11}
            (
                |a| a.ldm(LR, regs(&[4, 5, 6, 7, 8, 9, 10, 11])),
                &[0xe89e, 0x0ff0],
            ),
            // stm.w lr, {r4-r11}
            (
                |a| a.stm(LR, regs(&[4, 5, 6, 7, 8, 9, 10, 11])),
                &[0xe88e, 0x0ff0],
            ),
            // push.w {r4-r12, lr}
            (
                |a| a.push(regs(&[4, 5, 6, 7, 8, 9, 10, 11, 12, 14])),
                &[0xe92d, 0x5ff0],
            ),
            // pop.w {r4-r12, pc}
            (
                |a| a.pop(regs(&[4, 5, 6, 7, 8, 9, 10, 11, 12, 15])),
                &[0xe8bd, 0x9ff0],
            ),
            // bx r1
            (|a| a.bx(R1), &[0x4708]),
            // mrs r12, apsr
            (|a| a.mrs(R12), &[0xf3ef, 0x8c00]),
            // msr apsr_nzcvq, r12
            (|a| a.msr_flags(R12), &[0xf38c, 0x8800]),
            // it eq; ldreq.w pc, [r1, #4]
            (
                |a| a.with_cond(Cond::Eq, |a| a.ldr(PC, R1, 4)),
                &[0xbf08, 0xf8d1, 0xf004],
            ),
            // it ne; orrne r12, r12, #0x10000000
            (
                |a| {
                    a.dp_cond(
                        Cond::Ne,
                        Dp::Orr,
                        false,
                        R12,
                        R12,
                        Operand::Imm(0x1000_0000),
                    )
                },
                &[0xbf18, 0xf04c, 0x5c80],
            ),
            // it lt; movlt.w r4, r1
            (
                |a| a.mov_cond(Cond::Lt, Reg(4), R1),
                &[0xbfb8, 0xea4f, 0x0401],
            ),
            // it hi; movhi.w r1, #1
            (
                |a| a.mov_cond(Cond::Hi, R1, Operand::Imm(1)),
                &[0xbf88, 0xf04f, 0x0101],
            ),
            // it eq; cmpeq.w r4, #0x80000000
            (
                |a| a.with_cond(Cond::Eq, |a| a.cmp(Reg(4), Operand::Imm(0x8000_0000))),
                &[0xbf08, 0xf1b4, 0x4f00],
            ),
            // it ne; clzne r2, r1; it ne; rsbne.w r4, r2, #31
            (
                |a| {
                    a.with_cond(Cond::Ne, |a| {
                        a.clz(R2, R1);
                        a.dp(Dp::Rsb, false, Reg(4), R2, Operand::Imm(31));
                    })
                },
                &[0xbf18, 0xfab1, 0xf281, 0xbf18, 0xf1c2, 0x041f],
            ),
        ];
        for (index, (emit, expected)) in cases.iter().enumerate() {
            assert_eq!(&emitted(*emit)[..], *expected, "case {index}");
        }
    }

    #[test]
    fn branches_resolve_forward_and_backward() {
        let mut a = Assembler::new();
        let label = a.label();
        a.b(label); // b.w +4
        a.b_cond(Cond::Ne, label); // bne.w +0
        a.bind(label);
        a.b(label); // b.w -4
        a.b_cond(Cond::Ge, label); // bge.w -8
        assert_eq!(
            &a.finish()[..],
            &[
                0xf000, 0xb802, 0xf040, 0x8000, 0xf7ff, 0xbffe, 0xf6bf, 0xaffc
            ]
        );
        // The longest conditional reach is about one megabyte either way.
        let mut a = Assembler::new();
        let label = a.label();
        a.b_cond(Cond::Eq, label);
        a.b(label);
        for _ in 0..0x2000 {
            a.mov(R0, R0);
        }
        a.bind(label);
        let code = a.finish();
        // beq.w +0x8004, b.w +0x8000
        assert_eq!(&code[..4], &[0xf008, 0x8002, 0xf008, 0xb800]);
    }

    #[test]
    fn modified_immediates() {
        for (value, expected) in [
            (0, Some(0)),
            (0xff, Some(0xff)),
            (0x00ab_00ab, Some(0x1ab)),
            (0xab00_ab00, Some(0x2ab)),
            (0xabab_abab, Some(0x3ab)),
            (0xffff_ffff, Some(0x3ff)),
            (0x2000_0000, Some(0x500)),
            (0x3000_0000, Some(0x540)),
            (0xff00, Some(0xc7f)),
            (0x3fc, Some(0xf7f)),
            (0x8000_0000, Some(0x400)),
            (0x101, None),
            (0x1234, None),
            (0x1fe0_0001, None),
        ] {
            assert_eq!(encode_imm(value), expected, "{value:#x}");
        }
    }
}
