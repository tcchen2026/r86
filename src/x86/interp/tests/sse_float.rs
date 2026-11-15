use super::*;

fn run_float(cpu: &mut Cpu, ram: &mut Ram, code: &[u8], a: [u64; 2], b: [u64; 2], mxcsr: u32) {
    ram.data[..code.len()].copy_from_slice(code);
    cpu.eip = 0;
    cpu.cr4 |= CR4_OSFXSR;
    cpu.xmm[0] = a;
    cpu.xmm[1] = b;
    cpu.fpu.mm[0] = a[0];
    cpu.fpu.mm[1] = b[0];
    cpu.fpu.mxcsr = mxcsr;
    cpu.set_reg(Reg32::Ecx, b[0] as u32);
    cpu.replace_eflags(0xad7);
    step_running(cpu, ram);
}

fn instruction(prefix: u8, opcode: u8, imm: u8) -> Vec<u8> {
    let mut code = Vec::new();
    if prefix != 0 {
        code.push(prefix);
    }
    code.extend_from_slice(&[0x0f, opcode, 0xc1]);
    if opcode == 0xc2 {
        code.push(imm);
    }
    code
}

#[cfg(target_arch = "x86_64")]
struct Native {
    xmm: [u64; 2],
    mm: u64,
    gpr: u32,
    flags: u64,
    mxcsr: u32,
}

#[cfg(target_arch = "x86_64")]
fn native<const PREFIX: u8, const OP: u8, const IMM: u8>(
    a: [u64; 2],
    b: [u64; 2],
    controls: u32,
) -> Native {
    let mut xmm = [0u64; 2];
    let mut mm = 0u64;
    let mut mxcsr = controls | 0x1f80; // Always masked, DAZ off.
    let mut saved = 0u32;
    let gpr;
    let flags;
    // Fixed register-form instruction bytes. Every register the instruction or
    // wrapper can change is declared. Restore MXCSR and leave MMX before Rust.
    unsafe {
        core::arch::asm!(
            "stmxcsr [{saved}]", "ldmxcsr [{csr}]",
            "movdqu xmm0, [{a}]", "movdqu xmm1, [{b}]",
            ".if {mmx}", "movq mm0, [{a}]", "movq mm1, [{b}]", ".endif",
            ".if {prefix}", ".byte {prefix}", ".endif",
            ".byte 0x0f,{opcode},0xc1",
            ".if {immediate}", ".byte {imm}", ".endif",
            "pushfq", "pop {flags}",
            "movdqu [{out}], xmm0", "stmxcsr [{csr}]",
            ".if {mmx}", "movq [{mmout}], mm0", "emms", ".endif",
            "ldmxcsr [{saved}]",
            a=in(reg) a.as_ptr(), b=in(reg) b.as_ptr(), out=in(reg) xmm.as_mut_ptr(),
            mmout=in(reg) &mut mm, csr=in(reg) &mut mxcsr, saved=in(reg) &mut saved,
            in("ecx") b[0] as u32, out("eax") gpr, flags=out(reg) flags,
            out("xmm0") _, out("xmm1") _, out("mm0") _, out("mm1") _,
            prefix=const PREFIX, opcode=const OP, imm=const IMM,
            immediate=const (OP == 0xc2) as u8,
            mmx=const (matches!(OP,0x2a|0x2c|0x2d) && matches!(PREFIX,0|0x66)) as u8,
        );
    }
    Native {
        xmm,
        mm,
        gpr,
        flags,
        mxcsr,
    }
}

#[cfg(target_arch = "x86_64")]
fn differential<const PREFIX: u8, const OP: u8, const IMM: u8>(
    cpu: &mut Cpu,
    ram: &mut Ram,
    a: [u64; 2],
    b: [u64; 2],
    mxcsr: u32,
) {
    let conversion = matches!(OP, 0x2a | 0x2c | 0x2d | 0x5a | 0x5b | 0xe6);
    let controls = (mxcsr & 0x8000) | if conversion { mxcsr & 0x6000 } else { 0 };
    let expected = native::<PREFIX, OP, IMM>(a, b, controls);
    run_float(cpu, ram, &instruction(PREFIX, OP, IMM), a, b, mxcsr);
    assert_eq!(
        cpu.xmm[0], expected.xmm,
        "xmm p={PREFIX:02x} op={OP:02x} a={a:016x?} b={b:016x?} csr={mxcsr:x}"
    );
    assert_eq!(
        cpu.fpu.mxcsr & 0x1f,
        expected.mxcsr & 0x1f,
        "mxcsr p={PREFIX:02x} op={OP:02x} a={a:016x?} b={b:016x?} csr={mxcsr:x}"
    );
    if matches!(OP, 0x2c | 0x2d) {
        if matches!(PREFIX, 0 | 0x66) {
            assert_eq!(cpu.fpu.mmx(0), expected.mm);
        } else {
            assert_eq!(cpu.reg(Reg32::Eax), expected.gpr);
        }
    }
    if matches!(OP, 0x2e | 0x2f) {
        assert_eq!(cpu.eflags & 0x8d5, expected.flags as u32 & 0x8d5);
    }
}

#[cfg(target_arch = "x86_64")]
fn single_values() -> Vec<[u64; 2]> {
    let bits = [
        0u32,
        0x8000_0000,
        0x3f80_0000,
        0xbf80_0000,
        1,
        0x8000_0001,
        0x007f_ffff,
        0x0080_0000,
        0x7f7f_ffff,
        0x7f80_0000,
        0xff80_0000,
        0x7fc1_2345,
        0xffc5_4321,
        0x7f81_2345,
        0xff85_4321,
        0x3f00_0000,
        0x3fc0_0000,
        0x4020_0000,
        0x4f00_0000,
        0xcf00_0000,
        0x4eff_ffff,
        0xcf00_0001,
    ];
    let mut values: Vec<_> = bits
        .iter()
        .map(|v| [u64::from(*v) | (u64::from(*v) << 32); 2])
        .collect();
    let mut seed = 0x324d_7bc1_9f80_7342u64;
    for _ in 0..48 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let a = seed;
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        values.push([a, seed]);
    }
    values
}
#[cfg(target_arch = "x86_64")]
fn double_values() -> Vec<[u64; 2]> {
    let bits = [
        0,
        0x8000_0000_0000_0000,
        0x3ff0_0000_0000_0000,
        0xbff0_0000_0000_0000,
        1,
        0x8000_0000_0000_0001,
        0x000f_ffff_ffff_ffff,
        0x0010_0000_0000_0000,
        0x7fef_ffff_ffff_ffff,
        0x7ff0_0000_0000_0000,
        0xfff0_0000_0000_0000,
        0x7ff8_1234_5678_9abc,
        0xfff8_abcd_9876_5432,
        0x7ff0_1234_5678_9abc,
        0xfff0_abcd_9876_5432,
        0x3fe0_0000_0000_0000,
        0x3ff8_0000_0000_0000,
        0x4004_0000_0000_0000,
        0x41e0_0000_0000_0000,
        0xc1e0_0000_0000_0000,
        0x41df_ffff_ffc0_0000,
        0xc1e0_0000_0020_0000,
    ];
    let mut values: Vec<_> = bits.iter().map(|v| [*v; 2]).collect();
    let mut seed = 0x9123_9acb_3d27_0961u64;
    for _ in 0..48 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let a = seed;
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        values.push([a, seed]);
    }
    values
}

#[cfg(target_arch = "x86_64")]
#[test]
fn sse_float_arithmetic_sqrt_minmax_match_masked_host() {
    let single = single_values();
    let double = double_values();
    let mut cpu = flat_cpu();
    let mut ram = Ram::new();
    macro_rules! run {
        ($prefix:expr,$values:expr,$($op:expr),*)=>{$(for controls in [0,0x8000,0x6000] {
            for (i,a) in $values.iter().copied().enumerate() {
                // Pair every special input with every other special class;
                // random vectors get one unrelated random partner.
                let range = if i<22 {0..22} else {i..i+1};
                for j in range {
                    let b=$values[if i<22 {j} else {(j*11+3)%$values.len()}];
                    differential::<$prefix,$op,0>(&mut cpu,&mut ram,a,b,controls);
                }
            }
        })*};
    }
    run!(0, &single, 0x58, 0x59, 0x5c, 0x5e, 0x51, 0x5d, 0x5f);
    run!(0xf3, &single, 0x58, 0x59, 0x5c, 0x5e, 0x51, 0x5d, 0x5f);
    run!(0x66, &double, 0x58, 0x59, 0x5c, 0x5e, 0x51, 0x5d, 0x5f);
    run!(0xf2, &double, 0x58, 0x59, 0x5c, 0x5e, 0x51, 0x5d, 0x5f);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn sse_float_conversions_honor_all_rc_modes_and_match_host() {
    let single = single_values();
    let double = double_values();
    let mut cpu = flat_cpu();
    let mut ram = Ram::new();
    macro_rules! run {
        ($prefix:expr,$op:expr,$values:expr) => {
            for rc in 0..4 {
                for ftz in [0, 0x8000] {
                    for b in $values.iter().copied() {
                        differential::<$prefix, $op, 0>(
                            &mut cpu,
                            &mut ram,
                            [0x1234_5678_9abc_def0, 0xfedc_ba98_7654_3210],
                            b,
                            (rc << 13) | ftz,
                        );
                    }
                }
            }
        };
    }
    run!(0, 0x2c, &single);
    run!(0, 0x2d, &single);
    run!(0x66, 0x2c, &double);
    run!(0x66, 0x2d, &double);
    run!(0xf3, 0x2c, &single);
    run!(0xf3, 0x2d, &single);
    run!(0xf2, 0x2c, &double);
    run!(0xf2, 0x2d, &double);
    run!(0, 0x5a, &single);
    run!(0xf3, 0x5a, &single);
    run!(0x66, 0x5a, &double);
    run!(0xf2, 0x5a, &double);
    run!(0x66, 0x5b, &single);
    run!(0xf3, 0x5b, &single);
    run!(0x66, 0xe6, &double);
    run!(0xf2, 0xe6, &double);
    let integers = vec![
        [0; 2],
        [u64::MAX; 2],
        [0x8000_0000_7fff_ffff, 0x0100_0001_feff_ffff],
        [0x8000_0001_7fff_ff80, 0x1234_5678_ffff_ff7f],
    ];
    run!(0, 0x2a, &integers);
    run!(0x66, 0x2a, &integers);
    run!(0xf3, 0x2a, &integers);
    run!(0xf2, 0x2a, &integers);
    run!(0, 0x5b, &integers);
    run!(0xf3, 0xe6, &integers);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn sse_float_predicates_comis_and_logic_match_host() {
    let single = single_values();
    let double = double_values();
    let mut cpu = flat_cpu();
    let mut ram = Ram::new();
    macro_rules! compares {
        ($p:expr,$values:expr) => {
            for a in $values[..22].iter().copied() {
                for b in $values[..22].iter().copied() {
                    differential::<$p, 0xc2, 0>(&mut cpu, &mut ram, a, b, 0);
                    differential::<$p, 0xc2, 1>(&mut cpu, &mut ram, a, b, 0);
                    differential::<$p, 0xc2, 2>(&mut cpu, &mut ram, a, b, 0);
                    differential::<$p, 0xc2, 3>(&mut cpu, &mut ram, a, b, 0);
                    differential::<$p, 0xc2, 4>(&mut cpu, &mut ram, a, b, 0);
                    differential::<$p, 0xc2, 5>(&mut cpu, &mut ram, a, b, 0);
                    differential::<$p, 0xc2, 6>(&mut cpu, &mut ram, a, b, 0);
                    differential::<$p, 0xc2, 255>(&mut cpu, &mut ram, a, b, 0);
                }
            }
        };
    }
    compares!(0, &single);
    compares!(0xf3, &single);
    compares!(0x66, &double);
    compares!(0xf2, &double);
    for a in single[..22].iter().copied() {
        for b in single[..22].iter().copied() {
            differential::<0, 0x2e, 0>(&mut cpu, &mut ram, a, b, 0);
            differential::<0, 0x2f, 0>(&mut cpu, &mut ram, a, b, 0);
            differential::<0, 0x54, 0>(&mut cpu, &mut ram, a, b, 0);
            differential::<0, 0x55, 0>(&mut cpu, &mut ram, a, b, 0);
            differential::<0, 0x56, 0>(&mut cpu, &mut ram, a, b, 0);
            differential::<0, 0x57, 0>(&mut cpu, &mut ram, a, b, 0);
        }
    }
    for a in double[..22].iter().copied() {
        for b in double[..22].iter().copied() {
            differential::<0x66, 0x2e, 0>(&mut cpu, &mut ram, a, b, 0);
            differential::<0x66, 0x2f, 0>(&mut cpu, &mut ram, a, b, 0);
            differential::<0x66, 0x54, 0>(&mut cpu, &mut ram, a, b, 0);
            differential::<0x66, 0x55, 0>(&mut cpu, &mut ram, a, b, 0);
            differential::<0x66, 0x56, 0>(&mut cpu, &mut ram, a, b, 0);
            differential::<0x66, 0x57, 0>(&mut cpu, &mut ram, a, b, 0);
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[test]
fn sse_float_packed_lanes_accumulate_independent_flags() {
    let mut cpu = flat_cpu();
    let mut ram = Ram::new();
    // DIVPS: SNaN, 1/0, maximum/minimum normal, minimum subnormal/2.
    // All five supported flags must survive together with every mask clear.
    let a = [0x3f80_0000_7f81_2345, 0x0000_0001_7f7f_ffff];
    let b = [0, 0x4000_0000_0080_0000];
    differential::<0, 0x5e, 0>(&mut cpu, &mut ram, a, b, 0);
    assert_eq!(cpu.fpu.mxcsr, 0x1f);
    differential::<0, 0x58, 0>(&mut cpu, &mut ram, a, b, 0);
    differential::<0, 0x59, 0>(&mut cpu, &mut ram, a, b, 0);
    differential::<0, 0x5c, 0>(&mut cpu, &mut ram, a, b, 0);
    differential::<0, 0x51, 0>(&mut cpu, &mut ram, b, a, 0);
    differential::<0, 0xc2, 0>(&mut cpu, &mut ram, a, b, 0);
    differential::<0, 0x5d, 0>(&mut cpu, &mut ram, a, b, 0);
    differential::<0, 0x5f, 0>(&mut cpu, &mut ram, a, b, 0);
    let a = [0x7ff0_1234_5678_9abc, 1];
    let b = [0, 0x4000_0000_0000_0000];
    differential::<0x66, 0x5e, 0>(&mut cpu, &mut ram, a, b, 0);
    differential::<0x66, 0xc2, 0>(&mut cpu, &mut ram, a, b, 0);
    differential::<0x66, 0x59, 0>(
        &mut cpu,
        &mut ram,
        [0x7fef_ffff_ffff_ffff, 0x0010_0000_0000_0000],
        [0x4000_0000_0000_0000, 0x3fe0_0000_0000_0000],
        0x8000,
    );
}

#[test]
fn sse_float_tiny_results_ftz_and_suppressed_exceptions() {
    let mut cpu = flat_cpu();
    let mut ram = Ram::new();
    for masks in [0, 0x1f80] {
        for (opcode, a, b, result, flags) in [
            (
                0x59,
                0x0010_0000_0000_0000,
                0x3fe0_0000_0000_0000,
                0x0008_0000_0000_0000,
                0,
            ),
            (0x59, 1, 0x3fe0_0000_0000_0000, 0, 0x12),
            (0x59, 3, 0x3fe0_0000_0000_0000, 2, 0x12),
            (0x5e, 3, 0x4000_0000_0000_0000, 2, 0x12),
            (0x5e, 0x3ff0_0000_0000_0000, 0, 0x7ff0_0000_0000_0000, 4),
            (0x5e, 0, 0, 0xfff8_0000_0000_0000, 1),
            (
                0x59,
                0x7fef_ffff_ffff_ffff,
                0x4000_0000_0000_0000,
                0x7ff0_0000_0000_0000,
                8,
            ),
        ] {
            run_float(
                &mut cpu,
                &mut ram,
                &instruction(0xf2, opcode, 0),
                [a, 0x1234],
                [b, 0],
                masks,
            );
            assert_eq!(cpu.xmm[0], [result, 0x1234]);
            assert_eq!(cpu.fpu.mxcsr, masks | flags);
        }
    }
    run_float(
        &mut cpu,
        &mut ram,
        &instruction(0xf2, 0x59, 0),
        [0x0010_0000_0000_0000, 0],
        [0x3fe0_0000_0000_0000, 0],
        0x8000,
    );
    assert_eq!(cpu.xmm[0][0], 0);
    assert_eq!(cpu.fpu.mxcsr, 0x8010);
    // Explicit debt: a tiny exact result rounding to minimum normal omits UE.
    run_float(
        &mut cpu,
        &mut ram,
        &instruction(0xf2, 0x59, 0),
        [0x0010_0000_0000_0000, 0],
        [0x3fef_ffff_ffff_ffff, 0],
        0,
    );
    assert_eq!(cpu.xmm[0][0], 0x0010_0000_0000_0000);
    assert_eq!(cpu.fpu.mxcsr, 0);
    // Previously loaded PE remains sticky; ordinary inexact arithmetic cannot trap.
    run_float(
        &mut cpu,
        &mut ram,
        &instruction(0xf2, 0x5e, 0),
        [0x3ff0_0000_0000_0000, 0],
        [0x4008_0000_0000_0000, 0],
        0x20,
    );
    assert_eq!(cpu.fpu.mxcsr, 0x20);
}

#[test]
fn sse_float_reciprocal_special_cases_and_error_bound() {
    let mut cpu = flat_cpu();
    let mut ram = Ram::new();
    for (opcode, input, expected) in [
        (0x53, 0, 0x7f80_0000),
        (0x53, 0x8000_0000, 0xff80_0000),
        (0x53, 1, 0x7f80_0000),
        (0x53, 0x8000_0001, 0xff80_0000),
        (0x53, 0x7f80_0000, 0),
        (0x53, 0xff80_0000, 0x8000_0000),
        (0x53, 0x7f81_2345, 0x7fc1_2345),
        (0x52, 0xbf80_0000, 0xffc0_0000),
        (0x52, 0x8000_0001, 0xff80_0000),
        (0x52, 0xff80_0000, 0xffc0_0000),
        (0x52, 0x7f80_0000, 0),
    ] {
        run_float(
            &mut cpu,
            &mut ram,
            &instruction(0xf3, opcode, 0),
            [u64::MAX; 2],
            [input, 0],
            0,
        );
        assert_eq!(cpu.xmm[0], [0xffff_ffff_0000_0000 | expected, u64::MAX]);
        assert_eq!(cpu.fpu.mxcsr, 0);
    }
    for exponent in 1..255u32 {
        for fraction in [0, 1, 0x3fffff, 0x7fffff] {
            let bits = (exponent << 23) | fraction;
            let x = f64::from(f32::from_bits(bits));
            for opcode in [0x52, 0x53] {
                run_float(
                    &mut cpu,
                    &mut ram,
                    &instruction(0xf3, opcode, 0),
                    [0; 2],
                    [u64::from(bits), 0],
                    0,
                );
                let value = f64::from(f32::from_bits(cpu.xmm[0][0] as u32));
                let expected = if opcode == 0x52 {
                    1.0 / x.sqrt()
                } else {
                    1.0 / x
                };
                if expected < f64::from(f32::MIN_POSITIVE) {
                    assert_eq!(value, 0.0);
                } else {
                    assert!(((value - expected) / expected).abs() <= 1.5 / 4096.0);
                }
                assert_eq!(cpu.fpu.mxcsr, 0);
            }
        }
    }
}

#[test]
fn sse_float_memory_widths_scalar_lanes_and_mmx_transitions() {
    let mut cpu = flat_cpu();
    let mut ram = Ram::new();
    cpu.cr4 |= CR4_OSFXSR;
    for (prefix, opcode, width) in [
        (0xf3, 0x58, 4),
        (0xf2, 0x58, 8),
        (0, 0x5a, 8),
        (0xf3, 0xe6, 8),
        (0, 0x2a, 8),
    ] {
        let address = 0x1000 - width;
        let mut code = Vec::new();
        if prefix != 0 {
            code.push(prefix);
        }
        code.extend_from_slice(&[0x0f, opcode, 0x05]);
        code.extend_from_slice(&(address as u32).to_le_bytes());
        ram.data[..code.len()].copy_from_slice(&code);
        cpu.eip = 0;
        cpu.xmm[0] = [0, 0x1234];
        cpu.fpu.mmx_active = false;
        cpu.fpu.top = 5;
        ram.data[address..0x1000].fill(0);
        enable_test_paging(&mut cpu);
        map_test_page(&mut ram, 0, 0, TEST_PTE_PRESENT | TEST_PTE_WRITE);
        step_running(&mut cpu, &mut ram);
        assert!(!cpu.fpu.mmx_active);
        assert_eq!(cpu.fpu.top, 5);
        if opcode == 0x58 {
            assert_eq!(cpu.xmm[0], [0, 0x1234]);
        }
    }
    cpu.cr0 &= !CR0_PG;
    cpu.invalidate_translation_context();
    for prefix in [0, 0x66] {
        run_float(
            &mut cpu,
            &mut ram,
            &instruction(prefix, 0x2a, 0),
            [0; 2],
            [0x0000_0002_0000_0001, 0],
            0,
        );
        assert!(cpu.fpu.mmx_active);
        assert_eq!(cpu.fpu.top, 0);
    }
}

#[cfg(feature = "jit")]
#[test]
fn sse_float_opcodes_remain_jit_exits() {
    use crate::x86::jit::decode::block;
    for opcode in [
        0x2a, 0x2c, 0x2d, 0x2e, 0x2f, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a,
        0x5b, 0x5c, 0x5d, 0x5e, 0x5f, 0xc2, 0xe6,
    ] {
        for prefix in [0, 0x66, 0xf2, 0xf3] {
            let code = instruction(prefix, opcode, 0);
            assert!(block(&code, 0, 1).instructions.is_empty());
            let mut following = vec![0x90];
            following.extend_from_slice(&code);
            assert_eq!(block(&following, 0, 2).byte_len, 1);
        }
    }
}
