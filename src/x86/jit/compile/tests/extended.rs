//! Extended-decoder operations: native effects and precise interpreter exits.
use super::mmx::{Memory, invoke, same};
use super::*;
use crate::x86::cpu::{CR0_PG, CR0_WP, SEGMENT_FLAG_DEFAULT_BIG, SegmentReg};
use crate::x86::interp::{BatchStepStop, StepError};
use crate::x86::{LinearAddr, mmu};

fn fixture(bytes: &[u8], eip: u32) -> Fixture {
    let decoded = decode::block_extended(bytes, eip, 32);
    assert_eq!(decoded.byte_len, bytes.len());
    Fixture::new(&decoded)
}

fn setup(bytes: &[u8]) -> (Cpu, Memory) {
    let mut cpu = initial(29);
    cpu.regs = core::array::from_fn(|i| 0x1234_567u32.wrapping_mul(i as u32 + 1) ^ 0xdead_0000);
    cpu.regs[4] = 0x6800;
    cpu.cr0 |= CR0_PG | CR0_WP;
    cpu.cr3 = 0x4000;
    cpu.cr2 = 0xdead_beef;
    let mut memory = Memory(vec![0; 0x10000]);
    memory.word(0x4000, 0x5067);
    memory.word(0x5000, 0x2067);
    memory.word(0x5018, 0x8067);
    memory.word(0x501c, 0x9067);
    memory.0[0x2100..0x2100 + bytes.len()].copy_from_slice(bytes);
    memory.0[0x8000..0xa000].fill(0xa5);
    (cpu, memory)
}

#[test]
fn native_extended_loops_keep_flags_counts_and_both_targets() {
    for opcode in 0xe0..=0xe2 {
        for displacement in [0xfe, 4] {
            let bytes = [opcode, displacement];
            let code = fixture(&bytes, 0x100);
            for count in [0u32, 1, 2] {
                for flags in [0x402, 0x402 | STATUS] {
                    let mut cpu = initial(29);
                    cpu.regs[1] = count;
                    cpu.replace_eflags(flags);
                    let mut expected = cpu.clone();
                    let mut memory = Bytes(vec![0; 0x1000]);
                    memory.0[0x100..0x102].copy_from_slice(&bytes);
                    assert_eq!(
                        Interpreter::run_batch(&mut expected, &mut memory, 0, 1).ran,
                        1
                    );
                    let mut frame = Frame::from_cpu(&cpu, 1);
                    code.invoke(&mut frame);
                    frame.commit(&mut cpu);
                    same(&cpu, &expected);
                    assert_eq!(cpu.regs[1], count.wrapping_sub(1));
                    assert_eq!(cpu.effective_eflags(), flags);
                    assert_eq!((frame.completed, frame.remaining), (1, 0));
                }
            }
        }
        // Arithmetic before the loop must remain visible on either edge.
        for count in [1, 2] {
            for value in [0, 1, 0x7fff_ffff, u32::MAX] {
                let bytes = [0x01, 0xd8, opcode, 4];
                let code = fixture(&bytes, 0x100);
                let mut cpu = initial(29);
                cpu.regs[0] = value;
                cpu.regs[3] = 1;
                cpu.regs[1] = count;
                let mut expected = cpu.clone();
                let mut memory = Bytes(vec![0; 0x1000]);
                memory.0[0x100..0x104].copy_from_slice(&bytes);
                assert_eq!(
                    Interpreter::run_batch(&mut expected, &mut memory, 0, 2).ran,
                    2
                );
                let mut frame = Frame::from_cpu(&cpu, 2);
                code.invoke(&mut frame);
                frame.commit(&mut cpu);
                same(&cpu, &expected);
            }
        }
    }
}

#[test]
fn native_extended_loop_links_obey_backedge_budgets() {
    for opcode in 0xe0..=0xe2 {
        let bytes = [opcode, 0xfe];
        let decoded = decode::block_extended(&bytes, 0x100, 1);
        let mut arena = CodeArena::new(64 * 1024).unwrap();
        let stubs = entry().unwrap();
        let entry = arena.publish(&stubs.words).unwrap();
        let mut compiled = block(
            &decoded,
            Some(crate::x86::jit::links::Page { eip_start: 0 }),
            0,
        )
        .unwrap();
        let handle = arena.publish_block(&mut compiled).unwrap();
        let body = arena.address(handle, 0).unwrap();
        for edge in &compiled.edges {
            if edge.target_offset == 0x100 {
                assert!(arena.patch_branch(
                    arena.address(handle, edge.native_offset as usize).unwrap(),
                    body
                ));
            }
        }
        let code = Fixture {
            body,
            finish: arena.address(entry, stubs.return_offset).unwrap(),
            recheck: arena.address(entry, stubs.recheck_offset).unwrap(),
            arena,
            entry,
        };
        for count in [0, 1, 2, 7] {
            for budget in [0u64, 1, 2, 5] {
                let mut cpu = initial(17);
                cpu.regs[1] = count;
                cpu.replace_eflags(0x802 | if opcode == 0xe1 { 0x40 } else { 0 });
                let mut expected = cpu.clone();
                let mut memory = Bytes(vec![0; 0x1000]);
                memory.0[0x100..0x102].copy_from_slice(&bytes);
                let ran = budget.min(if count == 0 {
                    u64::MAX
                } else {
                    u64::from(count)
                });
                assert_eq!(
                    Interpreter::run_batch(&mut expected, &mut memory, 0, ran).ran,
                    ran
                );
                let mut frame = Frame::from_cpu(&cpu, budget);
                code.invoke(&mut frame);
                frame.commit(&mut cpu);
                same(&cpu, &expected);
                assert_eq!((frame.completed, frame.remaining), (ran, budget - ran));
            }
        }
    }
}

#[test]
fn native_extended_pushall_popall_keep_saved_sp_and_operand_width() {
    for bits in [16u8, 32] {
        let prefix: &[u8] = if bits == 16 { &[0x66] } else { &[] };
        let push_bytes: Vec<_> = prefix.iter().copied().chain([0x60]).collect();
        let pop_bytes: Vec<_> = prefix.iter().copied().chain([0x61]).collect();
        let bytes: Vec<_> = push_bytes.iter().chain(&pop_bytes).copied().collect();
        let push = fixture(&push_bytes, 0x100);
        let pop = fixture(&pop_bytes, 0x100 + push_bytes.len() as u32);
        for stack in [0x6800, 0x6fff] {
            let (mut expected, mut expected_bus) = setup(&bytes);
            expected.regs[4] = stack;
            let original = expected.regs;
            let mut actual = expected.clone();
            let mut actual_bus = Memory(expected_bus.0.clone());
            assert_eq!(
                Interpreter::run_batch(&mut expected, &mut expected_bus, 0, 1).stop,
                BatchStop::Budget
            );
            let frame = invoke(&push, &mut actual, &mut actual_bus, 1);
            assert_eq!((frame.exit_kind, frame.completed), (0, 1));
            same(&actual, &expected);
            assert_eq!(actual_bus.0, expected_bus.0);
            let base = (stack - u32::from(bits) + 0x2000) as usize;
            let width = usize::from(bits / 8);
            for (slot, register) in [7, 6, 5, 4, 3, 2, 1, 0].into_iter().enumerate() {
                assert_eq!(
                    &actual_bus.0[base + slot * width..base + (slot + 1) * width],
                    &original[register].to_le_bytes()[..width]
                );
            }
            actual_bus.0[base + 3 * width..base + 4 * width].fill(0xde);
            expected_bus.0[base + 3 * width..base + 4 * width].fill(0xde);
            for register in [0, 1, 2, 3, 5, 6, 7] {
                actual.regs[register] ^= 0xffff_55aa;
            }
            expected.regs = actual.regs;
            let before_pop = actual.regs;
            assert_eq!(
                Interpreter::run_batch(&mut expected, &mut expected_bus, 1, 1).stop,
                BatchStop::Budget
            );
            let frame = invoke(&pop, &mut actual, &mut actual_bus, 1);
            assert_eq!((frame.exit_kind, frame.completed), (0, 1));
            same(&actual, &expected);
            assert_eq!(actual_bus.0, expected_bus.0);
            for register in 0..8 {
                let value = if bits == 16 && register != 4 {
                    (before_pop[register] & 0xffff_0000) | (original[register] & 0xffff)
                } else {
                    original[register]
                };
                assert_eq!(actual.regs[register], value);
            }
        }
    }
}

#[test]
fn native_extended_stack_faults_preserve_uncommitted_state() {
    for (opcode, stack, missing, linear, error_code) in [
        (0x60, 0x6010, 0x5014, 0x5ffc, 2),
        (0x61, 0x6ff0, 0x501c, 0x7000, 0),
    ] {
        let bytes = [0x40, opcode];
        let code = fixture(&bytes, 0x100);
        let (mut expected, mut expected_bus) = setup(&bytes);
        expected.regs[4] = stack;
        expected_bus.word(missing, 0);
        let mut actual = expected.clone();
        let mut actual_bus = Memory(expected_bus.0.clone());
        let before = actual_bus.0.clone();
        let reference = Interpreter::run_batch(&mut expected, &mut expected_bus, 0, 2);
        let frame = invoke(&code, &mut actual, &mut actual_bus, 2);
        assert_eq!((frame.exit_kind, frame.completed, frame.eip), (1, 1, 0x101));
        assert_eq!(actual.regs[4], stack);
        assert_eq!(actual_bus.0, before);
        let resumed = Interpreter::run_batch(
            &mut actual,
            &mut actual_bus,
            frame.completed,
            frame.remaining,
        );
        assert_eq!(resumed.stop, reference.stop);
        let fault = StepError::PageFault(mmu::PageFault {
            linear: LinearAddr(linear),
            code: error_code,
        });
        assert!(
            matches!(resumed.stop, BatchStop::Step { instruction_eip: 0x101, reason: BatchStepStop::Error(e), .. } if e == fault),
            "{:?}",
            resumed.stop
        );
        same(&actual, &expected);
        assert_eq!(actual_bus.0, expected_bus.0);
        assert_eq!(actual.regs[4], stack);
    }
}

#[test]
fn native_extended_stack_qualification_exits_before_any_effect() {
    for opcode in [0x60, 0x61] {
        let bytes = [opcode];
        let code = fixture(&bytes, 0x100);
        for word_stack in [false, true] {
            let (mut expected, mut expected_bus) = setup(&bytes);
            let mut ss = expected.seg(SegmentReg::Ss);
            if word_stack {
                ss.flags &= !SEGMENT_FLAG_DEFAULT_BIG;
                ss.limit = 0xffff;
                expected.regs[4] |= 0xabcd_0000;
            } else {
                ss.base = 0x1000;
                expected.regs[4] -= 0x1000;
            }
            expected.set_seg_cache(SegmentReg::Ss, ss);
            let mut actual = expected.clone();
            let mut actual_bus = Memory(expected_bus.0.clone());
            let before = actual.clone();
            let frame = invoke(&code, &mut actual, &mut actual_bus, 1);
            assert_eq!((frame.exit_kind, frame.completed), (1, 0));
            same(&actual, &before);
            assert_eq!(actual_bus.0, expected_bus.0);
            let reference = Interpreter::run_batch(&mut expected, &mut expected_bus, 0, 1);
            let resumed = Interpreter::run_batch(&mut actual, &mut actual_bus, 0, 1);
            assert_eq!(reference.stop, BatchStop::Budget);
            assert_eq!(resumed.stop, reference.stop);
            same(&actual, &expected);
            assert_eq!(actual_bus.0, expected_bus.0);
        }
    }
}

#[test]
fn native_extended_push_cs_ss_store_selector_words() {
    for (opcode, segment) in [(0x0e, SegmentReg::Cs), (0x16, SegmentReg::Ss)] {
        for prefix in [false, true] {
            let bytes = if prefix {
                vec![0x66, opcode]
            } else {
                vec![opcode]
            };
            let code = fixture(&bytes, 0x100);
            let (mut expected, mut expected_bus) = setup(&bytes);
            let mut actual = expected.clone();
            let mut actual_bus = Memory(expected_bus.0.clone());
            let selector = expected.seg(segment).selector;
            assert_eq!(
                Interpreter::run_batch(&mut expected, &mut expected_bus, 0, 1).stop,
                BatchStop::Budget
            );
            let frame = invoke(&code, &mut actual, &mut actual_bus, 1);
            assert_eq!((frame.exit_kind, frame.completed), (0, 1));
            same(&actual, &expected);
            assert_eq!(actual_bus.0, expected_bus.0);
            let at = actual.regs[4] as usize + 0x2000;
            assert_eq!(&actual_bus.0[at..at + 2], &selector.to_le_bytes());
            if !prefix {
                assert_eq!(&actual_bus.0[at + 2..at + 4], &[0xa5; 2]);
            }
        }
    }
}
