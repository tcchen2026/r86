//! SSE/SSE2 interpretation, reached only by the corresponding 0F dispatch arms.
//! Mandatory prefixes are literal bytes, independent of CS default operand size.

mod float;

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    None,
    P66,
    F3,
    F2,
}

#[inline(never)]
pub(super) fn execute<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    prefix: Prefix,
    operand_size_prefix: bool,
    opcode: u8,
    address16: bool,
    eip: u32,
) -> Result<(), StepError> {
    // As in Bochs fetchdecode32, F2/F3 select the form even with a 66 prefix.
    let form = match prefix {
        Prefix::Rep => Form::F3,
        Prefix::Repne => Form::F2,
        Prefix::None if operand_size_prefix => Form::P66,
        Prefix::None => Form::None,
    };
    let modrm = fetch_u8(cpu, bus)?;
    let reg = ((modrm >> 3) & 7) as usize;
    use Form::*;
    match (opcode, form) {
        (0x2a | 0x2c..=0x2f | 0x51..=0x5f | 0xc2 | 0xe6, _) => {
            return float::execute(cpu, bus, form, opcode, modrm, address16, eip);
        }
        // MOVUPS/UPD/SS/SD, MOVAPS/APD, MOVDQA/DQU.
        (0x10..=0x11, _) | (0x28..=0x29, None | P66) | (0x6f | 0x7f, P66 | F3) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let width = match (opcode, form) {
                (0x10..=0x11, F3) => 4,
                (0x10..=0x11, F2) => 8,
                _ => 16,
            };
            let aligned =
                matches!(opcode, 0x28..=0x29) || matches!(opcode, 0x6f | 0x7f) && form == P66;
            if matches!(opcode, 0x10 | 0x28 | 0x6f) {
                let value = read_xmm(cpu, bus, rm, width, aligned, eip)?;
                // Scalar loads from memory zero the rest; register moves merge.
                cpu.xmm[reg] = if matches!(rm, Rm32::Reg(_)) {
                    merge_scalar(cpu.xmm[reg], value, width)
                } else {
                    value
                };
            } else {
                write_xmm(cpu, bus, rm, cpu.xmm[reg], width, aligned, eip)?;
            }
        }
        // MOVLPS/LPD/HPS/HPD; register forms are MOVHLPS/MOVLHPS.
        (0x12 | 0x16, None | P66) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let value = match rm {
                Rm32::Reg(src) if form == None => {
                    cpu.xmm[src as usize][usize::from(opcode == 0x12)]
                }
                Rm32::Reg(_) => return Err(StepError::InvalidOpcode { eip }),
                Rm32::Mem(addr) => read_mmx_mem_qword(cpu, bus, addr, eip)?,
            };
            cpu.xmm[reg][usize::from(opcode == 0x16)] = value;
        }
        (0x13 | 0x17, None | P66) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let addr = memory(rm, eip)?;
            write_mmx_mem_qword(
                cpu,
                bus,
                addr,
                cpu.xmm[reg][usize::from(opcode == 0x17)],
                eip,
            )?;
        }
        (0x14..=0x15, None | P66) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let src = read_xmm(cpu, bus, rm, 16, true, eip)?;
            let half = (opcode & 1) as usize;
            let a = cpu.xmm[reg][half];
            let b = src[half];
            cpu.xmm[reg] = if form == P66 {
                [a, b]
            } else {
                [mmx_binary_result(0x62, a, b), mmx_binary_result(0x6a, a, b)]
            };
        }
        (0x6e, P66) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            cpu.xmm[reg] = [u64::from(read_rm32(cpu, bus, rm)?), 0];
        }
        (0x7e, P66) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            write_rm32(cpu, bus, rm, cpu.xmm[reg][0] as u32)?;
        }
        (0x7e, F3) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let value = read_xmm(cpu, bus, rm, 8, false, eip)?;
            cpu.xmm[reg] = [value[0], 0];
        }
        (0xd6, P66) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let value = cpu.xmm[reg][0];
            match rm {
                Rm32::Reg(dst) => cpu.xmm[dst as usize] = [value, 0],
                Rm32::Mem(addr) => write_mmx_mem_qword(cpu, bus, addr, value, eip)?,
            }
        }
        (0xd6, F2 | F3) => {
            sse_available(cpu, eip)?;
            let src = register(modrm, eip)?;
            if form == F3 {
                let value = cpu.fpu.mmx(src as u8);
                cpu.fpu.prepare_mmx();
                cpu.xmm[reg] = [value, 0];
            } else {
                cpu.fpu.set_mmx(reg as u8, cpu.xmm[src][0]);
            }
        }
        (0x2b, None | P66) | (0xe7, P66) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let addr = memory(rm, eip)?;
            write_xmm(cpu, bus, Rm32::Mem(addr), cpu.xmm[reg], 16, true, eip)?;
        }
        (0xe7, None) => {
            mmx_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let addr = memory(rm, eip)?;
            write_mmx_mem_qword(cpu, bus, addr, cpu.fpu.mmx(reg as u8), eip)?;
            cpu.fpu.prepare_mmx();
        }
        (0xc3, None) => {
            // MOVNTI is an ordinary 32-bit integer store, even in 16-bit code.
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let addr = memory(rm, eip)?;
            write_mem_u32_at(cpu, bus, addr, cpu.reg(Reg32::from_u3(reg as u8)), eip)?;
        }
        (0x50, None | P66) => {
            sse_available(cpu, eip)?;
            let value = cpu.xmm[register(modrm, eip)?];
            let mask = if form == P66 {
                (value[0] >> 63) | ((value[1] >> 63) << 1)
            } else {
                ((value[0] >> 31) & 1)
                    | ((value[0] >> 62) & 2)
                    | ((value[1] >> 29) & 4)
                    | ((value[1] >> 60) & 8)
            };
            cpu.set_reg(Reg32::from_u3(reg as u8), mask as u32);
        }
        (0x70, _) => {
            let xmm = form != None;
            packed_available(cpu, xmm, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let imm = fetch_u8(cpu, bus)?;
            if xmm {
                let src = read_xmm(cpu, bus, rm, 16, true, eip)?;
                cpu.xmm[reg] = match form {
                    P66 => {
                        let mut value = [0; 2];
                        for lane in 0..4 {
                            let index = (imm >> (lane * 2)) & 3;
                            let dword = (src[(index / 2) as usize] >> ((index & 1) * 32)) as u32;
                            value[lane / 2] |= u64::from(dword) << ((lane & 1) * 32);
                        }
                        value
                    }
                    F2 => [shuffle_words(src[0], imm), src[1]],
                    F3 => [src[0], shuffle_words(src[1], imm)],
                    None => unreachable!(),
                };
            } else {
                let src = read_mmx_rm_qword(cpu, bus, rm, eip)?;
                cpu.fpu.set_mmx(reg as u8, shuffle_words(src, imm));
            }
        }
        (0x71..=0x73, P66) => {
            sse_available(cpu, eip)?;
            let dst = register(modrm, eip)?;
            let imm = fetch_u8(cpu, bus)?;
            let value = cpu.xmm[dst];
            cpu.xmm[dst] = if opcode == 0x73 && matches!(reg, 3 | 7) {
                if imm >= 16 {
                    [0; 2]
                } else {
                    let value = u128::from(value[0]) | (u128::from(value[1]) << 64);
                    let shifted = if reg == 3 {
                        value >> (imm * 8)
                    } else {
                        value << (imm * 8)
                    };
                    [shifted as u64, (shifted >> 64) as u64]
                }
            } else {
                let op = match (opcode, reg) {
                    (0x71..=0x73, 2) => opcode + 0x60,
                    (0x71..=0x72, 4) => opcode + 0x70,
                    (0x71..=0x73, 6) => opcode + 0x80,
                    _ => return Err(StepError::InvalidOpcode { eip }),
                };
                [
                    mmx_binary_result(op, value[0], u64::from(imm)),
                    mmx_binary_result(op, value[1], u64::from(imm)),
                ]
            };
        }
        (0xc4..=0xc5, None | P66) => {
            let xmm = form == P66;
            packed_available(cpu, xmm, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let imm = fetch_u8(cpu, bus)?;
            let lane = usize::from(imm & if xmm { 7 } else { 3 });
            if opcode == 0xc4 {
                let word = u64::from(read_rm16(cpu, bus, rm)?);
                let shift = (lane & 3) * 16;
                if xmm {
                    let half = &mut cpu.xmm[reg][lane / 4];
                    *half = (*half & !(0xffff << shift)) | (word << shift);
                } else {
                    let value = (cpu.fpu.mmx(reg as u8) & !(0xffff << shift)) | (word << shift);
                    cpu.fpu.set_mmx(reg as u8, value);
                }
            } else {
                let src = register(modrm, eip)?;
                let value = if xmm {
                    cpu.xmm[src][lane / 4]
                } else {
                    cpu.fpu.mmx(src as u8)
                };
                if !xmm {
                    cpu.fpu.prepare_mmx();
                }
                cpu.set_reg(
                    Reg32::from_u3(reg as u8),
                    ((value >> ((lane & 3) * 16)) & 0xffff) as u32,
                );
            }
        }
        (0xc6, None | P66) => {
            sse_available(cpu, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            let imm = fetch_u8(cpu, bus)?;
            let src = read_xmm(cpu, bus, rm, 16, true, eip)?;
            let dst = cpu.xmm[reg];
            cpu.xmm[reg] = if form == P66 {
                [dst[usize::from(imm & 1)], src[usize::from((imm >> 1) & 1)]]
            } else {
                let select = |v: [u64; 2], index: u8| {
                    (v[usize::from((index & 3) / 2)] >> ((index & 1) * 32)) as u32
                };
                [
                    u64::from(select(dst, imm)) | (u64::from(select(dst, imm >> 2)) << 32),
                    u64::from(select(src, imm >> 4)) | (u64::from(select(src, imm >> 6)) << 32),
                ]
            };
        }
        (0xd7, None | P66) => {
            let xmm = form == P66;
            packed_available(cpu, xmm, eip)?;
            let src = register(modrm, eip)?;
            let value = if xmm {
                cpu.xmm[src]
            } else {
                [cpu.fpu.mmx(src as u8), 0]
            };
            let mut mask = 0;
            for lane in 0..if xmm { 16 } else { 8 } {
                mask |= (((value[lane / 8] >> ((lane & 7) * 8 + 7)) & 1) as u32) << lane;
            }
            if !xmm {
                cpu.fpu.prepare_mmx();
            }
            cpu.set_reg(Reg32::from_u3(reg as u8), mask);
        }
        (0xf7, None | P66) => {
            let xmm = form == P66;
            packed_available(cpu, xmm, eip)?;
            let mask = register(modrm, eip)?;
            let (data, mask) = if xmm {
                (cpu.xmm[reg], cpu.xmm[mask])
            } else {
                ([cpu.fpu.mmx(reg as u8), 0], [cpu.fpu.mmx(mask as u8), 0])
            };
            mask_store(
                cpu,
                bus,
                data,
                mask,
                if xmm { 16 } else { 8 },
                address16,
                eip,
            )?;
            if !xmm {
                cpu.fpu.prepare_mmx();
            }
        }
        (
            0x60..=0x6d
            | 0x74..=0x76
            | 0xd1..=0xd5
            | 0xd8..=0xe5
            | 0xe8..=0xef
            | 0xf1..=0xf6
            | 0xf8..=0xfe,
            P66,
        )
        | (0xd4 | 0xda | 0xde | 0xe0 | 0xe3..=0xe4 | 0xea | 0xee | 0xf4 | 0xf6 | 0xfb, None) => {
            let xmm = form == P66;
            packed_available(cpu, xmm, eip)?;
            let rm = decode_rm32(cpu, bus, modrm, address16)?;
            if xmm {
                let src = read_xmm(cpu, bus, rm, 16, true, eip)?;
                cpu.xmm[reg] = integer_result(opcode, cpu.xmm[reg], src);
            } else {
                let src = read_mmx_rm_qword(cpu, bus, rm, eip)?;
                let value = integer_half(opcode, cpu.fpu.mmx(reg as u8), src);
                cpu.fpu.set_mmx(reg as u8, value);
            }
        }
        // SSE3 duplicates and SSE4a scalar streaming stores remain absent.
        (0x12, F2 | F3) | (0x16, F3) | (0x2b, F2 | F3) => {
            return Err(StepError::UndefinedOpcode { eip });
        }
        _ => return Err(StepError::InvalidOpcode { eip }),
    }
    Ok(())
}

fn packed_available(cpu: &Cpu, xmm: bool, eip: u32) -> Result<(), StepError> {
    if xmm {
        sse_available(cpu, eip)
    } else {
        mmx_available(cpu, eip)
    }
}

fn memory(rm: Rm32, eip: u32) -> Result<MemAddr, StepError> {
    match rm {
        Rm32::Mem(addr) => Ok(addr),
        Rm32::Reg(_) => Err(StepError::InvalidOpcode { eip }),
    }
}

fn register(modrm: u8, eip: u32) -> Result<usize, StepError> {
    if modrm >> 6 != 3 {
        return Err(StepError::InvalidOpcode { eip });
    }
    Ok(usize::from(modrm & 7))
}

fn merge_scalar(dst: [u64; 2], src: [u64; 2], width: u8) -> [u64; 2] {
    match width {
        4 => [
            (dst[0] & 0xffff_ffff_0000_0000) | (src[0] & 0xffff_ffff),
            dst[1],
        ],
        8 => [src[0], dst[1]],
        _ => src,
    }
}

fn read_xmm<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    rm: Rm32,
    width: u8,
    aligned: bool,
    eip: u32,
) -> Result<[u64; 2], StepError> {
    let addr = match rm {
        Rm32::Reg(reg) => return Ok(cpu.xmm[reg as usize]),
        Rm32::Mem(addr) => addr,
    };
    if aligned {
        aligned_fxsave_memory(rm, eip)?;
    }
    let linear = mem_linear_checked(cpu, addr, u32::from(width), false, eip)?;
    let mut value = [0; 2];
    for i in 0..usize::from(width / 4) {
        let word = read_linear_u32_data(cpu, bus, LinearAddr(linear.0.wrapping_add(i as u32 * 4)))?;
        value[i / 2] |= u64::from(word) << ((i & 1) * 32);
    }
    Ok(value)
}

fn write_xmm<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    rm: Rm32,
    value: [u64; 2],
    width: u8,
    aligned: bool,
    eip: u32,
) -> Result<(), StepError> {
    let addr = match rm {
        Rm32::Reg(reg) => {
            cpu.xmm[reg as usize] = merge_scalar(cpu.xmm[reg as usize], value, width);
            return Ok(());
        }
        Rm32::Mem(addr) => addr,
    };
    if aligned {
        aligned_fxsave_memory(rm, eip)?;
    }
    let linear = mem_linear_checked(cpu, addr, u32::from(width), true, eip)?;
    let mut physical = [[PhysAddr(0); 4]; 4];
    for i in 0..usize::from(width / 4) {
        physical[i] =
            probe_linear_write_bytes(cpu, bus, LinearAddr(linear.0.wrapping_add(i as u32 * 4)), 4)?;
    }
    for i in 0..usize::from(width / 4) {
        write_probed_le(
            cpu,
            bus,
            physical[i],
            4,
            (value[i / 2] >> ((i & 1) * 32)) as u32,
        )?;
    }
    Ok(())
}

fn mask_store<B: Bus>(
    cpu: &mut Cpu,
    bus: &mut B,
    data: [u64; 2],
    mask: [u64; 2],
    width: usize,
    address16: bool,
    eip: u32,
) -> Result<(), StepError> {
    let base = cpu.reg(Reg32::Edi);
    let seg = cpu.segment_override.unwrap_or(SegmentReg::Ds);
    let cpl = cpu.cpl();
    let mut physical = [PhysAddr(0); 16];
    for i in 0..width {
        if mask[i / 8] & (0x80 << ((i & 7) * 8)) != 0 {
            let offset = base.wrapping_add(i as u32) & if address16 { 0xffff } else { u32::MAX };
            let linear = mem_linear_checked(cpu, MemAddr::new(seg, offset), 1, true, eip)?;
            physical[i] = mmu::probe_linear_write_u8_at_cpl(cpu, bus, linear, cpl)?;
        }
    }
    // Only selected bytes are accessed; all-zero masks are allowed to avoid
    // memory faults. Probe every selected byte before committing any write.
    for i in 0..width {
        if mask[i / 8] & (0x80 << ((i & 7) * 8)) != 0 {
            mmu::write_probed_phys_u8(cpu, bus, physical[i], (data[i / 8] >> ((i & 7) * 8)) as u8)?;
        }
    }
    Ok(())
}

fn shuffle_words(value: u64, imm: u8) -> u64 {
    let mut result = 0;
    for lane in 0..4 {
        result |= ((value >> (((imm >> (lane * 2)) & 3) * 16)) & 0xffff) << (lane * 16);
    }
    result
}

fn integer_result(opcode: u8, lhs: [u64; 2], rhs: [u64; 2]) -> [u64; 2] {
    match opcode {
        0x60..=0x62 => [
            integer_half(opcode, lhs[0], rhs[0]),
            integer_half(opcode + 8, lhs[0], rhs[0]),
        ],
        0x68..=0x6a => [
            integer_half(opcode - 8, lhs[1], rhs[1]),
            integer_half(opcode, lhs[1], rhs[1]),
        ],
        0x63 | 0x67 | 0x6b => [
            integer_half(opcode, lhs[0], lhs[1]),
            integer_half(opcode, rhs[0], rhs[1]),
        ],
        0x6c => [lhs[0], rhs[0]],
        0x6d => [lhs[1], rhs[1]],
        0xd1..=0xd3 | 0xe1..=0xe2 | 0xf1..=0xf3 => [
            integer_half(opcode, lhs[0], rhs[0]),
            integer_half(opcode, lhs[1], rhs[0]),
        ],
        _ => [
            integer_half(opcode, lhs[0], rhs[0]),
            integer_half(opcode, lhs[1], rhs[1]),
        ],
    }
}

/// SSE's MMX additions, shared with both halves of the XMM forms.
fn integer_half(opcode: u8, lhs: u64, rhs: u64) -> u64 {
    match opcode {
        0xd4 => lhs.wrapping_add(rhs),
        0xfb => lhs.wrapping_sub(rhs),
        0xf4 => u64::from(lhs as u32) * u64::from(rhs as u32),
        0xda | 0xde | 0xe0 | 0xf6 => {
            let mut value = 0;
            for lane in 0..8 {
                let a = ((lhs >> (lane * 8)) & 255) as u16;
                let b = ((rhs >> (lane * 8)) & 255) as u16;
                if opcode == 0xf6 {
                    value += u64::from(a.abs_diff(b));
                } else {
                    let byte = match opcode {
                        0xda => a.min(b),
                        0xde => a.max(b),
                        _ => (a + b + 1) >> 1,
                    };
                    value |= u64::from(byte) << (lane * 8);
                }
            }
            value
        }
        0xe3..=0xe4 | 0xea | 0xee => {
            let mut value = 0;
            for lane in 0..4 {
                let a = ((lhs >> (lane * 16)) & 0xffff) as u16;
                let b = ((rhs >> (lane * 16)) & 0xffff) as u16;
                let word = match opcode {
                    0xe3 => ((u32::from(a) + u32::from(b) + 1) >> 1) as u16,
                    0xe4 => ((u32::from(a) * u32::from(b)) >> 16) as u16,
                    0xea => (a as i16).min(b as i16) as u16,
                    _ => (a as i16).max(b as i16) as u16,
                };
                value |= u64::from(word) << (lane * 16);
            }
            value
        }
        _ => mmx_binary_result(opcode, lhs, rhs),
    }
}
