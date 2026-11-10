//! AArch64 encodings used by the i386 translator. There is no generic host ISA.
//! Register 31 denotes SP or ZR as specified by the particular instruction.
//! Internal register/immediate mistakes are compiler bugs, checked at emission.
//! Label fixups are word-relative to the branch instruction, not its successor.

use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reg(u8);

impl Reg {
    pub const fn new(index: u8) -> Self {
        assert!(index < 32);
        Self(index)
    }

    const fn bits(self) -> u32 {
        self.0 as u32
    }
}

pub const ZR: Reg = Reg::new(31);
pub const SP: Reg = Reg::new(31);
pub const LR: Reg = Reg::new(30);

/// B imm26 for an already published target, measured from the branch itself.
pub fn branch_immediate(byte_delta: isize) -> u32 {
    assert!(byte_delta & 3 == 0 && (-(1 << 27)..(1 << 27)).contains(&byte_delta));
    0x1400_0000 | (((byte_delta >> 2) as u32) & 0x03ff_ffff)
}

/// Retarget a branch while preserving its condition, operand and CALL hint.
/// Short conditional branches may keep their existing local trampoline when
/// the final target is out of range; no published instruction changes then.
pub fn retarget_branch(word: u32, byte_delta: isize) -> Option<u32> {
    assert!(byte_delta & 3 == 0);
    let (bits, shift) = if word & 0x7c00_0000 == 0x1400_0000 {
        (26, 0) // B / BL.
    } else if word & 0xff00_0010 == 0x5400_0000 || word & 0x7e00_0000 == 0x3400_0000 {
        (19, 5) // B.cond / CBZ / CBNZ.
    } else if word & 0x7e00_0000 == 0x3600_0000 {
        (14, 5) // TBZ / TBNZ.
    } else {
        panic!("JIT link is not a supported branch");
    };
    let delta = byte_delta / 4;
    let bound = 1_isize << (bits - 1);
    if !(-bound..bound).contains(&delta) {
        return None;
    }
    if bits == 26 {
        return Some(branch_immediate(byte_delta) | (word & 0x8000_0000));
    }
    let mask = (1u32 << bits) - 1;
    Some((word & !(mask << shift)) | (((delta as u32) & mask) << shift))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
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
}

#[derive(Clone, Copy, Debug)]
pub struct Label(u32);

struct Fixup {
    instruction: usize,
    label: Label,
    bits: u8,
    shift: u8,
}

/// A relative reference whose source and target lie in different code regions.
/// Both indices are in words in the original contiguous emission.
pub struct Relocation {
    source: usize,
    target: usize,
    bits: u8,
    shift: u8,
}

impl Relocation {
    pub fn split(&self, words: &mut [u32], split: usize, cold_delta: usize) {
        let offset = |word| word * 4 + if word >= split { cold_delta } else { 0 };
        let delta = (offset(self.target) as i64 - offset(self.source) as i64) / 4;
        let bound = 1_i64 << (self.bits - 1);
        assert!(
            (-bound..bound).contains(&delta),
            "packed JIT branch out of range"
        );
        let mask = (1_u32 << self.bits) - 1;
        words[self.source] =
            (words[self.source] & !(mask << self.shift)) | (((delta as u32) & mask) << self.shift);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmitError {
    UnboundLabel,
    BranchOutOfRange,
}

#[derive(Default)]
pub struct Assembler {
    words: Vec<u32>,
    labels: Vec<Option<usize>>,
    fixups: Vec<Fixup>,
    tails: Vec<super::cold::Path>,
}

impl Assembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hints only: bounded x86 blocks have enough known structure to avoid
    /// repeatedly growing these vectors during cold-boot compilation.
    pub fn with_capacity(words: usize, branches: usize) -> Self {
        Self {
            words: Vec::with_capacity(words),
            labels: Vec::with_capacity(branches),
            fixups: Vec::with_capacity(branches),
            tails: Vec::new(),
        }
    }

    pub fn offset(&self) -> usize {
        self.words.len() * 4
    }

    fn emit(&mut self, word: u32) {
        self.words.push(word);
    }

    pub fn label(&mut self) -> Label {
        let label = Label(u32::try_from(self.labels.len()).expect("too many native labels"));
        self.labels.push(None);
        label
    }

    pub fn bind(&mut self, label: Label) {
        assert!(
            self.labels[label.0 as usize]
                .replace(self.words.len())
                .is_none()
        );
    }

    fn branch(&mut self, word: u32, label: Label, bits: u8, shift: u8) {
        self.fixups.push(Fixup {
            instruction: self.words.len(),
            label,
            bits,
            shift,
        });
        self.emit(word);
    }

    pub fn b(&mut self, label: Label) {
        self.branch(0x1400_0000, label, 26, 0);
    }

    pub fn bl(&mut self, label: Label) {
        self.branch(0x9400_0000, label, 26, 0);
    }

    /// All emitted labels and PCs are word aligned, so ADR's low two immediate
    /// bits are zero and its high field uses the ordinary word-relative fixup.
    pub fn adr(&mut self, dst: Reg, label: Label) {
        self.branch(0x1000_0000 | dst.bits(), label, 19, 5);
    }

    pub fn b_cond(&mut self, cond: Cond, label: Label) {
        self.branch(0x5400_0000 | cond as u32, label, 19, 5);
    }

    pub fn cbz(&mut self, reg: Reg, wide: bool, nonzero: bool, label: Label) {
        self.branch(
            0x3400_0000 | ((wide as u32) << 31) | ((nonzero as u32) << 24) | reg.bits(),
            label,
            19,
            5,
        );
    }

    pub fn tbz(&mut self, reg: Reg, bit: u8, nonzero: bool, label: Label) {
        assert!(bit < 64);
        self.branch(
            0x3600_0000
                | (u32::from(bit >> 5) << 31)
                | ((nonzero as u32) << 24)
                | (u32::from(bit & 31) << 19)
                | reg.bits(),
            label,
            14,
            5,
        );
    }

    pub(super) fn tail(&mut self, path: super::cold::Path) {
        self.tails.push(path);
    }

    /// Emit deferred paths before deciding which exits have callers.
    pub fn flush_tails(&mut self) {
        for path in core::mem::take(&mut self.tails) {
            path.emit(self);
        }
    }

    /// Includes references from deferred paths only after `flush_tails`.
    pub fn is_referenced(&self, label: Label) -> bool {
        self.fixups.iter().any(|fixup| fixup.label.0 == label.0)
    }

    pub fn finish(self) -> Result<Vec<u32>, EmitError> {
        Ok(self.finish_split(usize::MAX)?.0)
    }

    pub fn finish_split(mut self, split: usize) -> Result<(Vec<u32>, Vec<Relocation>), EmitError> {
        self.flush_tails();
        let mut relocations = Vec::new();
        for fixup in self.fixups {
            let target = self.labels[fixup.label.0 as usize].ok_or(EmitError::UnboundLabel)?;
            let delta = target as i64 - fixup.instruction as i64;
            let bound = 1_i64 << (fixup.bits - 1);
            if !(-bound..bound).contains(&delta) {
                return Err(EmitError::BranchOutOfRange);
            }
            let mask = (1_u32 << fixup.bits) - 1;
            self.words[fixup.instruction] |= ((delta as u32) & mask) << fixup.shift;
            if (fixup.instruction < split) != (target < split) {
                relocations.push(Relocation {
                    source: fixup.instruction,
                    target,
                    bits: fixup.bits,
                    shift: fixup.shift,
                });
            }
        }
        Ok((self.words, relocations))
    }

    pub fn ret(&mut self) {
        self.ret_to(LR);
    }

    pub fn ret_to(&mut self, reg: Reg) {
        self.emit(0xd65f_0000 | (reg.bits() << 5));
    }

    pub fn br(&mut self, reg: Reg) {
        self.emit(0xd61f_0000 | (reg.bits() << 5));
    }

    pub fn blr(&mut self, reg: Reg) {
        self.emit(0xd63f_0000 | (reg.bits() << 5));
    }

    pub fn mov(&mut self, dst: Reg, src: Reg, wide: bool) {
        self.emit(0x2a00_03e0 | ((wide as u32) << 31) | (src.bits() << 16) | dst.bits());
    }

    pub fn mov_imm(&mut self, dst: Reg, value: u64, wide: bool) {
        assert!(wide || value <= u32::MAX as u64);
        let halves = if wide { 4 } else { 2 };
        // A MOVZ always defines every bit, including when all halves are zero.
        let first = (0..halves)
            .find(|part| (value >> (part * 16)) & 0xffff != 0)
            .unwrap_or(0);
        self.emit(
            0x5280_0000
                | ((wide as u32) << 31)
                | (first << 21)
                | (((value >> (first * 16)) as u32 & 0xffff) << 5)
                | dst.bits(),
        );
        for part in 0..halves {
            let half = (value >> (part * 16)) as u32 & 0xffff;
            if part != first && half != 0 {
                self.emit(
                    0x7280_0000 | ((wide as u32) << 31) | (part << 21) | (half << 5) | dst.bits(),
                );
            }
        }
    }

    pub fn add_sub(&mut self, dst: Reg, lhs: Reg, rhs: Reg, wide: bool, sub: bool, flags: bool) {
        self.emit(
            0x0b00_0000
                | ((wide as u32) << 31)
                | ((sub as u32) << 30)
                | ((flags as u32) << 29)
                | (rhs.bits() << 16)
                | (lhs.bits() << 5)
                | dst.bits(),
        );
    }

    pub fn add_shifted(&mut self, dst: Reg, lhs: Reg, rhs: Reg, shift: u8, wide: bool) {
        assert!(shift < if wide { 64 } else { 32 });
        self.emit(
            0x0b00_0000
                | ((wide as u32) << 31)
                | (rhs.bits() << 16)
                | (u32::from(shift) << 10)
                | (lhs.bits() << 5)
                | dst.bits(),
        );
    }

    pub fn compare_lsr64(&mut self, lhs: Reg, rhs: Reg, shift: u8) {
        assert!(shift < 64);
        self.emit(0xeb40_001f | (rhs.bits() << 16) | (u32::from(shift) << 10) | (lhs.bits() << 5));
    }

    pub fn add_lsr64(&mut self, dst: Reg, lhs: Reg, rhs: Reg, shift: u8) {
        assert!(shift < 64);
        self.emit(
            0x8b40_0000
                | (rhs.bits() << 16)
                | (u32::from(shift) << 10)
                | (lhs.bits() << 5)
                | dst.bits(),
        );
    }

    /// Signed/unsigned 32-bit operands, exact 64-bit product (SMULL/UMULL).
    pub fn multiply_long(&mut self, dst: Reg, lhs: Reg, rhs: Reg, signed: bool) {
        self.emit(
            0x9b20_7c00
                | ((!signed as u32) << 23)
                | (rhs.bits() << 16)
                | (lhs.bits() << 5)
                | dst.bits(),
        );
    }

    pub fn divide64(&mut self, dst: Reg, lhs: Reg, rhs: Reg, signed: bool) {
        self.emit(
            0x9ac0_0800
                | ((signed as u32) << 10)
                | (rhs.bits() << 16)
                | (lhs.bits() << 5)
                | dst.bits(),
        );
    }

    /// MSUB: subtract the low 64 bits of lhs*rhs from the accumulator.
    pub fn multiply_subtract64(&mut self, dst: Reg, lhs: Reg, rhs: Reg, accumulator: Reg) {
        self.emit(
            0x9b00_8000
                | (rhs.bits() << 16)
                | (accumulator.bits() << 10)
                | (lhs.bits() << 5)
                | dst.bits(),
        );
    }

    pub fn reverse_bits32(&mut self, dst: Reg, src: Reg) {
        self.emit(0x5ac0_0000 | (src.bits() << 5) | dst.bits());
    }

    pub fn reverse_bytes32(&mut self, dst: Reg, src: Reg) {
        self.emit(0x5ac0_0800 | (src.bits() << 5) | dst.bits());
    }

    pub fn leading_zeros32(&mut self, dst: Reg, src: Reg) {
        self.emit(0x5ac0_1000 | (src.bits() << 5) | dst.bits());
    }

    /// ADC/SBC consumes NZCV.C; subtraction uses ARM's no-borrow convention.
    pub fn add_sub_carry(
        &mut self,
        dst: Reg,
        lhs: Reg,
        rhs: Reg,
        wide: bool,
        sub: bool,
        flags: bool,
    ) {
        self.emit(
            0x1a00_0000
                | ((wide as u32) << 31)
                | ((sub as u32) << 30)
                | ((flags as u32) << 29)
                | (rhs.bits() << 16)
                | (lhs.bits() << 5)
                | dst.bits(),
        );
    }

    pub fn add_sub_imm(
        &mut self,
        dst: Reg,
        lhs: Reg,
        imm: u16,
        wide: bool,
        sub: bool,
        flags: bool,
    ) {
        assert!(imm < 4096);
        self.emit(
            0x1100_0000
                | ((wide as u32) << 31)
                | ((sub as u32) << 30)
                | ((flags as u32) << 29)
                | ((imm as u32) << 10)
                | (lhs.bits() << 5)
                | dst.bits(),
        );
    }

    /// opc: AND=0, ORR=1, EOR=2, ANDS=3. No complement or shifted operand.
    pub fn logical(&mut self, opc: u8, dst: Reg, lhs: Reg, rhs: Reg, wide: bool) {
        assert!(opc < 4);
        self.emit(
            0x0a00_0000
                | ((wide as u32) << 31)
                | ((opc as u32) << 29)
                | (rhs.bits() << 16)
                | (lhs.bits() << 5)
                | dst.bits(),
        );
    }

    /// EOR with one immediate bit. Used to normalize x86's borrow flag without
    /// materializing a mask in another register.
    pub fn xor_bit(&mut self, dst: Reg, src: Reg, bit: u8, wide: bool) {
        let bits = if wide { 64 } else { 32 };
        assert!(bit < bits);
        self.emit(
            0x5200_0000
                | ((wide as u32) << 31)
                | ((wide as u32) << 22)
                | (u32::from((bits - bit) % bits) << 16)
                | (src.bits() << 5)
                | dst.bits(),
        );
    }

    /// AND with all bits at/above `lowest` set, for a page-aligned tag.
    pub fn and_high_bits(&mut self, dst: Reg, src: Reg, lowest: u8, wide: bool) {
        let bits = if wide { 64 } else { 32 };
        assert!(lowest > 0 && lowest < bits);
        self.emit(
            0x1200_0000
                | ((wide as u32) << 31)
                | ((wide as u32) << 22)
                | (u32::from(bits - lowest) << 16)
                | (u32::from(bits - lowest - 1) << 10)
                | (src.bits() << 5)
                | dst.bits(),
        );
    }

    /// UBFM/SBFM encodes immediate shifts and unsigned/signed extraction.
    pub fn bitfield(&mut self, dst: Reg, src: Reg, immr: u8, imms: u8, wide: bool, signed: bool) {
        let bits = if wide { 64 } else { 32 };
        assert!(immr < bits && imms < bits);
        let opc = if signed { 0x1300_0000 } else { 0x5300_0000 };
        self.emit(
            opc | ((wide as u32) << 31)
                | ((wide as u32) << 22)
                | ((immr as u32) << 16)
                | ((imms as u32) << 10)
                | (src.bits() << 5)
                | dst.bits(),
        );
    }

    pub fn bfi32(&mut self, dst: Reg, src: Reg, lsb: u8, width: u8) {
        assert!(width != 0 && lsb < 32 && u16::from(lsb) + u16::from(width) <= 32);
        self.emit(
            0x3300_0000
                | (((32 - lsb as u32) & 31) << 16)
                | ((width as u32 - 1) << 10)
                | (src.bits() << 5)
                | dst.bits(),
        );
    }

    pub fn cset(&mut self, dst: Reg, cond: Cond) {
        self.emit(0x1a9f_07e0 | (((cond as u32) ^ 1) << 12) | dst.bits());
    }

    pub fn select32(&mut self, dst: Reg, yes: Reg, no: Reg, cond: Cond) {
        self.emit(
            0x1a80_0000
                | (no.bits() << 16)
                | ((cond as u32) << 12)
                | (yes.bits() << 5)
                | dst.bits(),
        );
    }

    /// Variable LSL/LSR/ASR/ROR (kind 0..3); AArch64 masks the shift count to
    /// five bits for W registers or six for X registers.
    pub fn shift_reg(&mut self, dst: Reg, lhs: Reg, count: Reg, wide: bool, kind: u8) {
        assert!(kind < 4);
        self.emit(
            0x1ac0_2000
                | ((wide as u32) << 31)
                | ((kind as u32) << 10)
                | (count.bits() << 16)
                | (lhs.bits() << 5)
                | dst.bits(),
        );
    }

    /// Unsigned immediate addressing; log2_bytes is 0/1/2/3 for B/H/W/X.
    pub fn load_store(&mut self, load: bool, reg: Reg, base: Reg, offset: u32, log2_bytes: u8) {
        assert!(log2_bytes <= 3 && offset & ((1 << log2_bytes) - 1) == 0);
        let imm = offset >> log2_bytes;
        assert!(imm < 4096);
        self.emit(
            0x3900_0000
                | ((log2_bytes as u32) << 30)
                | ((load as u32) << 22)
                | (imm << 10)
                | (base.bits() << 5)
                | reg.bits(),
        );
    }

    /// Byte offset in an X register, without scaling or base writeback.
    pub fn load_store_offset(
        &mut self,
        load: bool,
        reg: Reg,
        base: Reg,
        offset: Reg,
        log2_bytes: u8,
    ) {
        assert!(log2_bytes <= 3);
        self.emit(
            0x3820_6800
                | ((log2_bytes as u32) << 30)
                | ((load as u32) << 22)
                | (offset.bits() << 16)
                | (base.bits() << 5)
                | reg.bits(),
        );
    }

    /// Store with a signed byte offset, without alignment scaling or writeback.
    pub fn store_unscaled(&mut self, reg: Reg, base: Reg, offset: i16, log2_bytes: u8) {
        assert!(log2_bytes <= 3 && (-256..=255).contains(&offset));
        self.emit(
            0x3800_0000
                | ((log2_bytes as u32) << 30)
                | (((offset as u32) & 0x1ff) << 12)
                | (base.bits() << 5)
                | reg.bits(),
        );
    }

    /// LDRSW X with an X-register index scaled by the word size.
    pub fn load_i32_indexed(&mut self, dst: Reg, base: Reg, index: Reg) {
        self.emit(0xb8a0_7800 | (index.bits() << 16) | (base.bits() << 5) | dst.bits());
    }

    pub fn clear_bits32(&mut self, dst: Reg, lhs: Reg, mask: Reg) {
        self.emit(0x0a20_0000 | (mask.bits() << 16) | (lhs.bits() << 5) | dst.bits());
    }

    /// STP/LDP with signed scaled offset, without base writeback.
    pub fn pair(
        &mut self,
        load: bool,
        first: Reg,
        second: Reg,
        base: Reg,
        offset: i16,
        wide: bool,
    ) {
        let scale = if wide { 8 } else { 4 };
        assert!(offset % scale == 0 && (-64..64).contains(&(offset / scale)));
        let imm = (offset / scale) as u32 & 127;
        self.emit(
            0x2900_0000
                | ((wide as u32) << 31)
                | ((load as u32) << 22)
                | (imm << 15)
                | (second.bits() << 10)
                | (base.bits() << 5)
                | first.bits(),
        );
    }

    /// 64-bit STP/LDP with pre/post-indexed base update. Pair operands must
    /// not overlap the base: that writeback overlap is architecturally unsafe.
    pub fn pair_writeback(
        &mut self,
        load: bool,
        first: Reg,
        second: Reg,
        base: Reg,
        offset: i16,
        pre: bool,
    ) {
        assert!(first != base && second != base);
        assert!(offset % 8 == 0 && (-64..64).contains(&(offset / 8)));
        self.emit(
            0xa880_0000
                | ((pre as u32) << 24)
                | ((load as u32) << 22)
                | (((offset / 8) as u32 & 127) << 15)
                | (second.bits() << 10)
                | (base.bits() << 5)
                | first.bits(),
        );
    }

    pub fn mrs_nzcv(&mut self, dst: Reg) {
        self.emit(0xd53b_4200 | dst.bits());
    }

    pub fn msr_nzcv(&mut self, src: Reg) {
        self.emit(0xd51b_4200 | src.bits());
    }
}

#[cfg(test)]
mod tests;
