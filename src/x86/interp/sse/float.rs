//! Scalar and packed floating-point semantics. Host arithmetic is nearest-even;
//! NaNs, conversion rounding, tiny results and x86 status are explicit.

use super::*;
use crate::x86::cpu::{EFLAGS_PF, EFLAGS_SF, EFLAGS_ZF};

const IE: u32 = 1;
const DE: u32 = 2;
const ZE: u32 = 4;
const OE: u32 = 8;
const UE: u32 = 16;

#[derive(Clone, Copy)]
struct Format {
    bits: u32,
    fraction: u32,
    bias: i32,
}
const SINGLE: Format = Format {
    bits: 32,
    fraction: 23,
    bias: 127,
};
const DOUBLE: Format = Format {
    bits: 64,
    fraction: 52,
    bias: 1023,
};

impl Format {
    fn sign(self) -> u64 {
        1 << (self.bits - 1)
    }
    fn fraction_mask(self) -> u64 {
        (1 << self.fraction) - 1
    }
    fn infinity(self) -> u64 {
        ((1 << (self.bits - self.fraction - 1)) - 1) << self.fraction
    }
    fn quiet(self) -> u64 {
        1 << (self.fraction - 1)
    }
    fn magnitude(self, a: u64) -> u64 {
        a & !self.sign()
    }
    fn nan(self, a: u64) -> bool {
        self.magnitude(a) > self.infinity()
    }
    fn snan(self, a: u64) -> bool {
        self.nan(a) && a & self.quiet() == 0
    }
    fn inf(self, a: u64) -> bool {
        self.magnitude(a) == self.infinity()
    }
    fn zero(self, a: u64) -> bool {
        self.magnitude(a) == 0
    }
    fn denormal(self, a: u64) -> bool {
        let m = self.magnitude(a);
        m != 0 && m < 1 << self.fraction
    }
    fn indefinite(self) -> u64 {
        self.sign() | self.infinity() | self.quiet()
    }
    // Finite magnitude = significand * 2^exponent, including subnormals.
    fn parts(self, a: u64) -> (u64, i32) {
        let exponent = (self.magnitude(a) >> self.fraction) as i32;
        let fraction = a & self.fraction_mask();
        if exponent == 0 {
            (fraction, 1 - self.bias - self.fraction as i32)
        } else {
            (
                fraction | (1 << self.fraction),
                exponent - self.bias - self.fraction as i32,
            )
        }
    }
}

fn format(form: Form) -> Format {
    if matches!(form, Form::P66 | Form::F2) {
        DOUBLE
    } else {
        SINGLE
    }
}
fn scalar(form: Form) -> bool {
    matches!(form, Form::F2 | Form::F3)
}
fn lane(value: [u64; 2], index: usize, f: Format) -> u64 {
    if f.bits == 64 {
        value[index]
    } else {
        (value[index / 2] >> ((index & 1) * 32)) & 0xffff_ffff
    }
}
fn put_lane(value: &mut [u64; 2], index: usize, f: Format, bits: u64) {
    if f.bits == 64 {
        value[index] = bits;
    } else {
        let shift = (index & 1) * 32;
        value[index / 2] = (value[index / 2] & !(0xffff_ffff << shift)) | (bits << shift);
    }
}

// Round an unsigned significand after a right shift. This is used only by
// conversions, whose RC still applies. No PE is exposed by the caller.
fn round_shift(value: u64, shift: u32, negative: bool, rc: u32) -> (u64, bool) {
    if shift == 0 {
        return (value, false);
    }
    let truncated = if shift < 64 { value >> shift } else { 0 };
    let remainder = if shift < 64 {
        value & ((1 << shift) - 1)
    } else {
        value
    };
    let inexact = remainder != 0;
    let increment = match rc {
        0 if shift <= 64 => {
            let halfway = 1u64 << (shift - 1);
            remainder > halfway || remainder == halfway && truncated & 1 != 0
        }
        1 => negative && inexact,
        2 => !negative && inexact,
        _ => false,
    };
    (truncated + u64::from(increment), inexact)
}

// Convert an exact finite integer significand and power of two to either
// format. One rounding step handles normal, subnormal and integer inputs.
fn from_scaled(
    f: Format,
    negative: bool,
    significand: u64,
    exponent: i32,
    rc: u32,
    ftz: bool,
) -> (u64, u32) {
    let sign = if negative { f.sign() } else { 0 };
    if significand == 0 {
        return (sign, 0);
    }
    let top = 63 - significand.leading_zeros();
    let mut output_exponent = top as i32 + exponent;
    let min_exponent = 1 - f.bias;
    if output_exponent < min_exponent {
        let shift = min_exponent - f.fraction as i32 - exponent;
        let (rounded, inexact) = if shift > 0 {
            round_shift(significand, shift as u32, negative, rc)
        } else {
            (significand << -shift, false)
        };
        let tiny = rounded < 1 << f.fraction;
        if ftz && tiny && rounded != 0 {
            return (sign, UE);
        }
        // The boundary which rounds up to exactly minimum normal is the
        // explicit tininess exception in debt SSE-3.
        return (sign | rounded, if tiny && inexact { UE } else { 0 });
    }
    let mut rounded = if top > f.fraction {
        round_shift(significand, top - f.fraction, negative, rc).0
    } else {
        significand << (f.fraction - top)
    };
    if rounded >= 1 << (f.fraction + 1) {
        rounded >>= 1;
        output_exponent += 1;
    }
    if output_exponent > f.bias {
        let infinity = rc == 0 || rc == 1 && negative || rc == 2 && !negative;
        return (
            sign | if infinity {
                f.infinity()
            } else {
                f.infinity() - 1
            },
            OE,
        );
    }
    (
        sign | (((output_exponent + f.bias) as u64) << f.fraction) | (rounded & f.fraction_mask()),
        0,
    )
}

fn convert_float(source: Format, dest: Format, value: u64, rc: u32, ftz: bool) -> (u64, u32) {
    let negative = value & source.sign() != 0;
    if source.nan(value) {
        let payload = value & source.fraction_mask();
        let payload = if dest.fraction >= source.fraction {
            payload << (dest.fraction - source.fraction)
        } else {
            payload >> (source.fraction - dest.fraction)
        };
        return (
            (if negative { dest.sign() } else { 0 }) | dest.infinity() | dest.quiet() | payload,
            if source.snan(value) { IE } else { 0 },
        );
    }
    if source.inf(value) {
        return (
            (if negative { dest.sign() } else { 0 }) | dest.infinity(),
            0,
        );
    }
    let (sig, exponent) = source.parts(value);
    let (result, flags) = from_scaled(dest, negative, sig, exponent, rc, ftz);
    (result, flags | if source.denormal(value) { DE } else { 0 })
}

fn from_i32(f: Format, value: u32, rc: u32) -> u64 {
    let value = value as i32;
    from_scaled(f, value < 0, u64::from(value.unsigned_abs()), 0, rc, false).0
}

fn to_i32(f: Format, value: u64, rc: u32) -> (u32, u32) {
    // x86 float-to-integer conversions raise IE/PE, never DE for a subnormal.
    // PE is suppressed by policy, including an ordinary rounded result.
    if f.nan(value) || f.inf(value) {
        return (0x8000_0000, IE);
    }
    let negative = value & f.sign() != 0;
    let (sig, exponent) = f.parts(value);
    if sig == 0 {
        return (0, 0);
    }
    let magnitude = if exponent >= 0 {
        if 64 - sig.leading_zeros() + exponent as u32 > 32 {
            return (0x8000_0000, IE);
        }
        sig << exponent
    } else {
        round_shift(sig, (-exponent) as u32, negative, rc).0
    };
    if magnitude > if negative { 0x8000_0000 } else { 0x7fff_ffff } {
        return (0x8000_0000, IE);
    }
    (
        if negative {
            0u32.wrapping_sub(magnitude as u32)
        } else {
            magnitude as u32
        },
        0,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Relation {
    Less,
    Equal,
    Greater,
    Unordered,
}
fn compare(f: Format, a: u64, b: u64, quiet: bool) -> (Relation, u32) {
    if f.nan(a) || f.nan(b) {
        return (
            Relation::Unordered,
            if !quiet || f.snan(a) || f.snan(b) {
                IE
            } else {
                0
            },
        );
    }
    let flags = if f.denormal(a) || f.denormal(b) {
        DE
    } else {
        0
    };
    if a == b || f.zero(a) && f.zero(b) {
        return (Relation::Equal, flags);
    }
    let less = if (a ^ b) & f.sign() != 0 {
        a & f.sign() != 0
    } else if a & f.sign() != 0 {
        a > b
    } else {
        a < b
    };
    (
        if less {
            Relation::Less
        } else {
            Relation::Greater
        },
        flags,
    )
}
fn predicate(relation: Relation, predicate: u8) -> bool {
    use Relation::*;
    match predicate & 7 {
        0 => relation == Equal,
        1 => relation == Less,
        2 => matches!(relation, Less | Equal),
        3 => relation == Unordered,
        4 => relation != Equal,
        5 => relation != Less,
        6 => !matches!(relation, Less | Equal),
        _ => relation != Unordered,
    }
}

fn same_scaled(a: u128, a_exp: i32, b: u128, b_exp: i32) -> bool {
    if a == 0 || b == 0 {
        return a == b;
    }
    let a_zeros = a.trailing_zeros();
    let b_zeros = b.trailing_zeros();
    a >> a_zeros == b >> b_zeros && a_exp + a_zeros as i32 == b_exp + b_zeros as i32
}

fn tiny_inexact(f: Format, opcode: u8, a: u64, b: u64, result: u64) -> bool {
    // A same-format tiny add/sub result is exact. Roots of nonzero finite
    // f32/f64 values are normal, so sqrt needs no tiny-result check.
    if !matches!(opcode, 0x59 | 0x5e) {
        return false;
    }
    if f.bits == 32 {
        let a = f64::from(f32::from_bits(a as u32));
        let b = f64::from(f32::from_bits(b as u32));
        let result = f64::from(f32::from_bits(result as u32));
        return if opcode == 0x59 {
            a * b != result
        } else {
            result * b != a
        };
    }
    let (a, ae) = f.parts(a);
    let (b, be) = f.parts(b);
    let (r, re) = f.parts(result);
    if opcode == 0x59 {
        !same_scaled(u128::from(a) * u128::from(b), ae + be, u128::from(r), re)
    } else {
        !same_scaled(u128::from(r) * u128::from(b), re + be, u128::from(a), ae)
    }
}

fn binary(f: Format, opcode: u8, a: u64, b: u64, ftz: bool) -> (u64, u32) {
    if matches!(opcode, 0x5d | 0x5f) {
        let (relation, flags) = compare(f, a, b, false);
        let take_a = if opcode == 0x5d {
            relation == Relation::Less
        } else {
            relation == Relation::Greater
        };
        return (if take_a { a } else { b }, flags);
    }
    if f.nan(a) || f.nan(b) {
        return (
            (if f.nan(a) { a } else { b }) | f.quiet(),
            if f.snan(a) || f.snan(b) { IE } else { 0 },
        );
    }
    let invalid = match opcode {
        0x58 => f.inf(a) && f.inf(b) && (a ^ b) & f.sign() != 0,
        0x5c => f.inf(a) && f.inf(b) && (a ^ b) & f.sign() == 0,
        0x59 => f.inf(a) && f.zero(b) || f.zero(a) && f.inf(b),
        0x5e => f.zero(a) && f.zero(b) || f.inf(a) && f.inf(b),
        _ => unreachable!(),
    };
    if invalid {
        return (f.indefinite(), IE);
    }
    // Divide-by-zero precedes a denormal numerator; infinity / zero is not ZE.
    if opcode == 0x5e && f.zero(b) && !f.inf(a) {
        return (((a ^ b) & f.sign()) | f.infinity(), ZE);
    }
    let mut flags = if f.denormal(a) || f.denormal(b) {
        DE
    } else {
        0
    };
    let mut result = if f.bits == 32 {
        let (a, b) = (f32::from_bits(a as u32), f32::from_bits(b as u32));
        u64::from(
            match opcode {
                0x58 => a + b,
                0x5c => a - b,
                0x59 => a * b,
                _ => a / b,
            }
            .to_bits(),
        )
    } else {
        let (a, b) = (f64::from_bits(a), f64::from_bits(b));
        match opcode {
            0x58 => a + b,
            0x5c => a - b,
            0x59 => a * b,
            _ => a / b,
        }
        .to_bits()
    };
    if !f.inf(a) && !f.inf(b) {
        if f.inf(result) {
            flags |= OE;
        } else if f.magnitude(result) < 1 << f.fraction && tiny_inexact(f, opcode, a, b, result) {
            flags |= UE;
        }
    }
    if ftz && f.denormal(result) {
        result &= f.sign();
        flags |= UE;
    }
    (result, flags)
}

// Restoring base-4 square root: only shifts/subtracts, at most 53 iterations
// for these radicands. The remainder decides rounding without host sqrt.
fn integer_sqrt(mut value: u128) -> (u64, u128) {
    let mut bit = 1u128 << ((127 - value.leading_zeros()) & !1);
    let mut root = 0u128;
    while bit != 0 {
        if value >= root + bit {
            value -= root + bit;
            root = (root >> 1) + bit;
        } else {
            root >>= 1;
        }
        bit >>= 2;
    }
    (root as u64, value)
}
fn sqrt(f: Format, value: u64) -> (u64, u32) {
    if f.nan(value) {
        return (value | f.quiet(), if f.snan(value) { IE } else { 0 });
    }
    if f.zero(value) {
        return (value, 0);
    }
    if value & f.sign() != 0 {
        return (f.indefinite(), IE);
    }
    if f.inf(value) {
        return (value, 0);
    }
    let (mut sig, mut exponent) = f.parts(value);
    if exponent & 1 != 0 {
        sig <<= 1;
        exponent -= 1;
    }
    let half_top = (63 - sig.leading_zeros()) / 2;
    let radicand = u128::from(sig) << (2 * (f.fraction - half_top));
    let (mut rounded, remainder) = integer_sqrt(radicand);
    // sqrt(n) is above q+1/2 iff n-q*q > q. Integer radicands cannot tie.
    if remainder > u128::from(rounded) {
        rounded += 1;
    }
    let mut output_exponent = half_top as i32 + exponent / 2;
    if rounded >= 1 << (f.fraction + 1) {
        rounded >>= 1;
        output_exponent += 1;
    }
    (
        (((output_exponent + f.bias) as u64) << f.fraction) | (rounded & f.fraction_mask()),
        if f.denormal(value) { DE } else { 0 },
    )
}

fn reciprocal(opcode: u8, value: u64) -> u64 {
    let f = SINGLE;
    if f.nan(value) {
        return value | f.quiet();
    }
    if f.zero(value) || f.denormal(value) {
        return (value & f.sign()) | f.infinity();
    }
    if opcode == 0x52 && value & f.sign() != 0 {
        return f.indefinite();
    }
    if f.inf(value) {
        return value & f.sign();
    }
    let divisor = if opcode == 0x52 {
        sqrt(f, value).0
    } else {
        value
    };
    let result = u64::from((1.0f32 / f32::from_bits(divisor as u32)).to_bits());
    if f.denormal(result) {
        result & f.sign()
    } else {
        result
    }
}

pub(super) fn execute<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    form: Form,
    opcode: u8,
    modrm: u8,
    address16: bool,
    eip: u32,
) -> Result<(), StepError> {
    use Form::*;
    let reg = usize::from((modrm >> 3) & 7);
    let f = format(form);
    let rc = (cpu.fpu.mxcsr >> 13) & 3;
    let ftz = cpu.fpu.mxcsr & 0x8000 != 0;
    match (opcode, form) {
        (0x51 | 0x58..=0x59 | 0x5c..=0x5f | 0xc2, _) | (0x52..=0x53, None | F3) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let predicate = if opcode == 0xc2 {
                fetch_u8(cpu, bus)? & 7
            } else {
                0
            };
            let width = if scalar(form) { f.bits as u8 / 8 } else { 16 };
            let src = read_xmm(cpu, bus, rm, width, !scalar(form), eip)?;
            let mut result = cpu.xmm[reg];
            let mut flags = 0;
            for i in 0..if scalar(form) {
                1
            } else {
                128 / f.bits as usize
            } {
                let a = lane(result, i, f);
                let b = lane(src, i, f);
                let (value, raised) = match opcode {
                    0x51 => sqrt(f, b),
                    0x52..=0x53 => (reciprocal(opcode, b), 0),
                    0xc2 => {
                        let quiet = matches!(predicate, 0 | 3 | 4 | 7);
                        let (relation, raised) = compare(f, a, b, quiet);
                        (
                            if self::predicate(relation, predicate) {
                                u64::MAX >> (64 - f.bits)
                            } else {
                                0
                            },
                            raised,
                        )
                    }
                    _ => binary(f, opcode, a, b, ftz),
                };
                put_lane(&mut result, i, f, value);
                flags |= raised;
            }
            sse_record_exceptions(cpu, flags);
            cpu.xmm[reg] = result;
        }
        (0x54..=0x57, None | P66) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let src = read_xmm(cpu, bus, rm, 16, true, eip)?;
            for (dst, src) in cpu.xmm[reg].iter_mut().zip(src) {
                *dst = match opcode {
                    0x54 => *dst & src,
                    0x55 => !*dst & src,
                    0x56 => *dst | src,
                    _ => *dst ^ src,
                };
            }
        }
        (0x2e..=0x2f, None | P66) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let src = read_xmm(cpu, bus, rm, f.bits as u8 / 8, false, eip)?;
            let (relation, flags) =
                compare(f, lane(cpu.xmm[reg], 0, f), lane(src, 0, f), opcode == 0x2e);
            let status = match relation {
                Relation::Less => EFLAGS_CF,
                Relation::Equal => EFLAGS_ZF,
                Relation::Greater => 0,
                Relation::Unordered => EFLAGS_CF | EFLAGS_PF | EFLAGS_ZF,
            };
            cpu.materialize_flags();
            cpu.eflags = (cpu.eflags
                & !(EFLAGS_CF | EFLAGS_PF | EFLAGS_AF | EFLAGS_ZF | EFLAGS_SF | EFLAGS_OF))
                | status;
            sse_record_exceptions(cpu, flags);
        }
        (0x2a, _) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let mut result = cpu.xmm[reg];
            if scalar(form) {
                let value = read_rm32(cpu, bus, rm)?;
                put_lane(&mut result, 0, f, from_i32(f, value, rc));
            } else {
                let value = read_mmx_rm_qword(cpu, bus, rm, eip)?;
                for i in 0..2 {
                    put_lane(
                        &mut result,
                        i,
                        f,
                        from_i32(f, (value >> (i * 32)) as u32, rc),
                    );
                }
                if matches!(rm, Rm32::Reg(_)) {
                    cpu.fpu.prepare_mmx();
                }
            }
            cpu.xmm[reg] = result;
        }
        (0x2c..=0x2d, _) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let width = f.bits as u8 / 8 * if scalar(form) { 1 } else { 2 };
            let src = read_xmm(cpu, bus, rm, width, width == 16, eip)?;
            let rounding = if opcode == 0x2c { 3 } else { rc };
            let (low, mut flags) = to_i32(f, lane(src, 0, f), rounding);
            if scalar(form) {
                cpu.set_reg(Reg32::from_u3(reg as u8), low);
            } else {
                let (high, raised) = to_i32(f, lane(src, 1, f), rounding);
                flags |= raised;
                cpu.fpu
                    .set_mmx(reg as u8, u64::from(low) | (u64::from(high) << 32));
            }
            sse_record_exceptions(cpu, flags);
        }
        (0x5a, _) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let dest = if f.bits == 32 { DOUBLE } else { SINGLE };
            let count = if scalar(form) { 1 } else { 2 };
            let width = f.bits as u8 / 8 * count as u8;
            let src = read_xmm(cpu, bus, rm, width, width == 16, eip)?;
            let mut result = if scalar(form) { cpu.xmm[reg] } else { [0; 2] };
            let mut flags = 0;
            for i in 0..count {
                let (value, raised) = convert_float(f, dest, lane(src, i, f), rc, ftz);
                put_lane(&mut result, i, dest, value);
                flags |= raised;
            }
            sse_record_exceptions(cpu, flags);
            cpu.xmm[reg] = result;
        }
        (0x5b, None | P66 | F3) | (0xe6, P66 | F2 | F3) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let float = if opcode == 0xe6 { DOUBLE } else { SINGLE };
            let to_float = (opcode == 0x5b && form == None) || (opcode == 0xe6 && form == F3);
            let width = if opcode == 0xe6 && to_float { 8 } else { 16 };
            let src = read_xmm(cpu, bus, rm, width, width == 16, eip)?;
            let mut result = [0; 2];
            let mut flags = 0;
            let rounding = if (opcode == 0x5b && form == F3) || (opcode == 0xe6 && form == P66) {
                3
            } else {
                rc
            };
            for i in 0..if opcode == 0xe6 { 2 } else { 4 } {
                if to_float {
                    put_lane(
                        &mut result,
                        i,
                        float,
                        from_i32(float, lane(src, i, SINGLE) as u32, rc),
                    );
                } else {
                    let (value, raised) = to_i32(float, lane(src, i, float), rounding);
                    put_lane(&mut result, i, SINGLE, u64::from(value));
                    flags |= raised;
                }
            }
            sse_record_exceptions(cpu, flags);
            cpu.xmm[reg] = result;
        }
        _ => return Err(StepError::InvalidOpcode { eip }),
    }
    Ok(())
}
