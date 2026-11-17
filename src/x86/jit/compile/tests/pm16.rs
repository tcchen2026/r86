//! PM16 effects and precise boundaries, with the shared interpreter as reference.
use super::mmx::{Memory, same};
use super::*;
use crate::x86::cpu::{CR0_PG, SegmentCache, SegmentReg};
use crate::x86::interp::{BatchStepStop, StepError};

fn setup() -> (Cpu, Memory) {
    let mut cpu = initial(29);
    for (seg, base, selector, flags) in [
        (SegmentReg::Cs, 0x10000, 0x0b, 0xfb),
        (SegmentReg::Ds, 0x30000, 0x13, 0xf3),
        (SegmentReg::Es, 0x50000, 0x1b, 0xf3),
        (SegmentReg::Ss, 0x70000, 0x23, 0xf3),
        (SegmentReg::Fs, 0x90000, 0x2b, 0xf3),
        (SegmentReg::Gs, 0xb0000, 0x33, 0xf3),
    ] {
        cpu.set_seg_cache(seg, SegmentCache::new(selector, base, 0xffff, flags));
    }
    cpu.regs = [
        0xdead_5678,
        0xbeef_0002,
        0x1234_55aa,
        0xabcd_0120,
        0xa5a5_0800,
        0xface_0900,
        0xbeef_0030,
        0xcafe_0040,
    ];
    cpu.replace_eflags(0x3203);
    let memory = Memory(
        (0..0x100000)
            .map(|i| ((i ^ (i >> 8)) & 255) as u8)
            .collect(),
    );
    (cpu, memory)
}

fn invoke_pm16(code: &Fixture, cpu: &mut Cpu, memory: &mut Memory, count: u64) -> Frame {
    // CALL's bounded native return stack belongs to this invocation.
    let mut returns = Box::new(crate::x86::jit::links::ReturnStack::new());
    let mut frame = Frame::from_cpu(cpu, count);
    frame.call_base = &mut *returns;
    let mut view = soft::View::new(cpu, true);
    view.bus = (memory as *mut Memory).cast();
    view.refill = super::mmx::refill as *const () as usize;
    frame.memory = (&mut view as *mut soft::View).cast();
    code.invoke(&mut frame);
    frame.commit(cpu);
    frame.memory = core::ptr::null_mut();
    frame.call_base = core::ptr::null_mut();
    frame
}

fn compare(bytes: &[u8], configure: impl FnOnce(&mut Cpu, &mut Memory), native: bool) -> Frame {
    check(bytes, configure, native).0
}

fn check(
    bytes: &[u8],
    configure: impl FnOnce(&mut Cpu, &mut Memory),
    native: bool,
) -> (Frame, BatchStop) {
    check_mode(bytes, configure, native, true)
}

fn check_mode(
    bytes: &[u8],
    configure: impl FnOnce(&mut Cpu, &mut Memory),
    native: bool,
    code16: bool,
) -> (Frame, BatchStop) {
    let (mut actual, mut actual_memory) = setup();
    if !code16 {
        for seg in [SegmentReg::Cs, SegmentReg::Ss] {
            let mut cache = actual.seg(seg);
            cache.flags |= crate::x86::cpu::SEGMENT_FLAG_DEFAULT_BIG;
            if seg == SegmentReg::Cs {
                cache.limit = u32::MAX;
            }
            actual.set_seg_cache(seg, cache);
        }
        actual.regs[4] = 0x800;
        actual.regs[5] = 0x900;
    }
    configure(&mut actual, &mut actual_memory);
    let eip = actual.eip;
    let linear = actual.seg(SegmentReg::Cs).base.wrapping_add(eip) as usize;
    actual_memory.0[linear..linear + bytes.len()].copy_from_slice(bytes);
    let mut expected = actual.clone();
    // Cached pointers must still refer to the actual owner. Invalidate after
    // cloning before the reference bus gets its separate RAM allocation.
    expected.invalidate_translation_context();
    let mut expected_memory = Memory(actual_memory.0.clone());
    let decoded = if code16 {
        decode::block_code16(bytes, eip, 32)
    } else {
        decode::block_extended(bytes, eip, 32)
    };
    assert_eq!(decoded.byte_len, bytes.len(), "decode {bytes:02x?}");
    let count = decoded.instructions.len() as u64;
    let fixture = Fixture::with_facts(&decoded, soft::View::block_facts(&actual));
    let reference = Interpreter::run_batch(&mut expected, &mut expected_memory, 0, count);
    let before_native = actual_memory.0.clone();
    let frame = invoke_pm16(&fixture, &mut actual, &mut actual_memory, count);
    if frame.completed == 0 {
        assert!(
            actual_memory.0 == before_native,
            "native wrote before a zero-step exit {bytes:02x?}"
        );
    }
    if native {
        assert_eq!(
            frame.completed, count,
            "must execute natively {bytes:02x?}, exit={}",
            frame.exit_kind
        );
    }
    let stop = if frame.completed < count {
        Interpreter::run_batch(
            &mut actual,
            &mut actual_memory,
            frame.completed,
            count - frame.completed,
        )
        .stop
    } else {
        BatchStop::Budget
    };
    assert_eq!(stop, reference.stop, "stop {bytes:02x?}");
    same(&actual, &expected);
    for seg in 0..6 {
        let seg = SegmentReg::from_modrm_reg(seg);
        assert_eq!(actual.seg(seg), expected.seg(seg));
    }
    assert_eq!(
        actual_memory
            .0
            .iter()
            .zip(&expected_memory.0)
            .position(|(a, b)| a != b),
        None,
        "RAM {bytes:02x?}"
    );
    (frame, stop)
}

#[test]
fn native_pm32_nonflat_segments_and_esp_stack_match_interpreter() {
    for (prefix, segment) in [
        (0x26, SegmentReg::Es),
        (0x36, SegmentReg::Ss),
        (0x3e, SegmentReg::Ds),
    ] {
        for base in [0, 0x30000] {
            for opcode in [0x8b, 0x89] {
                for offset in [0x420u32, 0x4ff] {
                    let mut bytes = vec![prefix, opcode, 0x05];
                    bytes.extend(offset.to_le_bytes());
                    check_mode(
                        &bytes,
                        |cpu, _| {
                            let mut cache = cpu.seg(segment);
                            cache.base = base;
                            cache.limit = 0x4ff;
                            cpu.set_seg_cache(segment, cache);
                        },
                        offset == 0x420,
                        false,
                    );
                }
            }
        }
    }
    // Repeated exact operands must not reuse an offset-only flat bias.
    check_mode(
        &[
            0x8b, 0x05, 0x20, 4, 0, 0, 0x03, 0x05, 0x20, 4, 0, 0, 0x89, 0x05, 0x20, 4, 0, 0,
        ],
        |_, _| {},
        true,
        false,
    );
    for bytes in [
        &[0x50, 0x5b][..],
        &[0x66, 0x50, 0x66, 0x5b],
        &[0x60],
        &[0x61],
        &[0xc9],
        &[0x66, 0xc9],
        &[0x9c, 0x5b],
        &[0xe8, 0x10, 0, 0, 0],
    ] {
        check_mode(bytes, |_, _| {}, true, false);
    }
    for flags in [0xc0f1, 0xc0f7, 0] {
        // Read-only, expand-down, null.
        let (frame, _) = check_mode(
            &[0x89, 0x05, 0x20, 4, 0, 0],
            |cpu, _| {
                let mut ds = cpu.seg(SegmentReg::Ds);
                ds.flags = flags;
                if flags == 0 {
                    ds.selector = 0;
                }
                cpu.set_seg_cache(SegmentReg::Ds, ds);
            },
            false,
            false,
        );
        assert_eq!(frame.completed, 0);
    }
    let (frame, stop) = check_mode(
        &[0x50],
        |cpu, _| {
            let mut ss = cpu.seg(SegmentReg::Ss);
            ss.limit = 0x801;
            cpu.set_seg_cache(SegmentReg::Ss, ss);
            cpu.regs[4] = 0x804;
        },
        false,
        false,
    );
    assert_eq!(frame.completed, 0);
    fault(stop, true);
    let (frame, _) = check_mode(
        &[0x50],
        |cpu, _| {
            let mut ss = cpu.seg(SegmentReg::Ss);
            ss.flags &= !crate::x86::cpu::SEGMENT_FLAG_DEFAULT_BIG;
            cpu.set_seg_cache(SegmentReg::Ss, ss);
            cpu.regs[4] |= 0xdead_0000;
        },
        false,
        false,
    );
    assert_eq!(frame.completed, 0); // PM32+SS.B=0 stack remains interpreted.
}

fn fault(stop: BatchStop, stack: bool) {
    let expected = if stack {
        StepError::StackFault {
            eip: 0x100,
            code: 0,
        }
    } else {
        StepError::GeneralProtection {
            eip: 0x100,
            code: 0,
        }
    };
    assert!(
        matches!(stop, BatchStop::Step { reason: BatchStepStop::Error(error), .. } if error == expected),
        "{stop:?}"
    );
}

#[test]
fn native_pm16_all_modrm_forms_displacements_and_wrap() {
    for opcode in [0x8b, 0x89] {
        for mode in 0..3 {
            for rm in 0..8 {
                let mut bytes = vec![opcode, (mode << 6) | rm];
                if mode == 0 && rm == 6 {
                    bytes.extend(0x420u16.to_le_bytes());
                }
                if mode == 1 {
                    bytes.push(0x80);
                }
                if mode == 2 {
                    bytes.extend(0xfff0u16.to_le_bytes());
                }
                compare(
                    &bytes,
                    |cpu, _| {
                        cpu.regs[3] = 0xdead_fff0;
                    },
                    true,
                );
            }
        }
    }
    for bytes in [&[0x8d, 0x04][..], &[0x66, 0x8d, 0x04], &[0x8d, 0x00]] {
        compare(
            bytes,
            |cpu, _| {
                cpu.regs[6] = 0xface_ffff;
                cpu.regs[3] = 4;
            },
            true,
        );
    }
}

#[test]
fn native_pm16_overrides_moffs_and_register_widths() {
    for bytes in [
        &[0xa1, 0x20, 0x04][..],
        &[0x66, 0xa1, 0x20, 0x04],
        &[0xa3, 0x20, 0x04],
        &[0x66, 0xa3, 0x20, 0x04],
        &[0x67, 0x8b, 0x44, 0x73, 0x08],
        &[0x66, 0x67, 0x8b, 0x44, 0x73, 0x08],
        &[0x67, 0xa1, 0x20, 0x04, 0, 0],
        &[0x66, 0x66, 0x67, 0x67, 0xa1, 0x20, 0x04, 0, 0],
        &[0x11, 0xd8],
        &[0x19, 0xd8],
        &[0xd0, 0xd0],
        &[0xd3, 0xc8],
        &[0x86, 0xe3],
        &[0x8c, 0xd8],
        &[0x66, 0x8c, 0xd8],
    ] {
        compare(
            bytes,
            |cpu, _| {
                cpu.regs[3] = 0x400;
                cpu.regs[6] = 3;
            },
            true,
        );
    }
    for prefix in [0x26, 0x2e, 0x36, 0x3e, 0x64, 0x65] {
        compare(&[prefix, 0x8b, 0x06, 0x20, 0x04], |_, _| {}, true);
    }
}

#[test]
fn native_pm16_stack_width_wrap_and_upper_halves() {
    for bytes in [
        &[0x50][..],
        &[0x54],
        &[0x66, 0x50],
        &[0x66, 0x54],
        &[0x58],
        &[0x5c],
        &[0x66, 0x58],
        &[0x66, 0x5c],
        &[0x1e],
        &[0x66, 0x1e],
        &[0x9c],
        &[0x66, 0x9c],
        &[0xc9],
        &[0x66, 0xc9],
        &[0x60],
        &[0x66, 0x60],
        &[0x61],
        &[0x66, 0x61],
        &[0xff, 0x36, 0x20, 0x04],
    ] {
        compare(bytes, |_, _| {}, true);
    }
    for (bytes, sp) in [
        (&[0x50][..], 0),
        (&[0x66, 0x50][..], 0),
        (&[0x58][..], 0xfffe),
        (&[0x66, 0x58][..], 0xfffc),
        (&[0x60][..], 0),
    ] {
        compare(
            bytes,
            |cpu, _| {
                cpu.regs[4] = 0xa5a5_0000 | sp;
            },
            true,
        );
    }
    for bytes in [&[0x9d][..], &[0x66, 0x9d]] {
        compare(
            bytes,
            |_, mem| {
                mem.word(0x70800, 0x3003);
            },
            true,
        );
    }
}

#[test]
fn native_pm16_segment_limits_permissions_and_restartable_stack_faults() {
    for (prefix, seg) in [
        (0x3e, SegmentReg::Ds),
        (0x26, SegmentReg::Es),
        (0x36, SegmentReg::Ss),
    ] {
        let (frame, stop) = check(&[prefix, 0x8b, 0x06, 0xff, 0xff], |_, _| {}, false);
        assert_eq!(frame.completed, 0);
        fault(stop, seg == SegmentReg::Ss);
        // Segment #GP/#SS precedes a missing page; CR2 remains unchanged.
        compare(
            &[prefix, 0x8b, 0x06, 0x20, 0x04],
            |cpu, mem| {
                let mut cache = cpu.seg(seg);
                cache.limit = 0x41f;
                cpu.set_seg_cache(seg, cache);
                cpu.cr0 |= CR0_PG;
                cpu.cr3 = 0xc0000;
                cpu.cr2 = 0xdead_beef;
                mem.0[0xc0000..0xc2000].fill(0);
                mem.word(0xc0000, 0xc1067);
                mem.word(0xc1040, 0x10067); // Only CS is present, with A/D already set.
            },
            false,
        );
    }
    for bytes in [&[0x50][..], &[0x66, 0x50], &[0x58], &[0x66, 0x58]] {
        let push = bytes.last() == Some(&0x50);
        let frame = compare(
            bytes,
            |cpu, _| {
                cpu.regs[4] = 0xdead_0000 | if push { 1 } else { 0xffff };
            },
            false,
        );
        assert_eq!(frame.completed, 0);
    }
    // A larger CS used to compile the body does not authorize a later entry
    // after its limit shrinks. The body check is independent of the link tag.
    let frame = compare(
        &[0xb8, 0x34, 0x12],
        |cpu, _| {
            let mut cs = cpu.seg(SegmentReg::Cs);
            cs.limit = cpu.eip;
            cpu.set_seg_cache(SegmentReg::Cs, cs);
        },
        false,
    );
    assert_eq!((frame.completed, frame.exit_kind), (0, 7));
    for (seg, prefix, flags) in [
        (SegmentReg::Cs, 0x2e, 0xf9), // execute-only code
        (SegmentReg::Ds, 0x3e, 0xf5), // empty expand-down range
    ] {
        let frame = compare(
            &[prefix, 0x8b, 0x06, 0x20, 0x04],
            |cpu, _| {
                let mut cache = cpu.seg(seg);
                cache.flags = flags;
                cpu.set_seg_cache(seg, cache);
            },
            false,
        );
        assert_eq!(frame.completed, 0);
    }
    let frame = compare(&[0x2e, 0x89, 0x06, 0x20, 0x04], |_, _| {}, false);
    assert_eq!(frame.completed, 0); // readable CS is never writable
    for null in [false, true] {
        let frame = compare(
            &[0x89, 0x06, 0x20, 0x04],
            |cpu, _| {
                let mut ds = cpu.seg(SegmentReg::Ds);
                if null {
                    ds.selector = 0;
                } else {
                    ds.flags &= !2;
                }
                cpu.set_seg_cache(SegmentReg::Ds, ds);
            },
            false,
        );
        assert_eq!(frame.completed, 0);
    }
    // A memory PUSH must try its explicit operand before the stack access.
    compare(
        &[0xff, 0x36, 0x20, 0x04],
        |cpu, _| {
            for seg in [SegmentReg::Ds, SegmentReg::Ss] {
                let mut cache = cpu.seg(seg);
                cache.limit = 0;
                cpu.set_seg_cache(seg, cache);
            }
        },
        false,
    );
}

#[test]
fn native_pm16_near_transfers_and_address_size_counts() {
    for bytes in [
        &[0xe8, 0x10, 0][..],
        &[0x66, 0xe8, 0x10, 0, 0, 0],
        &[0xff, 0xd0],
        &[0x66, 0xff, 0xd0],
        &[0xff, 0x16, 0x20, 0x04],
        &[0x66, 0xff, 0x16, 0x20, 0x04],
        &[0xff, 0xe0],
        &[0x66, 0xff, 0xe0],
        &[0xc3],
        &[0x66, 0xc3],
        &[0xc2, 0xff, 0xff],
        &[0x66, 0xc2, 0xff, 0xff],
        &[0xeb, 0xfe],
        &[0xe9, 0x10, 0],
        &[0x66, 0xe9, 0x10, 0, 0, 0],
    ] {
        compare(
            bytes,
            |cpu, mem| {
                cpu.regs[0] = 0x140;
                mem.word(0x30420, 0x140);
                mem.word(0x70800, 0x140);
            },
            true,
        );
    }
    for opcode in 0xe0..=0xe3 {
        for prefixes in [&[][..], &[0x66], &[0x67], &[0x66, 0x67]] {
            let mut bytes = prefixes.to_vec();
            bytes.extend([opcode, 0xfe]);
            for count in [0, 1, 2, 0xffff] {
                for zero in [false, true] {
                    compare(
                        &bytes,
                        |cpu, _| {
                            cpu.regs[1] = 0xdead_0000 | count;
                            cpu.replace_eflags(0x3203 | if zero { 0x40 } else { 0 });
                        },
                        true,
                    );
                }
            }
        }
    }
    compare(
        &[0xeb, 0x7f],
        |cpu, _| {
            cpu.eip = 0xff80;
        },
        true,
    );
}

#[test]
fn native_pm16_target_limit_faults_precede_stack_and_counter_commits() {
    for bytes in [
        &[0xeb, 0x7f][..],
        &[0xe8, 0x7f, 0],
        &[0xe2, 0x7f],
        &[0xe3, 0x7f],
    ] {
        let (frame, stop) = check(
            bytes,
            |cpu, _| {
                let mut cs = cpu.seg(SegmentReg::Cs);
                cs.limit = 0x110;
                cpu.set_seg_cache(SegmentReg::Cs, cs);
                cpu.regs[1] = if bytes[0] == 0xe3 {
                    0xdead_0000
                } else {
                    0xdead_0002
                };
            },
            false,
        );
        assert_eq!((frame.completed, frame.exit_kind), (0, 7));
        fault(stop, false);
    }
    for taken in [false, true] {
        let (frame, stop) = check(
            &[0x74, 0x7f],
            |cpu, _| {
                let mut cs = cpu.seg(SegmentReg::Cs);
                cs.limit = 0x110;
                cpu.set_seg_cache(SegmentReg::Cs, cs);
                cpu.replace_eflags(0x3203 | if taken { 0x40 } else { 0 });
            },
            false,
        );
        assert_eq!((frame.completed, frame.exit_kind), (0, 7));
        if taken {
            fault(stop, false);
        } else {
            assert_eq!(stop, BatchStop::Budget);
        }
    }
    for bytes in [
        &[0xc3][..],
        &[0xc2, 0x10, 0],
        &[0x66, 0xc3],
        &[0xff, 0xd0],
        &[0xff, 0xe0],
        &[0x66, 0xff, 0xd0],
        &[0xff, 0x16, 0x20, 0x04],
    ] {
        let (frame, stop) = check(
            bytes,
            |cpu, mem| {
                let mut cs = cpu.seg(SegmentReg::Cs);
                cs.limit = 0x110;
                cpu.set_seg_cache(SegmentReg::Cs, cs);
                cpu.regs[0] = 0x140;
                mem.word(0x70800, 0x140);
                mem.word(0x30420, 0x140);
            },
            false,
        );
        assert_eq!((frame.completed, frame.exit_kind), (0, 7));
        assert_eq!(frame.regs[4], 0xa5a5_0800);
        fault(stop, false);
    }
    // The indirect target check must precede an invalid CALL stack as well.
    let (_, stop) = check(
        &[0xff, 0xd0],
        |cpu, _| {
            let mut cs = cpu.seg(SegmentReg::Cs);
            cs.limit = 0x110;
            cpu.set_seg_cache(SegmentReg::Cs, cs);
            let mut ss = cpu.seg(SegmentReg::Ss);
            ss.limit = 0;
            cpu.set_seg_cache(SegmentReg::Ss, ss);
            cpu.regs[0] = 0x140;
        },
        false,
    );
    fault(stop, false);
}

#[test]
fn native_pm16_decode_stops_at_deferred_forms_and_ip_boundary() {
    for bytes in [
        &[0x9a, 0, 0, 0, 0][..],
        &[0xcb],
        &[0xca, 0, 0],
        &[0xea, 0, 0, 0, 0],
        &[0xc4, 0x06, 0, 4],
        &[0xc5, 0x06, 0, 4],
        &[0x8e, 0xd0],
        &[0x17],
        &[0xac],
        &[0xad],
        &[0xaa],
        &[0xab],
        &[0xa4],
        &[0xa5],
        &[0xf3, 0xa4],
    ] {
        assert!(
            decode::block_code16(bytes, 0x100, 32)
                .instructions
                .is_empty(),
            "{bytes:02x?}"
        );
    }
    assert!(
        decode::block_code16(&[0xb8, 1, 2], 0xffff, 1)
            .instructions
            .is_empty()
    );
    assert_eq!(decode::block_code16(&[0x90, 0x90], 0xffff, 2).byte_len, 1);
    assert!(
        decode::block_code16(&[0x90], 0x10000, 1)
            .instructions
            .is_empty()
    );
    compare(&[0x90], |cpu, _| cpu.eip = 0xffff, true);
}

#[test]
fn native_pm16_segment_loads_keep_the_chain_and_refresh_based_access() {
    use crate::x86::jit::{links, runtime};
    use crate::x86::{LinearAddr, mmu};
    for seg in [SegmentReg::Ds, SegmentReg::Es] {
        for base in [0u32, 0x50000] {
            let make = || {
                let (mut cpu, mut memory) = setup();
                cpu.gdtr.base = 0x2000;
                cpu.gdtr.limit = 0xff;
                cpu.regs[0] = 0x3b;
                // Accessed ring-3 RW data, first becoming flat and then based.
                let (limit, flags) = if base == 0 {
                    (0xfffff, 0xc0f3)
                } else {
                    (0xffff, 0x00f3)
                };
                let low = (base << 16) | (limit & 0xffff);
                let high = (base & 0xff000000)
                    | ((base >> 16) & 255)
                    | ((flags & 255) << 8)
                    | (limit & 0xf0000)
                    | ((flags & 0xf000) << 8);
                memory.word(0x2038, low);
                memory.word(0x203c, high);
                let load = [0x8e, 0xc0 | (seg as u8) << 3];
                let read = if seg == SegmentReg::Es {
                    vec![0x26, 0xa1, 0x20, 1]
                } else {
                    vec![0xa1, 0x20, 1]
                };
                memory.0[0x10100..0x10102].copy_from_slice(&load);
                memory.0[0x10102..0x10102 + read.len()].copy_from_slice(&read);
                memory.0[base as usize + 0x120..base as usize + 0x122]
                    .copy_from_slice(&0xbeefu16.to_le_bytes());
                mmu::read_linear_u32_at_priv(
                    &mut cpu,
                    &mut memory,
                    LinearAddr(0x2038),
                    mmu::AccessKind::Read,
                    mmu::AccessPriv::ImplicitSupervisor,
                )
                .unwrap();
                (cpu, memory, load, read)
            };
            let (mut cpu, mut memory, load, read) = make();
            let (mut expected, mut expected_memory, _, _) = make();
            let reference = Interpreter::run_batch(&mut expected, &mut expected_memory, 0, 2);
            assert_eq!(reference.stop, BatchStop::Budget);
            let mut arena = CodeArena::with_limits(64 * 1024, 1).unwrap();
            let stubs = entry().unwrap();
            let entry = arena.publish(&stubs.words).unwrap();
            let dispatch = arena.address(entry, stubs.dispatch_offset).unwrap();
            let finish = arena.address(entry, stubs.return_offset).unwrap();
            let recheck = arena.address(entry, stubs.recheck_offset).unwrap();
            let context = (1u64 << 32) | 1;
            let mut links = Box::new([links::Link::EMPTY; links::SLOTS]);
            let table = arena.allocate_entries();
            let entries = arena.entries_pointer(table);
            for (eip, bytes) in [(0x100, &load[..]), (0x102, &read[..])] {
                let decoded = decode::block_code16(bytes, eip, 1);
                let mut code = block(&decoded, None, soft::View::block_facts(&cpu)).unwrap();
                let handle = arena.publish_block(&mut code).unwrap();
                let body = arena.address(handle, 0).unwrap();
                arena.entries_mut(table)[eip as usize] =
                    (body as isize - entries as isize) as i32 as u32;
            }
            links[links::slot(0x10100)] = links::Link {
                tag: context | 0x10000,
                entries,
            };
            let mut frame = Frame::from_cpu(&cpu, 2);
            let mut view = soft::View::new(&mut cpu, true);
            view.bus = (&mut memory as *mut Memory).cast();
            view.load_segment = runtime::load_cached_segment::<Memory> as *const () as usize;
            view.refill = super::mmx::refill as *const () as usize;
            frame.memory = (&mut view as *mut soft::View).cast();
            frame.native_entry = dispatch;
            frame.dispatch_entry = dispatch;
            frame.return_entry = finish;
            frame.recheck_entry = recheck;
            frame.links = links.as_mut_ptr();
            frame.link_context = context;
            // SAFETY: the two bodies, table, View and owners stay live. This
            // is the production helper specialized only for the test Bus.
            unsafe {
                arena.invoke(entry, &mut frame).unwrap();
            }
            frame.commit(&mut cpu);
            assert_eq!((frame.completed, frame.loaded_segments), (2, 1));
            same(&cpu, &expected);
            assert_eq!(cpu.seg(seg), expected.seg(seg));
            assert_eq!(cpu.regs[0] & 0xffff, 0xbeef);
            assert_eq!(soft::View::block_facts(&cpu), soft::CODE16);
            assert_eq!(
                memory
                    .0
                    .iter()
                    .zip(&expected_memory.0)
                    .position(|(a, b)| a != b),
                None
            );
        }
    }
}

#[test]
fn native_pm32_expand_down_access_bounds_match_interpreter() {
    for (prefix, segment) in [
        (0x26, SegmentReg::Es),
        (0x36, SegmentReg::Ss),
        (0x3e, SegmentReg::Ds),
    ] {
        for big in [false, true] {
            let upper = if big { u32::MAX } else { 0xffff };
            for width in [2, 4] {
                for opcode in [0x8b, 0x89, 0x01] {
                    for (offset, valid) in [
                        (0x400, false),
                        (0x401, true),
                        (upper - width + 1, true),
                        (upper - width + 2, false),
                    ] {
                        let mut bytes = vec![prefix];
                        if width == 2 {
                            bytes.push(0x66);
                        }
                        bytes.extend([opcode, 5]);
                        bytes.extend(offset.to_le_bytes());
                        let (frame, stop) = check_mode(
                            &bytes,
                            |cpu, _| {
                                let mut cache = cpu.seg(segment);
                                cache.flags = 0xf7 | if big { 0x4000 } else { 0 };
                                cache.limit = 0x400;
                                // Valid upper-end operands fit the small test RAM only
                                // after wrapping base addition. Bounds precede that add.
                                cache.base = 0x40000u32.wrapping_sub(offset);
                                cpu.set_seg_cache(segment, cache);
                            },
                            valid,
                            false,
                        );
                        assert_eq!(frame.completed, u64::from(valid));
                        if !valid {
                            fault(stop, segment == SegmentReg::Ss);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn native_pm32_expand_down_push_checks_before_stack_effects() {
    for width in [2, 4] {
        for start in [0x3ffu32, 0x400, 0x401] {
            let bytes = if width == 2 {
                &[0x66, 0x50][..]
            } else {
                &[0x50][..]
            };
            let valid = start > 0x400;
            let (frame, stop) = check_mode(
                bytes,
                |cpu, _| {
                    let mut ss = cpu.seg(SegmentReg::Ss);
                    ss.flags = 0x40f7;
                    ss.limit = 0x400;
                    cpu.set_seg_cache(SegmentReg::Ss, ss);
                    cpu.regs[4] = start + width;
                },
                valid,
                false,
            );
            assert_eq!(frame.completed, u64::from(valid));
            assert_eq!(frame.regs[4], if valid { start } else { start + width });
            if !valid {
                fault(stop, true);
            }
        }
    }
}

#[test]
fn native_expand_down_permissions_and_empty_ranges() {
    // Read-only expand-down is readable in PM32; writes and RMW still leave.
    for opcode in [0x8b, 0x89, 0x01] {
        let (frame, stop) = check_mode(
            &[opcode, 5, 0x20, 4, 0, 0],
            |cpu, _| {
                let mut ds = cpu.seg(SegmentReg::Ds);
                ds.flags = 0x40f5;
                ds.limit = 0x400;
                cpu.set_seg_cache(SegmentReg::Ds, ds);
            },
            opcode == 0x8b,
            false,
        );
        assert_eq!(frame.completed, u64::from(opcode == 0x8b));
        if opcode != 0x8b {
            fault(stop, false);
        }
    }
    for big in [false, true] {
        let (frame, stop) = check_mode(
            &[0xa1, 0x20, 4],
            |cpu, _| {
                let mut ds = cpu.seg(SegmentReg::Ds);
                ds.flags = 0xf7 | if big { 0x4000 } else { 0 };
                ds.limit = 0x400;
                cpu.set_seg_cache(SegmentReg::Ds, ds);
            },
            true,
            true,
        );
        assert_eq!(frame.completed, 1);
        assert_eq!(stop, BatchStop::Budget);
        // A limit at the upper end describes an empty range, not flat memory.
        let (frame, stop) = check_mode(
            &[0xa1, 0x20, 4, 0, 0],
            |cpu, _| {
                let mut ds = cpu.seg(SegmentReg::Ds);
                ds.flags = 0xf7 | if big { 0x4000 } else { 0 };
                ds.limit = if big { u32::MAX } else { 0xffff };
                ds.base = 0;
                cpu.set_seg_cache(SegmentReg::Ds, ds);
            },
            false,
            false,
        );
        assert_eq!(frame.completed, 0);
        fault(stop, false);
    }
}

#[test]
fn native_pm32_segment_kind_load_continues_with_refreshed_bounds() {
    use crate::x86::jit::runtime;
    use crate::x86::{LinearAddr, mmu};
    // SS loads already leave for their interrupt shadow; the native helper
    // only continues ES/DS loads.
    for seg in [SegmentReg::Es, SegmentReg::Ds] {
        for old_down in [false, true] {
            for new_down in [false, true] {
                let (mut cpu, mut memory) = setup();
                let mut cs = cpu.seg(SegmentReg::Cs);
                cs.flags |= 0x4000;
                cs.limit = u32::MAX;
                cpu.set_seg_cache(SegmentReg::Cs, cs);
                let mut cache = cpu.seg(seg);
                cache.flags = 0x40f3 | if old_down { 4 } else { 0 };
                cache.limit = 0x3ff;
                cpu.set_seg_cache(seg, cache);
                let old_facts = soft::View::block_facts(&cpu);
                cpu.gdtr.base = 0x2000;
                cpu.gdtr.limit = 0xff;
                let base = 0x50000;
                let flags = 0x40f3 | if new_down { 4 } else { 0 };
                memory.word(0x2038, (base << 16) | 0x4ff);
                memory.word(
                    0x203c,
                    ((base >> 16) & 255) | ((flags & 255) << 8) | ((flags & 0xf000) << 8),
                );
                mmu::read_linear_u32_at_priv(
                    &mut cpu,
                    &mut memory,
                    LinearAddr(0x2038),
                    mmu::AccessKind::Read,
                    mmu::AccessPriv::ImplicitSupervisor,
                )
                .unwrap();
                let mut view = soft::View::new(&mut cpu, true);
                view.bus = (&mut memory as *mut Memory).cast();
                let mut frame = Frame::from_cpu(&cpu, 1);
                frame.memory = (&mut view as *mut soft::View).cast();
                // SAFETY: the production segment helper owns live test CPU,
                // view and bus; its descriptor is resident and already accessed.
                let result =
                    unsafe { runtime::load_cached_segment::<Memory>(&mut frame, seg as u32, 0x3b) };
                assert_eq!(result, 1);
                assert_eq!(old_facts, soft::View::block_facts(&cpu));
                assert_eq!(
                    cpu.seg(seg),
                    SegmentCache::new(0x3b, base, 0x4ff, flags as u16)
                );
                // The same compiled facts use the new bounds for either kind.
                let offset: u32 = if new_down { 0x500 } else { 0x4fc };
                let mut bytes = vec![[0x26, 0x2e, 0x36, 0x3e][seg as usize], 0xa1];
                bytes.extend(offset.to_le_bytes());
                let decoded = decode::block_extended(&bytes, cpu.eip, 1);
                let fixture = Fixture::with_facts(&decoded, old_facts);
                let expected = u32::from_le_bytes(
                    memory.0[(base + offset) as usize..(base + offset + 4) as usize]
                        .try_into()
                        .unwrap(),
                );
                view.refill = super::mmx::refill as *const () as usize;
                fixture.invoke(&mut frame);
                frame.commit(&mut cpu);
                assert_eq!(frame.completed, 1);
                assert_eq!(cpu.regs[0], expected);
            }
        }
    }
}

fn based_access_bytes(
    code16: bool,
    segment: SegmentReg,
    width: u32,
    write: bool,
    offset: u32,
) -> Vec<u8> {
    let mut bytes = vec![[0x26, 0x2e, 0x36, 0x3e, 0x64, 0x65][segment as usize]];
    if code16 {
        bytes.push(0x67); // Also reach B=1 bounds above the 16-bit address space.
    }
    if width == 8 {
        bytes.extend([0x0f, 0xc7, 0x0d]); // CMPXCHG8B [disp32]
    } else {
        if width > 1 && (width == 2) != code16 {
            bytes.push(0x66);
        }
        bytes.extend([if write { 0x88 } else { 0x8a } + u8::from(width > 1), 5]);
    }
    bytes.extend(offset.to_le_bytes());
    bytes
}

#[test]
fn native_pm16_and_fs_gs_expand_down_accesses_check_both_bounds() {
    for code16 in [false, true] {
        for segment in [SegmentReg::Ds, SegmentReg::Fs, SegmentReg::Gs] {
            if !code16 && segment == SegmentReg::Ds {
                continue; // Existing PM32 ES/SS/DS matrix covers this path.
            }
            for big in [false, true] {
                let upper = if big { u32::MAX } else { 0xffff };
                for width in [1, 2, 4, 8] {
                    let mut cases = vec![(0x400, false), (0x401, true), (upper - width + 1, true)];
                    if let Some(past) = (upper - width + 1).checked_add(1) {
                        cases.push((past, false));
                    }
                    for (offset, valid) in cases {
                        for write in [false, true] {
                            if width == 8 && !write {
                                continue;
                            }
                            let bytes = based_access_bytes(code16, segment, width, write, offset);
                            let (frame, stop) = check_mode(
                                &bytes,
                                |cpu, memory| {
                                    let mut cache = cpu.seg(segment);
                                    cache.flags = 0xf7 | if big { 0x4000 } else { 0 };
                                    cache.limit = 0x400;
                                    cache.base = 0x40000u32.wrapping_sub(offset);
                                    cpu.set_seg_cache(segment, cache);
                                    // Force the wide compare to store a different value.
                                    memory.word(0x40000, cpu.regs[0]);
                                    memory.word(0x40004, cpu.regs[2]);
                                },
                                valid,
                                code16,
                            );
                            assert_eq!(frame.completed, u64::from(valid));
                            if !valid {
                                fault(stop, false);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn native_based_narrow_and_empty_ranges_never_underflow() {
    for code16 in [false, true] {
        for down in [false, true] {
            for length in 1..=4 {
                for width in [1, 2, 4, 8] {
                    let offset = if down { 0x10000 - length } else { 0 };
                    let bytes = based_access_bytes(code16, SegmentReg::Fs, width, true, offset);
                    let valid = width <= length;
                    let (frame, stop) = check_mode(
                        &bytes,
                        |cpu, memory| {
                            let mut cache = cpu.seg(SegmentReg::Fs);
                            cache.flags = 0xf3 | if down { 4 } else { 0 };
                            cache.limit = if down { offset - 1 } else { length - 1 };
                            cache.base = 0x40000u32.wrapping_sub(offset);
                            cpu.set_seg_cache(SegmentReg::Fs, cache);
                            memory.word(0x40000, cpu.regs[0]);
                            memory.word(0x40004, cpu.regs[2]);
                        },
                        valid,
                        code16,
                    );
                    assert_eq!(frame.completed, u64::from(valid));
                    if !valid {
                        fault(stop, false);
                    }
                }
            }
        }
        for segment in [SegmentReg::Ds, SegmentReg::Fs, SegmentReg::Gs] {
            for (big, limit) in [
                (false, 0xffff),
                (false, 0x10000),
                (false, u32::MAX),
                (true, u32::MAX),
            ] {
                let (frame, stop) = check_mode(
                    &based_access_bytes(code16, segment, 8, true, 0),
                    |cpu, _| {
                        let mut cache = cpu.seg(segment);
                        cache.flags = 0xf7 | if big { 0x4000 } else { 0 };
                        cache.limit = limit;
                        cpu.set_seg_cache(segment, cache);
                    },
                    false,
                    code16,
                );
                assert_eq!(frame.completed, 0);
                fault(stop, false);
            }
        }
    }
}

#[test]
fn native_expand_down_pusha_popa_windows_check_before_any_commit() {
    for code16 in [false, true] {
        let upper = if code16 { 0xffff } else { u32::MAX };
        for width in [2, 4] {
            let window = width * 8;
            for push in [false, true] {
                let mut bytes = vec![];
                if (width == 2) != code16 {
                    bytes.push(0x66);
                }
                bytes.push(if push { 0x60 } else { 0x61 });
                let cases = if push {
                    vec![
                        (0x400 + window, false),
                        (0x401 + window, true),
                        (0, true),
                        (1, false),
                    ]
                } else {
                    vec![
                        (0x400, false),
                        (0x401, true),
                        (upper - window + 1, true),
                        (upper - window + 2, false),
                    ]
                };
                for (sp, valid) in cases {
                    let esp = sp | if code16 { 0xa5a5_0000 } else { 0 };
                    let (frame, stop) = check_mode(
                        &bytes,
                        |cpu, _| {
                            let mut ss = cpu.seg(SegmentReg::Ss);
                            ss.flags = 0xf7 | if code16 { 0 } else { 0x4000 };
                            ss.limit = 0x400;
                            cpu.set_seg_cache(SegmentReg::Ss, ss);
                            cpu.regs[4] = esp;
                        },
                        valid,
                        code16,
                    );
                    assert_eq!(frame.completed, u64::from(valid));
                    if !valid {
                        assert_eq!(frame.regs[4], esp);
                        fault(stop, true);
                    }
                }
            }
        }
    }
}
