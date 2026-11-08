//! Execute x86 bytes through both engines on the ARM64 test device. Operand
//! values cover carries, signed overflow, parity, partial-register aliasing and
//! untouched flag bits; explicit ISA examples complement differential checks.

use super::*;
use crate::x86::jit::{
    code::{CodeArena, CodeHandle},
    decode,
};
use crate::x86::{BatchStop, Bus, Cpu, Interpreter, MemError, PhysAddr};

struct Bytes(Vec<u8>);
impl Bus for Bytes {
    fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError> {
        self.0
            .get(addr.0 as usize)
            .map(|_| ())
            .ok_or(MemError::OutOfRange { addr })
    }
    fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
        self.0
            .get(addr.0 as usize)
            .copied()
            .ok_or(MemError::OutOfRange { addr })
    }
    fn write_phys_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
        *self
            .0
            .get_mut(addr.0 as usize)
            .ok_or(MemError::OutOfRange { addr })? = value;
        Ok(())
    }
}

fn initial(seed: u32) -> Cpu {
    let mut cpu = Cpu::new();
    cpu.enter_protected_flat(0x100, 0);
    let edges = [
        0,
        1,
        0x7f,
        0x80,
        0xff,
        0x7fff,
        0x8000,
        0xffff,
        0x7fff_ffff,
        0x8000_0000,
        u32::MAX,
    ];
    let mut random = seed.wrapping_add(17);
    for (index, register) in cpu.regs.iter_mut().enumerate() {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        *register = if seed < edges.len() as u32 {
            edges[(seed as usize + index) % edges.len()]
        } else {
            random
        };
    }
    // Includes flags not modified by these operations and both incoming carries.
    cpu.replace_eflags(0x2402 | (seed.wrapping_mul(0x155) & STATUS));
    cpu.set_tsc(u64::MAX - seed as u64);
    cpu
}

struct Fixture {
    arena: CodeArena,
    entry: CodeHandle,
    body: usize,
    finish: usize,
}

#[test]
fn native_partial_flag_sequences_keep_encoded_auxiliary_and_parity() {
    for alu in [0x01, 0x11, 0x19, 0x21, 0x29, 0x31, 0x39, 0x85] {
        for shift in [0xa4, 0xac, 0xa5, 0xad] {
            for consume_parity in [false, true] {
                let mut code = vec![alu, 0xd8];
                if consume_parity {
                    code.extend([0x0f, 0x9a, 0xc2]);
                }
                code.extend([0x0f, shift, 0xd8]);
                if shift & 1 == 0 {
                    code.push(3);
                }
                compare(&code, 80);
            }
        }
    }
}

#[test]
fn native_segment_reads_obey_operand_width_and_preserve_flags() {
    for segment in 0..6 {
        for dst in 0..8 {
            compare(&[0x8c, 0xc0 | segment << 3 | dst], 24);
            compare(&[0x66, 0x8c, 0xc0 | segment << 3 | dst], 24);
        }
    }
}

#[test]
fn native_bit_modifications_preserve_register_aliases_and_other_flags() {
    for bits in [16, 32] {
        for opcode in [0xa3, 0xab, 0xb3, 0xbb] {
            for dst in 0..8 {
                for src in 0..8 {
                    let mut code = Vec::new();
                    if bits == 16 {
                        code.push(0x66);
                    }
                    code.extend([0x0f, opcode, 0xc0 | (src << 3) | dst]);
                    compare(&code, 24);
                }
            }
        }
        for group in 4..=7 {
            for bit in [0, 15, 16, 31, 32, 255] {
                let mut code = Vec::new();
                if bits == 16 {
                    code.push(0x66);
                }
                code.extend([0x0f, 0xba, 0xc0 | (group << 3), bit]);
                compare(&code, 24);
            }
        }
    }
}

impl Fixture {
    fn new(decoded: &decode::Block) -> Self {
        let mut arena = CodeArena::new(64 * 1024).unwrap();
        let stubs = entry().unwrap();
        let entry = arena.publish(&stubs.words).unwrap();
        let mut code = block(decoded, None, 0).unwrap();
        assert!(code.faults.is_empty(), "register-only compiler fixture");
        let body_handle = arena.publish_block(&mut code).unwrap();
        let body = arena.address(body_handle, 0).unwrap();
        let finish = arena.address(entry, stubs.return_offset).unwrap();
        Self {
            arena,
            entry,
            body,
            finish,
        }
    }

    fn invoke(&self, frame: &mut Frame) {
        frame.native_entry = self.body;
        // Register-only fixtures execute one block, with no link table.
        frame.dispatch_entry = self.finish;
        frame.call_dispatch_entry = self.finish;
        frame.return_entry = self.finish;
        // SAFETY: this fixture contains only locally compiled register operations
        // with the shared frame ABI. All three entry addresses belong to its
        // live arena, which stays immutable during invocation.
        unsafe { self.arena.invoke(self.entry, frame).unwrap() };
    }
}

fn compare(bytes: &[u8], seeds: u32) {
    let decoded = decode::block(bytes, 0x100, 64);
    assert_eq!(decoded.byte_len, bytes.len(), "{bytes:02x?}");
    let count = decoded.instructions.len() as u64;
    let code = Fixture::new(&decoded);
    let mut memory = Bytes(vec![0; 0x10000]);
    memory.0[0x100..0x100 + bytes.len()].copy_from_slice(bytes);
    for seed in 0..seeds {
        let mut expected = initial(seed);
        let mut actual = Frame::from_cpu(&expected, count);
        assert_eq!(
            Interpreter::run_batch(&mut expected, &mut memory, 0, count).stop,
            BatchStop::Budget,
            "{bytes:02x?}"
        );
        code.invoke(&mut actual);
        assert_eq!(actual.regs, expected.regs, "seed={seed} bytes={bytes:02x?}");
        assert_eq!(
            actual.eflags,
            expected.effective_eflags(),
            "seed={seed} bytes={bytes:02x?}"
        );
        assert_eq!(actual.eip, expected.eip, "seed={seed} bytes={bytes:02x?}");
        assert_eq!(actual.tsc, expected.tsc());
        assert_eq!(actual.completed, count);
        assert_eq!(actual.remaining, 0);
    }
    let cpu = initial(7);
    let mut short = Frame::from_cpu(&cpu, count - 1);
    code.invoke(&mut short);
    assert_eq!(short.regs, cpu.regs);
    assert_eq!(short.eflags, cpu.effective_eflags());
    assert_eq!(short.eip, cpu.eip);
    assert_eq!(short.completed, 0);
    assert_eq!(short.tsc, cpu.tsc());
}

#[test]
fn native_fault_records_reconstruct_retirement_before_the_access() {
    let decoded = decode::block(&[0x40, 0x8b, 0x03, 0x89, 0x0a], 0x100, 64);
    // INC EAX; MOV EAX,[EBX]; MOV [EDX],ECX. Both memory instructions
    // must reconstruct only the earlier retired instructions at a native fault.
    let code = block(&decoded, None, 0).unwrap();
    assert_eq!(code.faults.len(), 2);
    for (fault, eip, prior) in [(&code.faults[0], 0x101, 1), (&code.faults[1], 0x103, 2)] {
        assert_eq!(fault.guest_eip, eip);
        for initial in [10u64, 1 << 63, u64::MAX] {
            let earlier = 7;
            let live = initial - earlier - decoded.instructions.len() as u64;
            let retired = (initial - live).wrapping_add_signed(i64::from(fault.steps_adjust));
            assert_eq!(retired, earlier + prior);
        }
    }
}

#[test]
fn native_budget_guards_preserve_full_unsigned_remaining_counts() {
    for count in [1, 7, 31, 32, 33, 64] {
        let bytes = vec![0x40; count]; // INC EAX, preserving CF.
        let decoded = decode::block(&bytes, 0x100, count);
        let code = Fixture::new(&decoded);
        let mut memory = Bytes(vec![0; 0x10000]);
        memory.0[0x100..0x100 + bytes.len()].copy_from_slice(&bytes);
        for budget in [
            0,
            1,
            count as u64 - 1,
            count as u64,
            count as u64 + 1,
            31,
            32,
            33,
            1 << 63,
            (1 << 63) + 31,
            (1 << 63) + 32,
            u64::MAX,
        ] {
            let mut expected = initial(7);
            expected.set_tsc(u64::MAX - 5);
            let mut actual = Frame::from_cpu(&expected, budget);
            code.invoke(&mut actual);
            let completed = if budget >= count as u64 {
                count as u64
            } else {
                0
            };
            Interpreter::run_batch(&mut expected, &mut memory, 0, completed);
            assert_eq!(actual.completed, completed, "count={count} budget={budget}");
            assert_eq!(actual.remaining, budget - completed);
            assert_eq!(actual.regs, expected.regs);
            assert_eq!(actual.eflags, expected.effective_eflags());
            assert_eq!(actual.eip, expected.eip);
            assert_eq!(actual.tsc, expected.tsc());
        }
    }
}

#[test]
fn native_forward_comparisons_preserve_taken_flags_and_exact_prefix_budget() {
    for writer in [
        vec![0x39, 0xd8], // CMP EAX,EBX.
        vec![0x85, 0xd8], // TEST EAX,EBX.
        vec![0x85, 0xc0], // TEST EAX,EAX.
        vec![0x83, 0xf8, 1],
        vec![0x83, 0xf8, 0xff],
        vec![0x66, 0x39, 0xd8],
        vec![0x84, 0xdc], // TEST AH,BL.
    ] {
        for condition in 0..16 {
            for overwrite_input in [false, true] {
                let mut bytes = writer.clone();
                bytes.extend([
                    0x70 | condition,
                    4,
                    0x89,
                    if overwrite_input { 0xc8 } else { 0xca }, // MOV EAX/EDX,ECX.
                    0x31,
                    0xc9, // XOR ECX,ECX kills earlier flags only on fallthrough.
                ]);
                let decoded = decode::block(&bytes, 0x100, 64);
                let code = Fixture::new(&decoded);
                let mut memory = Bytes(vec![0; 0x10000]);
                memory.0[0x100..0x100 + bytes.len()].copy_from_slice(&bytes);
                for seed in 0..80 {
                    for budget in [1, 4, 7] {
                        let mut expected = initial(seed);
                        let mut actual = Frame::from_cpu(&expected, budget);
                        code.invoke(&mut actual);
                        let completed = actual.completed;
                        Interpreter::run_batch(&mut expected, &mut memory, 0, completed);
                        assert_eq!(actual.regs, expected.regs, "{bytes:x?} seed={seed}");
                        assert_eq!(
                            actual.eflags,
                            expected.effective_eflags(),
                            "{bytes:x?} seed={seed}"
                        );
                        assert_eq!(actual.eip, expected.eip);
                        assert_eq!(actual.tsc, expected.tsc());
                        assert_eq!(actual.remaining, budget - completed);
                    }
                }
            }
        }
    }
}

#[test]
fn native_bit_scans_match_zero_aliases_partial_writes_and_flags() {
    for bits in [16, 32] {
        for opcode in [0xbc, 0xbd] {
            for dst in 0..8 {
                for src in [0, 1, 4, 7, dst] {
                    let mut bytes = vec![0x47]; // An earlier flag writer.
                    if bits == 16 {
                        bytes.push(0x66);
                    }
                    bytes.extend([0x0f, opcode, 0xc0 | dst << 3 | src]);
                    compare(&bytes, 96);
                    bytes.extend([0x31, 0xdb]); // Dead output ZF still needs a zero-source guard.
                    compare(&bytes, 96);
                }
            }
        }
    }
}

#[test]
fn native_division_checks_faults_before_register_and_flag_commit() {
    let mut successes = 0;
    let mut faults = 0;
    for bits in [8, 16, 32] {
        for signed in [false, true] {
            for src in 0..8 {
                let mut bytes = vec![0x47]; // INC EDI must remain observable at #DE.
                if bits == 16 {
                    bytes.push(0x66);
                }
                bytes.extend([
                    if bits == 8 { 0xf6 } else { 0xf7 },
                    0xf0 | (u8::from(signed) << 3) | src,
                    0x31,
                    0xdb,
                ]);
                let decoded = decode::block(&bytes, 0x100, 64);
                assert_eq!(decoded.byte_len, bytes.len());
                let code = Fixture::new(&decoded);
                let mut memory = Bytes(vec![0; 0x10000]);
                memory.0[0x100..0x100 + bytes.len()].copy_from_slice(&bytes);
                for seed in 0..192 {
                    let mut expected = initial(seed);
                    if seed & 1 == 0 {
                        expected.regs[2] = 0;
                    }
                    let mut actual = Frame::from_cpu(&expected, 3);
                    let reference = Interpreter::run_batch(&mut expected, &mut memory, 0, 3);
                    code.invoke(&mut actual);
                    assert_eq!(
                        actual.regs, expected.regs,
                        "bits={bits} signed={signed} src={src} seed={seed}"
                    );
                    assert_eq!(actual.eflags, expected.effective_eflags());
                    if matches!(reference.stop, BatchStop::Budget) {
                        successes += 1;
                        assert_eq!(actual.completed, 3);
                        assert_eq!(actual.eip, expected.eip);
                        assert_eq!(actual.tsc, expected.tsc());
                    } else {
                        faults += 1;
                        assert!(matches!(
                            reference.stop,
                            BatchStop::Step {
                                reason: crate::x86::BatchStepStop::Error(
                                    crate::x86::StepError::DivideError { eip: 0x101 }
                                ),
                                ..
                            }
                        ));
                        assert_eq!(actual.exit_kind, 4);
                        assert_eq!(actual.completed, 1);
                        assert_eq!(actual.eip, 0x101);
                        assert_eq!(actual.tsc.wrapping_add(1), expected.tsc());
                    }
                }
            }
        }
    }
    assert!(successes > 1000 && faults > 1000);
}

#[test]
fn native_exchange_families_preserve_aliases_partial_registers_and_flags() {
    for bits in [8, 16, 32] {
        for opcode in [0x86, 0xb0, 0xc0] {
            for dst in 0..8 {
                for src in [0, 1, 4, 7, dst] {
                    let mut bytes = Vec::new();
                    if bits == 16 {
                        bytes.push(0x66);
                    }
                    if opcode != 0x86 {
                        bytes.push(0x0f);
                    }
                    bytes.extend([opcode + u8::from(bits != 8), 0xc0 | (src << 3) | dst]);
                    compare(&bytes, 64);
                    // A following writer makes these output flags dead while
                    // CMPXCHG must still choose the correct register update.
                    bytes.extend([0x31, 0xff]);
                    compare(&bytes, 32);
                }
            }
        }
    }
}

#[test]
fn native_direction_and_sign_extension_preserve_other_state() {
    for bytes in [
        &[0x98][..],
        &[0x99],
        &[0x66, 0x98],
        &[0x66, 0x99],
        &[0xfc],
        &[0xfd],
        &[0xfc, 0x40, 0xfd, 0x98, 0x99],
    ] {
        compare(bytes, 256);
    }
}

#[test]
fn native_imul_preserves_undefined_flags_and_matches_signed_truncation() {
    for bits in [16, 32] {
        for dst in 0..8 {
            for src in [0, 1, 4, 7, dst] {
                let mut bytes = Vec::new();
                if bits == 16 {
                    bytes.push(0x66);
                }
                let prefix = bytes.clone();
                bytes.extend([0x0f, 0xaf, 0xc0 | (dst << 3) | src]);
                compare(&bytes, 128);
                for immediate in [0u32, 1, 127, 128, 0x8000, 0x8000_0000, u32::MAX] {
                    for opcode in [0x69, 0x6b] {
                        let mut bytes = prefix.clone();
                        bytes.extend([opcode, 0xc0 | (dst << 3) | src]);
                        bytes.extend_from_slice(
                            &immediate.to_le_bytes()[..if opcode == 0x6b { 1 } else { bits / 8 }],
                        );
                        compare(&bytes, 64);
                    }
                }
            }
        }
    }
    for bytes in [
        &[0x01, 0xc8, 0x0f, 0xaf, 0xc3, 0x0f, 0x9a, 0xc2][..],
        &[0x0f, 0xaf, 0xc3, 0x11, 0xc2, 0x29, 0xc9][..],
        &[0x0f, 0xaf, 0xc3, 0x31, 0xc9][..],
    ] {
        compare(bytes, 128);
    }
    for (operand16, lhs, rhs, result, overflow) in [
        (false, 0x8000_0000, u32::MAX, 0x8000_0000, true),
        (false, 0x8000_0000, 1, 0x8000_0000, false),
        (false, u32::MAX, u32::MAX, 1, false),
        (true, 0xa5a5_8000, 0xffff, 0xa5a5_8000, true),
        (true, 0xa5a5_8000, 1, 0xa5a5_8000, false),
        (true, 0xa5a5_ffff, 0xffff, 0xa5a5_0001, false),
    ] {
        let bytes = if operand16 {
            &[0x66, 0x0f, 0xaf, 0xc3][..]
        } else {
            &[0x0f, 0xaf, 0xc3][..]
        };
        let code = Fixture::new(&decode::block(bytes, 0x100, 1));
        for flags in [2, 0x2402 | STATUS] {
            let mut cpu = initial(0);
            cpu.regs[0] = lhs;
            cpu.regs[3] = rhs;
            cpu.replace_eflags(flags);
            let mut frame = Frame::from_cpu(&cpu, 1);
            code.invoke(&mut frame);
            assert_eq!(frame.regs[0], result);
            assert_eq!(
                frame.eflags,
                (flags & !0x801) | if overflow { 0x801 } else { 0 }
            );
        }
    }
}

#[test]
fn native_full_multiply_preserves_undefined_flags_and_output_aliases() {
    for bits in [8, 16, 32] {
        for group in [4, 5] {
            for source in 0..8 {
                let mut bytes = if bits == 16 { vec![0x66] } else { vec![] };
                bytes.extend([
                    if bits == 8 { 0xf6 } else { 0xf7 },
                    0xc0 | group << 3 | source,
                ]);
                compare(&bytes, 128);
                // CF feeds ADC even when its later status writes make MUL's
                // OF dead; the earlier add's PF/AF/SF/ZF survive MUL itself.
                let mut chain = vec![0x01, 0xcb];
                chain.extend(&bytes);
                chain.extend([0x11, 0xde, 0x31, 0xc9]);
                compare(&chain, 128);
            }
        }
    }
    for (bits, signed, lhs, rhs, low, high, overflow) in [
        (32, false, u32::MAX, 2, 0xffff_fffe, 1, true),
        (32, false, 1, u32::MAX, u32::MAX, 0, false),
        (32, true, 0x8000_0000, u32::MAX, 0x8000_0000, 0, true),
        (32, true, 0x8000_0000, 1, 0x8000_0000, u32::MAX, false),
        (
            16,
            true,
            0x1122_8000,
            0xffff,
            0x1122_8000,
            0x90ab_0000,
            true,
        ),
        (16, true, 0x1122_8000, 1, 0x1122_8000, 0x90ab_ffff, false),
        (8, true, 0x1122_3380, 0xff, 0x1122_0080, 0x90ab_cdef, true),
        (8, true, 0x1122_3380, 1, 0x1122_ff80, 0x90ab_cdef, false),
        (8, false, 0x1122_33ff, 2, 0x1122_01fe, 0x90ab_cdef, true),
    ] {
        let mut bytes = if bits == 16 { vec![0x66] } else { vec![] };
        bytes.extend([
            if bits == 8 { 0xf6 } else { 0xf7 },
            if signed { 0xeb } else { 0xe3 },
        ]);
        let code = Fixture::new(&decode::block(&bytes, 0x100, 1));
        for flags in [2, 0x2402 | STATUS] {
            let mut cpu = initial(0);
            cpu.regs[0] = lhs;
            cpu.regs[2] = 0x90ab_cdef;
            cpu.regs[3] = rhs;
            cpu.replace_eflags(flags);
            let mut frame = Frame::from_cpu(&cpu, 1);
            code.invoke(&mut frame);
            assert_eq!(frame.regs[0], low);
            assert_eq!(frame.regs[2], high);
            assert_eq!(
                frame.eflags,
                (flags & !0x801) | if overflow { 0x801 } else { 0 }
            );
        }
    }
}

#[test]
fn native_negate_matches_subtraction_widths_high_bytes_and_live_flags() {
    for bits in [8, 16, 32] {
        for dst in 0..8 {
            let mut bytes = if bits == 16 { vec![0x66] } else { vec![] };
            bytes.extend([if bits == 8 { 0xf6 } else { 0xf7 }, 0xd8 | dst]);
            compare(&bytes, 128);
            bytes.extend([0x11, 0xde, 0x31, 0xc9]);
            compare(&bytes, 128);
        }
    }
}

#[test]
fn native_lea_wraps_scaled_indices_displacements_and_destination_aliases() {
    for bits in [16, 32] {
        for scale in 0..4 {
            for index in [0, 1, 4, 7] {
                for base in [0, 4, 5, 7] {
                    for dst in [0, 4] {
                        for displacement in [0u32, 127, 4095, 4096, 0xffff_f001, 0xffff_f000] {
                            let mut bytes = Vec::new();
                            if bits == 16 {
                                bytes.push(0x66);
                            }
                            bytes.extend([
                                0x8d,
                                0x84 | (dst << 3),
                                (scale << 6) | (index << 3) | base,
                            ]);
                            bytes.extend(displacement.to_le_bytes());
                            compare(&bytes, 32);
                        }
                    }
                }
            }
        }
    }
    for sib in [0x05, 0x45, 0x85, 0xc5, 0x25] {
        compare(&[0x64, 0x8d, 0x04, sib, 0, 0, 0, 0], 64);
    }
}

#[test]
fn native_double_shifts_preserve_flags_and_handle_register_aliases() {
    for opcode in [0xa4, 0xac] {
        for modrm in [0xc0, 0xc1, 0xc8, 0xc9, 0xd8, 0xe4, 0xff] {
            for count in [0, 1, 2, 7, 16, 31, 32, 33, 255] {
                compare(&[0x40, 0x0f, opcode, modrm, count], 64);
            }
            compare(&[0x40, 0x0f, opcode + 1, modrm], 128);
        }
    }
}

#[test]
fn native_byte_swap_conditional_moves_and_partial_flags() {
    for reg in 0..8 {
        compare(&[0x01, 0xd0, 0x0f, 0xc8 | reg], 64);
    }
    for prefix in [&[][..], &[0x66][..]] {
        for condition in 0..16 {
            for modrm in [0xc2, 0xc0, 0xc8] {
                let mut code = vec![0x01, 0xd0];
                code.extend(prefix);
                code.extend([0x0f, 0x40 | condition, modrm, 0x31, 0xdb]);
                compare(&code, 64); // input flags stay live even when outputs are killed
            }
        }
    }
    for opcode in [0x9e, 0x9f, 0xf5, 0xf8, 0xf9] {
        compare(&[opcode], 64);
        compare(&[0x01, 0xd0, opcode, 0x11, 0xd3, 0x9f], 64);
    }
    // SAHF installs AF/PF without disturbing OF; LAHF must preserve AL and
    // upper EAX, including after carry changes and prior arithmetic.
    compare(&[0x01, 0xd0, 0x9f, 0xf5, 0x9e, 0x0f, 0x9a, 0xc2], 64);
    for code in [
        &[0xe3, 0x20][..],
        &[0x39, 0xd0, 0xe3, 0xfc],
        &[0xb9, 0, 0, 1, 0, 0xe3, 2],
    ] {
        compare(code, 32);
    }
}

#[test]
fn native_shifts_match_masked_counts_widths_and_cl_aliases() {
    for group in [0, 1, 2, 3, 4, 5, 6, 7] {
        for bits in [8, 16, 32] {
            for encoded in [0, 1, 4, 5, 7] {
                let prefix = if bits == 16 { vec![0x66] } else { vec![] };
                let modrm = 0xc0 | group << 3 | encoded;
                for count in [0, 1, 2, 7, 8, 9, 15, 16, 17, 31, 32, 33, 255] {
                    let mut bytes = prefix.clone();
                    bytes.extend([if bits == 8 { 0xc0 } else { 0xc1 }, modrm, count]);
                    compare(&bytes, 32);
                }
                // Preceding flags must stay live when CL is zero; read the old
                // count before changing ECX/CX/CL/CH when either is the destination.
                let mut bytes = vec![0x40];
                bytes.extend(prefix);
                bytes.extend([if bits == 8 { 0xd2 } else { 0xd3 }, modrm]);
                compare(&bytes, 128);
            }
        }
    }
}

#[test]
fn native_narrow_alu_ignores_upper_inputs_and_preserves_upper_destinations() {
    for (bytes, eax, ecx, result, status) in [
        (
            &[0x00, 0xc8][..],
            0xaabb_ccff,
            0x1122_3301,
            0xaabb_cc00,
            0x55,
        ), // ADD AL,CL.
        (
            &[0x66, 0x01, 0xc8],
            0xaabb_7fff,
            0x1122_0001,
            0xaabb_8000,
            0x894,
        ), // ADD AX,CX.
        (&[0x18, 0xc8], 0xaabb_cc00, 0x1122_3300, 0xaabb_ccff, 0x95), // SBB AL,CL with CF.
        (
            &[0x66, 0x11, 0xc8],
            0xaabb_ffff,
            0x1122_0000,
            0xaabb_0000,
            0x55,
        ), // ADC AX,CX with CF.
        (&[0x84, 0xc0], 0xaabb_cc00, 0x1122_3344, 0xaabb_cc00, 0x44), // TEST AL,AL.
        (
            &[0x66, 0x83, 0xf8, 0],
            0xaabb_0000,
            0x1122_3344,
            0xaabb_0000,
            0x44,
        ), // CMP AX,0.
        (&[0x20, 0xc8], 0xaabb_cc80, 0x1122_337f, 0xaabb_cc00, 0x44), // AND AL,CL.
        (&[0x08, 0xc8], 0xaabb_cc00, 0x1122_3300, 0xaabb_cc00, 0x44), // OR AL,CL.
        (
            &[0x66, 0x31, 0xc8],
            0xaabb_8000,
            0x1122_8000,
            0xaabb_0000,
            0x44,
        ), // XOR AX,CX.
        (
            &[0x66, 0xf7, 0xd8],
            0xaabb_8000,
            0x1122_3344,
            0xaabb_8000,
            0x885,
        ), // NEG AX.
    ] {
        let code = Fixture::new(&decode::block(bytes, 0x100, 1));
        let mut cpu = initial(0);
        cpu.regs[0] = eax;
        cpu.regs[1] = ecx;
        cpu.replace_eflags(0x2402 | STATUS); // Carry input and stale flags to replace.
        let mut frame = Frame::from_cpu(&cpu, 1);
        code.invoke(&mut frame);
        let mut expected = cpu.regs;
        expected[0] = result;
        assert_eq!(frame.regs, expected, "{bytes:x?}");
        assert_eq!(frame.eflags, 0x2402 | status, "{bytes:x?}");
        assert_eq!(frame.completed, 1);
        assert_eq!(frame.tsc, cpu.tsc().wrapping_add(1));
    }
}

#[test]
fn native_x86_arithmetic_matches_interpreter_across_widths_and_aliases() {
    for group in 0..8 {
        for word in [false, true] {
            for byte in [false, true] {
                // The reference interpreter has no redundant 66+byte lowering.
                if word && byte {
                    continue;
                }
                for modrm in [0xc0, 0xc1, 0xe0, 0xe4, 0xff] {
                    let mut bytes = Vec::new();
                    if word {
                        bytes.push(0x66);
                    }
                    bytes.extend([group * 8 + u8::from(!byte), modrm]);
                    compare(&bytes, 96);
                }
            }
            let mut immediate = if word { vec![0x66] } else { Vec::new() };
            immediate.extend([0x83, 0xc0 | (group << 3), 0xff]);
            compare(&immediate, 96);
        }
    }
    for bytes in [
        &[0x40][..],
        &[0x4f],
        &[0x66, 0x40],
        &[0xfe, 0xc4],
        &[0xfe, 0xcc],
        &[0x85, 0xc8],
        &[0x83, 0xf8, 0],
        &[0x66, 0x83, 0xf8, 0],
        &[0x80, 0xfc, 0], // cmp ah,0
        &[0x84, 0xe4],    // test ah,ah
        &[0x66, 0x85, 0xc0],
        &[0x83, 0xf8, 0, 0x11, 0xdb, 0x31, 0xc0], // only CMP's carry is live
        &[0x84, 0xe4, 0x0f, 0x9a, 0xc3, 0x31, 0xc9], // TEST's parity remains live
        &[0x84, 0xe0],
        &[0x66, 0xa9, 0xff, 0x80],
        &[0x31, 0xc0, 0x48, 0x40],
        &[0x01, 0xc8, 0x41, 0x42],
        &[0xf6, 0xd4],
        &[0xf7, 0xd0],
        &[0x86, 0xe0],
        &[0x66, 0x87, 0xd8],
        &[0xb4, 0xa5, 0x88, 0xe0],
        &[0x0f, 0xbe, 0xc4],
        &[0x66, 0x0f, 0xb6, 0xe7],
        &[0x0f, 0xbf, 0xd8],
        &[0x0f, 0xb7, 0xf5],
    ] {
        compare(bytes, 128);
    }
}

#[test]
fn native_negative_constants_keep_carry_overflow_and_auxiliary() {
    for value in [1_u32, 15, 16, 17, 127, 128, 255, 256, 4095, 4096] {
        for value in [value, value.wrapping_neg()] {
            for group in [0, 5, 7] {
                for register in [0, 4, 7] {
                    let mut bytes = vec![0x81, 0xc0 | group << 3 | register];
                    bytes.extend(value.to_le_bytes());
                    compare(&bytes, 192);
                    // INC makes the earlier carry the only live arithmetic
                    // output. A second complete writer then makes it dead.
                    bytes.push(0x43);
                    compare(&bytes, 96);
                    bytes.extend([0x31, 0xdb]);
                    compare(&bytes, 96);
                }
            }
        }
    }
}

#[test]
fn native_logical_constants_match_masks_registers_and_live_flags() {
    let mut masks = vec![0, u32::MAX, 0x1234_5678, 0x8000_0101];
    // Independently construct every run length and rotation at each element
    // width, then replicate it through a 32-bit operand.
    for width in [2, 4, 8, 16, 32] {
        for length in 1..width {
            for first in 0..width {
                let mut mask = 0;
                for base in (0..32).step_by(width) {
                    for bit in 0..length {
                        mask |= 1 << (base + (first + bit) % width);
                    }
                }
                masks.push(mask);
            }
        }
    }
    for mask in masks {
        for group in [1, 4, 6, 8] {
            // OR, AND, XOR and TEST.
            let mut bytes = if group == 8 {
                vec![0xf7, 0xc4]
            } else {
                vec![0x81, 0xc4 | group << 3]
            };
            bytes.extend(mask.to_le_bytes());
            compare(&bytes, 24);
            // Consume PF before a complete writer makes the other outputs dead.
            bytes.extend([0x0f, 0x9a, 0xc0, 0x31, 0xdb]);
            compare(&bytes, 24);
        }
    }
}

#[test]
fn native_carry_chains_keep_inputs_live_when_output_flags_are_dead() {
    for bits in [8, 16, 32] {
        for group in [2, 3] {
            for immediate in [0, 1, 0x7f, 0x80, 0xff] {
                let mut bytes = vec![0x01, 0xc0]; // ADD produces the incoming CF.
                if bits == 16 {
                    bytes.push(0x66);
                }
                bytes.extend([
                    if bits == 8 { 0x80 } else { 0x83 },
                    0xc1 | (group << 3),
                    immediate,
                ]);
                // These flags kill the ADC/SBB outputs, but its value still
                // depends on the preceding ADD's carry.
                bytes.extend([0x31, 0xdb]);
                compare(&bytes, 128);
            }
        }
    }
    // Edge carries/borrows followed by another carry consumer.
    for bytes in [
        &[0x31, 0xc0, 0x83, 0xe8, 1, 0x83, 0xd0, 0][..],
        &[0x31, 0xc0, 0x83, 0xe8, 1, 0x83, 0xd8, 0xff],
        &[0x31, 0xc0, 0x83, 0xe8, 1, 0x14, 0, 0x10, 0xc0],
        &[0x31, 0xc0, 0x83, 0xe8, 1, 0x1c, 0xff, 0x18, 0xc0],
        &[0x31, 0xc0, 0x83, 0xe8, 1, 0x66, 0x15, 0, 0],
        &[0x31, 0xc0, 0x83, 0xe8, 1, 0x66, 0x1d, 0xff, 0xff],
    ] {
        compare(bytes, 16);
    }
}

#[test]
fn native_x86_conditions_match_every_status_flag_combination() {
    for condition in 0..16 {
        compare(&[0x70 | condition, 0xf9], 512);
        compare(&[0x0f, 0x80 | condition, 0xf0, 0xff, 0xff, 0xff], 512);
        compare(&[0x66, 0x70 | condition, 0xf9], 512);
        // Compare-generated conditions, including narrow signs, no-borrow
        // carry and the CMP x,0 / TEST logical flag convention.
        for bits in [8, 16, 32] {
            for opcode in [0x38, 0x84] {
                for rhs in [0xc0, 0xd8] {
                    let mut code = if bits == 16 { vec![0x66] } else { vec![] };
                    code.extend([opcode | u8::from(bits != 8), rhs, 0x70 | condition, 0xf0]);
                    compare(&code, 64);
                }
            }
            for immediate in [0, 1, 0x7f, 0xff] {
                let mut code = if bits == 16 { vec![0x66] } else { vec![] };
                code.extend([
                    if bits == 8 { 0x80 } else { 0x83 },
                    0xf8,
                    immediate,
                    0x70 | condition,
                    0xf0,
                ]);
                compare(&code, 64);
            }
        }
        for reg in [0, 1, 4, 7] {
            compare(&[0x0f, 0x90 | condition, 0xc0 | reg], 512);
            compare(
                &[0x01, 0xc0, 0x0f, 0x90 | condition, 0xc0 | reg, 0x31, 0xdb],
                64,
            );
        }
    }
    compare(&[0x31, 0xc0, 0x74, 0xfc], 8);
    compare(&[0xe9, 0xf0, 0xff, 0xff, 0xff], 8);
}

#[test]
fn native_bit_test_masks_indices_and_preserves_other_flags() {
    for bits in [16, 32] {
        for reg in 0..8 {
            let prefix = if bits == 16 { vec![0x66] } else { vec![] };
            for bit in [0, 1, 7, 15, 16, 31, 32, 255] {
                let mut bytes = vec![0x40];
                bytes.extend(&prefix);
                bytes.extend([0x0f, 0xba, 0xe0 | reg, bit]);
                compare(&bytes, 128);
            }
            let mut bytes = prefix;
            bytes.extend([0x0f, 0xa3, 0xc8 | reg]); // register index, including aliases
            compare(&bytes, 128);
        }
    }
}

#[test]
fn architectural_partial_add_and_inc_preserve_unwritten_bits_and_carry() {
    let decoded = decode::block(&[0xb4, 0xff, 0x80, 0xc4, 1, 0x66, 0x40], 0x100, 64);
    let code = Fixture::new(&decoded);
    let mut cpu = initial(0);
    cpu.regs[0] = 0x1234_5600;
    let mut frame = Frame::from_cpu(&cpu, 3);
    code.invoke(&mut frame);
    assert_eq!(frame.regs[0], 0x1234_0001);
    assert_eq!(frame.eflags & STATUS, 1);
}
