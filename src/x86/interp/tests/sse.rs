// Shares the interpreter tests' guest RAM and paging fixtures.
use super::*;

fn sse_run(cpu: &mut Cpu, ram: &mut Ram, code: &[u8]) {
    ram.data[..code.len()].copy_from_slice(code);
    cpu.eip = 0;
    step_running(cpu, ram);
}

fn xmm_cpu() -> Cpu {
    let mut cpu = flat_cpu();
    cpu.cr4 |= CR4_OSFXSR;
    cpu
}

fn sse_memory_code(prefix: &[u8], opcode: u8, addr: u32) -> Vec<u8> {
    let mut code = prefix.to_vec();
    code.extend_from_slice(&[0x0f, opcode, 0x05]);
    code.extend_from_slice(&addr.to_le_bytes());
    code
}

#[test]
fn sse_scalar_moves_merge_registers_and_clear_memory_load_upper_lanes() {
    let a = [0x1122_3344_5566_7788, 0x99aa_bbcc_ddee_ff00];
    let b = [0xdead_beef_7654_3210, 0x0123_4567_89ab_cdef];
    for (prefix, width) in [(0xf3, 4), (0xf2, 8)] {
        let mut cpu = xmm_cpu();
        let mut ram = Ram::new();
        cpu.xmm[0] = a;
        cpu.xmm[1] = b;
        sse_run(&mut cpu, &mut ram, &[prefix, 0x0f, 0x10, 0xc1]);
        assert_eq!(
            cpu.xmm[0],
            [
                if width == 4 {
                    0x1122_3344_7654_3210
                } else {
                    b[0]
                },
                a[1]
            ]
        );
        cpu.xmm[0] = a;
        sse_run(&mut cpu, &mut ram, &[prefix, 0x0f, 0x11, 0xc1]);
        assert_eq!(
            cpu.xmm[1],
            [
                if width == 4 {
                    0xdead_beef_5566_7788
                } else {
                    a[0]
                },
                b[1]
            ]
        );
        ram.write_u64(0x301, b[0]);
        sse_run(&mut cpu, &mut ram, &sse_memory_code(&[prefix], 0x10, 0x301));
        assert_eq!(
            cpu.xmm[0],
            [if width == 4 { b[0] & 0xffff_ffff } else { b[0] }, 0]
        );
        ram.data[0x301..0x311].fill(0xa5);
        cpu.xmm[0] = a;
        sse_run(&mut cpu, &mut ram, &sse_memory_code(&[prefix], 0x11, 0x301));
        assert_eq!(
            &ram.data[0x301..0x301 + width],
            &a[0].to_le_bytes()[..width]
        );
        assert!(ram.data[0x301 + width..0x311].iter().all(|v| *v == 0xa5));
    }
}

#[test]
fn sse_packed_moves_use_exact_bits_and_distinguish_alignment() {
    let value = [0x7ff0_0000_0000_0001u64, 0x8000_0000_0123_4567];
    for (prefix, load, store, aligned) in [
        (&[][..], 0x10, 0x11, false),
        (&[0x66][..], 0x10, 0x11, false),
        (&[][..], 0x28, 0x29, true),
        (&[0x66][..], 0x28, 0x29, true),
        (&[0x66][..], 0x6f, 0x7f, true),
        (&[0xf3][..], 0x6f, 0x7f, false),
    ] {
        for addr in [0x300, 0x301] {
            let mut ram = Ram::new();
            let mut cpu = xmm_cpu();
            ram.write_u64(addr, value[0]);
            ram.write_u64(addr + 8, value[1]);
            for opcode in [load, store] {
                let code = sse_memory_code(prefix, opcode, addr as u32);
                ram.data[..code.len()].copy_from_slice(&code);
                cpu.eip = 0;
                if aligned && addr & 15 != 0 {
                    step_gp(&mut cpu, &mut ram, 0);
                    assert_eq!(cpu.xmm[0], [0; 2]);
                } else {
                    step_running(&mut cpu, &mut ram);
                    assert_eq!(cpu.xmm[0], value);
                    assert_eq!([ram.read_u64(addr), ram.read_u64(addr + 8)], value);
                }
            }
        }
    }
}

#[test]
fn sse_half_and_integer_moves_preserve_or_zero_the_right_lanes() {
    let a = [0x1122_3344_5566_7788, 0x99aa_bbcc_ddee_ff00];
    let b = [0xdead_beef_7654_3210, 0x0123_4567_89ab_cdef];
    let mut cpu = xmm_cpu();
    let mut ram = Ram::new();
    cpu.xmm[1] = b;
    for (opcode, expected) in [(0x12, [b[1], a[1]]), (0x16, [a[0], b[0]])] {
        cpu.xmm[0] = a;
        sse_run(&mut cpu, &mut ram, &[0x0f, opcode, 0xc1]);
        assert_eq!(cpu.xmm[0], expected);
    }
    for prefix in [&[][..], &[0x66][..]] {
        for (load, store, half) in [(0x12, 0x13, 0), (0x16, 0x17, 1)] {
            cpu.xmm[0] = a;
            ram.write_u64(0x301, b[0]);
            sse_run(&mut cpu, &mut ram, &sse_memory_code(prefix, load, 0x301));
            let mut expected = a;
            expected[half] = b[0];
            assert_eq!(cpu.xmm[0], expected);
            ram.write_u64(0x301, 0);
            sse_run(&mut cpu, &mut ram, &sse_memory_code(prefix, store, 0x301));
            assert_eq!(ram.read_u64(0x301), b[0]);
        }
    }
    cpu.set_reg(Reg32::Ecx, 0x8765_4321);
    sse_run(&mut cpu, &mut ram, &[0x66, 0x0f, 0x6e, 0xc1]);
    assert_eq!(cpu.xmm[0], [0x8765_4321, 0]);
    cpu.xmm[0] = a;
    sse_run(&mut cpu, &mut ram, &[0x66, 0x0f, 0x7e, 0xc1]);
    assert_eq!(cpu.reg(Reg32::Ecx), a[0] as u32);
    sse_run(&mut cpu, &mut ram, &[0xf3, 0x0f, 0x7e, 0xc1]);
    assert_eq!(cpu.xmm[0], [b[0], 0]);
    cpu.xmm[0] = a;
    sse_run(&mut cpu, &mut ram, &[0x66, 0x0f, 0xd6, 0xc1]);
    assert_eq!(cpu.xmm[1], [a[0], 0]);
    cpu.fpu.mm[1] = b[0];
    sse_run(&mut cpu, &mut ram, &[0xf3, 0x0f, 0xd6, 0xc1]);
    assert_eq!(cpu.xmm[0], [b[0], 0]);
    assert!(cpu.fpu.mmx_active);
    cpu.xmm[1] = a;
    sse_run(&mut cpu, &mut ram, &[0xf2, 0x0f, 0xd6, 0xc1]);
    assert_eq!(cpu.fpu.mmx(0), a[0]);
    assert_eq!(cpu.fpu.top, 0);
}

#[test]
fn sse_shuffles_unpacks_insert_extract_and_sign_masks() {
    let a = [0x0004_0003_0002_0001, 0x0008_0007_0006_0005];
    let b = [0x0014_0013_0012_0011, 0x0018_0017_0016_0015];
    for (code, expected) in [
        (
            &[0x66, 0x0f, 0x70, 0xc1, 0x1b][..],
            [0x0016_0015_0018_0017, 0x0012_0011_0014_0013],
        ),
        (
            &[0xf2, 0x0f, 0x70, 0xc1, 0x1b][..],
            [0x0011_0012_0013_0014, b[1]],
        ),
        (
            &[0xf3, 0x0f, 0x70, 0xc1, 0x1b][..],
            [b[0], 0x0015_0016_0017_0018],
        ),
        (
            &[0x0f, 0xc6, 0xc1, 0x1b][..],
            [0x0006_0005_0008_0007, 0x0012_0011_0014_0013],
        ),
        (&[0x66, 0x0f, 0xc6, 0xc1, 0x01][..], [a[1], b[0]]),
        (
            &[0x0f, 0x14, 0xc1][..],
            [0x0012_0011_0002_0001, 0x0014_0013_0004_0003],
        ),
        (
            &[0x0f, 0x15, 0xc1][..],
            [0x0016_0015_0006_0005, 0x0018_0017_0008_0007],
        ),
        (&[0x66, 0x0f, 0x14, 0xc1][..], [a[0], b[0]]),
        (&[0x66, 0x0f, 0x15, 0xc1][..], [a[1], b[1]]),
    ] {
        let mut cpu = xmm_cpu();
        let mut ram = Ram::new();
        cpu.xmm[0] = a;
        cpu.xmm[1] = b;
        sse_run(&mut cpu, &mut ram, code);
        assert_eq!(cpu.xmm[0], expected, "{code:02x?}");
    }
    let mut cpu = xmm_cpu();
    let mut ram = Ram::new();
    cpu.fpu.mm[1] = b[0];
    sse_run(&mut cpu, &mut ram, &[0x0f, 0x70, 0xc1, 0x1b]);
    assert_eq!(cpu.fpu.mmx(0), 0x0011_0012_0013_0014);
    cpu.xmm[0] = a;
    cpu.set_reg(Reg32::Ecx, 0xdead_9988);
    sse_run(&mut cpu, &mut ram, &[0x66, 0x0f, 0xc4, 0xc1, 0xfe]);
    assert_eq!(cpu.xmm[0], [a[0], 0x0008_9988_0006_0005]);
    sse_run(&mut cpu, &mut ram, &[0x66, 0x0f, 0xc5, 0xc0, 0xfe]);
    assert_eq!(cpu.reg(Reg32::Eax), 0x9988);
    cpu.xmm[1] = [0x8000_0000_8000_0000, 0x0080_8000_0000_0000];
    sse_run(&mut cpu, &mut ram, &[0x66, 0x0f, 0xd7, 0xc1]);
    assert_eq!(cpu.reg(Reg32::Eax), 0x6088);
    sse_run(&mut cpu, &mut ram, &[0x0f, 0x50, 0xc1]);
    assert_eq!(cpu.reg(Reg32::Eax), 3);
    sse_run(&mut cpu, &mut ram, &[0x66, 0x0f, 0x50, 0xc1]);
    assert_eq!(cpu.reg(Reg32::Eax), 1);
}

#[test]
fn sse_shift_counts_use_the_full_low_qword_and_cross_qword_bytes() {
    let value = [0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908];
    let mut cpu = xmm_cpu();
    let mut ram = Ram::new();
    for count in [0u8, 1, 7, 8, 9, 15, 16, 17, 255] {
        for group in [3, 7] {
            cpu.xmm[0] = value;
            sse_run(
                &mut cpu,
                &mut ram,
                &[0x66, 0x0f, 0x73, 0xc0 | (group << 3), count],
            );
            let mut expected = [0u8; 16];
            for (i, byte) in expected.iter_mut().enumerate() {
                let src = if group == 3 {
                    i as i32 + i32::from(count)
                } else {
                    i as i32 - i32::from(count)
                };
                if (0..16).contains(&src) {
                    *byte = src as u8;
                }
            }
            assert_eq!(
                cpu.xmm[0],
                [
                    u64::from_le_bytes(expected[..8].try_into().unwrap()),
                    u64::from_le_bytes(expected[8..].try_into().unwrap())
                ]
            );
        }
    }
    for (opcode, group, count, input, expected) in [
        (
            0x71,
            4,
            15,
            [0x8000_7fff_ffff_0001, 0x0001_8000_7fff_ffff],
            [0xffff_0000_ffff_0000, 0x0000_ffff_0000_ffff],
        ),
        (
            0x72,
            4,
            31,
            [0x8000_0000_7fff_ffff, 0xffff_ffff_0000_0001],
            [0xffff_ffff_0000_0000; 2],
        ),
        (0x73, 6, 63, [1, 3], [0x8000_0000_0000_0000; 2]),
    ] {
        cpu.xmm[0] = input;
        sse_run(
            &mut cpu,
            &mut ram,
            &[0x66, 0x0f, opcode, 0xc0 | (group << 3), count],
        );
        assert_eq!(cpu.xmm[0], expected);
    }
    for opcode in [0xd1, 0xd2, 0xd3, 0xf1, 0xf2, 0xf3] {
        cpu.xmm[0] = [u64::MAX; 2];
        cpu.xmm[1] = [0x1_0000_0000, 0];
        sse_run(&mut cpu, &mut ram, &[0x66, 0x0f, opcode, 0xc1]);
        assert_eq!(cpu.xmm[0], [0; 2]);
    }
}

#[test]
fn sse_streaming_and_masked_stores_observe_width_mask_segment_and_address_size() {
    let data = [0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908];
    for (prefix, opcode, width) in [
        (&[][..], 0x2b, 16),
        (&[0x66][..], 0x2b, 16),
        (&[0x66][..], 0xe7, 16),
        (&[][..], 0xe7, 8),
        (&[][..], 0xc3, 4),
    ] {
        let mut cpu = xmm_cpu();
        let mut ram = Ram::new();
        cpu.xmm[0] = data;
        cpu.fpu.mm[0] = data[0];
        cpu.set_reg(Reg32::Eax, data[0] as u32);
        ram.data[0x300..0x320].fill(0xa5);
        sse_run(&mut cpu, &mut ram, &sse_memory_code(prefix, opcode, 0x300));
        assert_eq!(
            &ram.data[0x300..0x300 + width],
            &(0..width as u8).collect::<Vec<_>>()
        );
        assert!(ram.data[0x300 + width..0x320].iter().all(|v| *v == 0xa5));
    }
    for xmm in [false, true] {
        let mut cpu = xmm_cpu();
        let mut ram = Ram::new();
        let mask = [0x8000_8000_8000_8000; 2];
        cpu.xmm[0] = data;
        cpu.xmm[1] = mask;
        cpu.fpu.mm[0] = data[0];
        cpu.fpu.mm[1] = mask[0];
        cpu.fpu.top = 5;
        cpu.set_reg(Reg32::Edi, 0xdead_0300);
        cpu.set_seg_cache(
            SegmentReg::Fs,
            SegmentCache::new(0x10, 0x1000, 0xffff, 0xc093),
        );
        ram.data[0x1300..0x1310].fill(0xa5);
        let mut code = vec![0x64, 0x67];
        if xmm {
            code.push(0x66);
        }
        code.extend_from_slice(&[0x0f, 0xf7, 0xc1]);
        sse_run(&mut cpu, &mut ram, &code);
        for i in 0..16 {
            assert_eq!(
                ram.data[0x1300 + i],
                if i % 2 == 1 && (xmm || i < 8) {
                    i as u8
                } else {
                    0xa5
                }
            );
        }
        assert_eq!(&ram.data[0x300..0x310], &[0; 16]);
        assert_eq!(cpu.fpu.mmx_active, !xmm);
        assert_eq!(cpu.fpu.top, if xmm { 5 } else { 0 });
        cpu.xmm[1] = [0; 2];
        cpu.fpu.mm[1] = 0;
        cpu.set_reg(Reg32::Edi, u32::MAX - 7);
        sse_run(
            &mut cpu,
            &mut ram,
            if xmm {
                &[0x66, 0x0f, 0xf7, 0xc1]
            } else {
                &[0x0f, 0xf7, 0xc1]
            },
        );
    }
}

#[test]
fn sse_split_page_faults_preserve_destinations_and_selected_store_bytes() {
    for store in [false, true] {
        let mut ram = Ram::new();
        let mut cpu = xmm_cpu();
        cpu.xmm[0] = [u64::MAX; 2];
        ram.data[0xff8..0x1000].fill(0xa5);
        enable_test_paging(&mut cpu);
        map_test_page(&mut ram, 0, 0, TEST_PTE_PRESENT | TEST_PTE_WRITE);
        let code = sse_memory_code(&[0xf3], if store { 0x7f } else { 0x6f }, 0xff8);
        ram.data[..code.len()].copy_from_slice(&code);
        let err = Interpreter::step(&mut cpu, &mut ram).unwrap_err();
        assert_eq!(err.page_fault().unwrap().linear, LinearAddr(0x1000));
        assert_eq!(cpu.xmm[0], [u64::MAX; 2]);
        assert_eq!(&ram.data[0xff8..0x1000], &[0xa5; 8]);
    }
    for selected_high in [false, true] {
        let mut ram = Ram::new();
        let mut cpu = xmm_cpu();
        cpu.xmm[0] = [u64::MAX; 2];
        cpu.xmm[1] = [u64::MAX, if selected_high { u64::MAX } else { 0 }];
        cpu.set_reg(Reg32::Edi, 0xff8);
        ram.data[..4].copy_from_slice(&[0x66, 0x0f, 0xf7, 0xc1]);
        ram.data[0xff8..0x1000].fill(0xa5);
        enable_test_paging(&mut cpu);
        map_test_page(&mut ram, 0, 0, TEST_PTE_PRESENT | TEST_PTE_WRITE);
        if selected_high {
            assert_eq!(
                Interpreter::step(&mut cpu, &mut ram)
                    .unwrap_err()
                    .page_fault()
                    .unwrap()
                    .linear,
                LinearAddr(0x1000)
            );
            assert_eq!(&ram.data[0xff8..0x1000], &[0xa5; 8]);
        } else {
            step_running(&mut cpu, &mut ram);
            assert_eq!(&ram.data[0xff8..0x1000], &[0xff; 8]);
        }
    }
}

#[test]
fn sse_mmx_xmm_and_movnti_have_distinct_availability_gates() {
    let mut forms: Vec<(Vec<u8>, bool, bool)> = Vec::new();
    for opcode in [
        0xd4, 0xda, 0xde, 0xe0, 0xe3, 0xe4, 0xea, 0xee, 0xf4, 0xf6, 0xfb, 0xd7, 0xf7,
    ] {
        forms.push((vec![0x0f, opcode, 0xc1], false, true));
        forms.push((vec![0x66, 0x0f, opcode, 0xc1], true, false));
    }
    for opcode in [0x70, 0xc4, 0xc5] {
        forms.push((vec![0x0f, opcode, 0xc1, 1], false, true));
        forms.push((vec![0x66, 0x0f, opcode, 0xc1, 1], true, false));
    }
    forms.push((sse_memory_code(&[], 0xe7, 0x300), false, true));
    forms.push((sse_memory_code(&[], 0xc3, 0x300), false, false));
    forms.push((vec![0xf2, 0x0f, 0xd6, 0xc1], true, true));
    forms.push((vec![0xf3, 0x0f, 0xd6, 0xc1], true, true));
    for (code, xmm, mmx) in forms {
        let movnti = !xmm && !mmx;
        for cr0 in [0, CR0_EM, CR0_TS, CR0_EM | CR0_TS] {
            for cr4 in [0, CR4_OSFXSR] {
                let mut cpu = flat_cpu();
                let mut ram = Ram::new();
                cpu.cr0 |= cr0;
                cpu.cr4 |= cr4;
                cpu.fpu.top = 5;
                cpu.set_reg(Reg32::Edi, 0x300);
                ram.data[..code.len()].copy_from_slice(&code);
                if !movnti && (cr0 & CR0_EM != 0 || xmm && cr4 == 0) {
                    step_ud(&mut cpu, &mut ram, 0);
                    assert!(!cpu.fpu.mmx_active);
                } else if !movnti && cr0 & CR0_TS != 0 {
                    step_nm(&mut cpu, &mut ram, 0);
                    assert!(!cpu.fpu.mmx_active);
                } else {
                    step_running(&mut cpu, &mut ram);
                    assert_eq!(cpu.fpu.mmx_active, mmx, "{code:02x?}");
                    assert_eq!(cpu.fpu.top, if mmx { 0 } else { 5 }, "{code:02x?}");
                }
            }
        }
    }
}

#[test]
fn sse_mandatory_prefixes_are_literal_in_both_code_sizes() {
    for default16 in [false, true] {
        for prefix in [&[0xf3][..], &[0x66, 0xf3][..], &[0xf3, 0x66][..]] {
            let mut cpu = if default16 { Cpu::new() } else { flat_cpu() };
            cpu.cr4 |= CR4_OSFXSR;
            let mut ram = Ram::new();
            cpu.xmm[1] = [0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210];
            let mut code = prefix.to_vec();
            code.extend_from_slice(&[0x0f, 0x6f, 0xc1]);
            sse_run(&mut cpu, &mut ram, &code);
            assert_eq!(cpu.xmm[0], cpu.xmm[1]);
        }
    }
    let mut cpu = xmm_cpu();
    let mut ram = Ram::new();
    for code in [
        &[0xf2, 0x0f, 0x12, 0xc1][..],
        &[0xf3, 0x0f, 0x12, 0xc1],
        &[0xf3, 0x0f, 0x16, 0xc1],
        &[0xf2, 0x0f, 0x2b, 0x00],
        &[0xf3, 0x0f, 0x2b, 0x00],
        &[0x0f, 0xd0, 0xc1],
    ] {
        ram.data[..code.len()].copy_from_slice(code);
        cpu.eip = 0;
        assert_eq!(
            Interpreter::step(&mut cpu, &mut ram),
            Err(StepError::UndefinedOpcode { eip: 0 })
        );
    }
    for code in [
        &[0xf2, 0x0f, 0x6e, 0xc1][..],
        &[0x66, 0x0f, 0x12, 0xc1],
        &[0x0f, 0x2b, 0xc1],
        &[0x0f, 0xc3, 0xc1],
        &[0xf0, 0x66, 0x0f, 0x6f, 0xc1],
    ] {
        ram.data[..code.len()].copy_from_slice(code);
        cpu.eip = 0;
        step_ud(&mut cpu, &mut ram, 0);
    }
}

#[cfg(target_arch = "x86_64")]
fn native_sse_integer<const OP: u8>(a: [u64; 2], b: [u64; 2]) -> [u64; 2] {
    let mut result = [0u64; 2];
    // Only integer SSE2 instructions: no host FP flags or rounding are changed.
    unsafe {
        core::arch::asm!(
            "movdqu xmm0, [{a}]", "movdqu xmm1, [{b}]",
            ".byte 0x66,0x0f,{opcode},0xc1", "movdqu [{out}], xmm0",
            a=in(reg) a.as_ptr(), b=in(reg) b.as_ptr(), out=in(reg) result.as_mut_ptr(),
            opcode=const OP, out("xmm0") _,out("xmm1") _,options(nostack,preserves_flags),
        );
    }
    result
}

#[cfg(target_arch = "x86_64")]
#[test]
fn sse_packed_integer_results_match_host_sse2() {
    let mut values = vec![
        [0; 2],
        [u64::MAX; 2],
        [0x8000_7fff_ff00_0100, 0x8000_0000_7fff_ffff],
        [1, 64],
        [255, 0],
    ];
    let mut seed = 0x2384_9983_0103_bf91u64;
    for _ in 0..48 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let first = seed;
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        values.push([first, seed]);
    }
    let mut ram = Ram::new();
    let mut cpu = xmm_cpu();
    macro_rules! check {
        ($($op:expr),* $(,)?)=>{$({
            for (i,a) in values.iter().copied().enumerate() {
                let b=values[(i*7+3)%values.len()];
                cpu.xmm[0]=a; cpu.xmm[1]=b;
                sse_run(&mut cpu,&mut ram,&[0x66,0x0f,$op,0xc1]);
                assert_eq!(cpu.xmm[0],native_sse_integer::<$op>(a,b),"opcode={:02x}, a={a:016x?}, b={b:016x?}",$op);
            }
        })*};
    }
    check!(
        0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x74,
        0x75, 0x76, 0xd1, 0xd2, 0xd3, 0xd4, 0xd5, 0xd8, 0xd9, 0xda, 0xdb, 0xdc, 0xdd, 0xde, 0xdf,
        0xe0, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe8, 0xe9, 0xea, 0xeb, 0xec, 0xed, 0xee, 0xef, 0xf1,
        0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd, 0xfe
    );
    macro_rules! check_mmx {
        ($($op:expr),*)=>{$({
            for (i,a) in values.iter().copied().enumerate() {
                let b=values[(i*7+3)%values.len()];
                cpu.fpu.mm[0]=a[0];cpu.fpu.mm[1]=b[0];
                sse_run(&mut cpu,&mut ram,&[0x0f,$op,0xc1]);
                assert_eq!(cpu.fpu.mmx(0),native_sse_integer::<$op>([a[0],0],[b[0],0])[0],"MMX opcode={:02x}",$op);
            }
        })*};
    }
    cpu.cr4 &= !CR4_OSFXSR;
    check_mmx!(
        0xd4, 0xda, 0xde, 0xe0, 0xe3, 0xe4, 0xea, 0xee, 0xf4, 0xf6, 0xfb
    );
}

#[cfg(feature = "jit")]
#[test]
fn sse_new_opcodes_are_all_interpreter_exits_in_the_jit_decoder() {
    use crate::x86::jit::decode::block;
    let mut codes = Vec::new();
    for op in [
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x28, 0x29, 0x2b, 0x50, 0x70, 0xc4, 0xc5,
        0xc6, 0xd7, 0xe7, 0xf7, 0xd4, 0xda, 0xde, 0xe0, 0xe3, 0xe4, 0xea, 0xee, 0xf4, 0xf6, 0xfb,
    ] {
        codes.push(vec![0x0f, op, 0xc1, 0x1b]);
        codes.push(vec![0x66, 0x0f, op, 0xc1, 0x1b]);
    }
    for op in [
        0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e,
        0x6f, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x7e, 0x7f, 0xd1, 0xd2, 0xd3, 0xd5, 0xd6, 0xd8,
        0xd9, 0xdb, 0xdc, 0xdd, 0xdf, 0xe1, 0xe2, 0xe5, 0xe8, 0xe9, 0xeb, 0xec, 0xed, 0xef, 0xf1,
        0xf2, 0xf3, 0xf5, 0xf8, 0xf9, 0xfa, 0xfc, 0xfd, 0xfe,
    ] {
        codes.push(vec![0x66, 0x0f, op, 0xc1, 0x1b]);
    }
    for op in [0x10, 0x11, 0x6f, 0x70, 0x7e, 0x7f, 0xd6] {
        codes.push(vec![0xf3, 0x0f, op, 0xc1, 0x1b]);
    }
    for op in [0x10, 0x11, 0x70, 0xd6] {
        codes.push(vec![0xf2, 0x0f, op, 0xc1, 0x1b]);
    }
    codes.push(sse_memory_code(&[], 0xc3, 0x300));
    for code in codes {
        assert!(block(&code, 0, 1).instructions.is_empty(), "{code:02x?}");
        let mut after_nop = vec![0x90];
        after_nop.extend_from_slice(&code);
        let decoded = block(&after_nop, 0, 2);
        assert_eq!(decoded.byte_len, 1, "{code:02x?}");
        assert_eq!(decoded.instructions.len(), 1);
    }
}
