//! ISA encodings are checked against a separately assembled source fixture.
//! On Linux ARM64, additionally execute the resulting code and check outcomes;
//! matching encoded words alone cannot validate the ABI or publication sequence.

use super::*;

fn fixture() -> Assembler {
    let mut a = Assembler::new();
    a.emit(branch_immediate(4));
    a.emit(branch_immediate(-8));
    a.mov_imm(Reg::new(0), 0x1234_5678, false);
    a.mov_imm(Reg::new(1), 0x1234_5678_9abc_def0, true);
    a.mov_imm(Reg::new(2), 0x1234_0000, false);
    a.mov_imm(Reg::new(3), 0, true);
    a.mov(Reg::new(4), Reg::new(5), false);
    a.mov(Reg::new(6), Reg::new(7), true);
    a.add_sub(Reg::new(8), Reg::new(9), Reg::new(10), false, false, true);
    a.add_sub(Reg::new(11), Reg::new(12), Reg::new(13), true, true, false);
    a.add_shifted(Reg::new(0), Reg::new(1), Reg::new(2), 3, false);
    a.add_shifted(Reg::new(3), Reg::new(4), Reg::new(5), 5, true);
    a.compare_lsr64(Reg::new(12), Reg::new(9), 32);
    a.add_lsr64(Reg::new(0), Reg::new(14), Reg::new(16), 32);
    a.add_lsr64(Reg::new(5), Reg::new(5), Reg::new(5), 0);
    a.add_lsr64(Reg::new(6), ZR, Reg::new(7), 63);
    a.multiply_long(Reg::new(2), Reg::new(0), Reg::new(1), true);
    a.multiply_long(Reg::new(4), Reg::new(3), Reg::new(5), false);
    a.divide64(Reg::new(2), Reg::new(0), Reg::new(1), false);
    a.divide64(Reg::new(4), Reg::new(3), Reg::new(5), true);
    a.multiply_subtract64(Reg::new(6), Reg::new(2), Reg::new(1), Reg::new(0));
    a.reverse_bits32(Reg::new(3), Reg::new(5));
    a.leading_zeros32(Reg::new(4), Reg::new(7));
    a.add_sub_carry(Reg::new(0), Reg::new(1), Reg::new(2), false, false, true);
    a.add_sub_carry(Reg::new(3), Reg::new(4), Reg::new(5), true, false, false);
    a.add_sub_carry(Reg::new(6), Reg::new(7), Reg::new(8), false, true, false);
    a.add_sub_carry(Reg::new(9), Reg::new(10), Reg::new(11), true, true, true);
    a.add_sub_imm(SP, SP, 96, true, true, false);
    a.add_sub_imm(Reg::new(14), Reg::new(15), 4095, false, false, true);
    a.logical(0, Reg::new(0), Reg::new(1), Reg::new(2), false);
    a.logical(1, Reg::new(3), Reg::new(4), Reg::new(5), true);
    a.logical(2, Reg::new(6), Reg::new(7), Reg::new(8), false);
    a.logical(3, Reg::new(9), Reg::new(10), Reg::new(11), true);
    a.xor_bit(Reg::new(15), Reg::new(15), 29, false);
    a.xor_bit(Reg::new(1), Reg::new(2), 0, true);
    a.xor_bit(Reg::new(3), Reg::new(4), 63, true);
    a.and_high_bits(Reg::new(12), Reg::new(13), 12, false);
    a.and_high_bits(Reg::new(14), Reg::new(15), 12, true);
    a.bitfield(Reg::new(12), Reg::new(13), 8, 15, false, false);
    a.bitfield(Reg::new(14), Reg::new(15), 8, 31, false, true);
    a.bitfield(Reg::new(16), Reg::new(17), 59, 58, true, false);
    a.bfi32(Reg::new(0), Reg::new(1), 8, 8);
    a.cset(Reg::new(2), Cond::Cc);
    a.shift_reg(Reg::new(3), Reg::new(4), Reg::new(5), true, 1);
    a.shift_reg(Reg::new(6), Reg::new(7), Reg::new(8), false, 0);
    a.shift_reg(Reg::new(9), Reg::new(10), Reg::new(11), true, 2);
    a.shift_reg(Reg::new(12), Reg::new(13), Reg::new(14), false, 3);
    a.mrs_nzcv(Reg::new(3));
    a.msr_nzcv(Reg::new(4));
    for size in 0..4 {
        a.load_store(true, Reg::new(5), Reg::new(6), 12 << size, size);
        a.load_store(false, Reg::new(7), Reg::new(8), 4095 << size, size);
        a.load_store_offset(true, Reg::new(5), Reg::new(6), Reg::new(13), size);
        a.load_store_offset(false, Reg::new(7), Reg::new(8), Reg::new(13), size);
    }
    a.load_i32_indexed(Reg::new(3), Reg::new(4), Reg::new(0));
    a.pair(false, Reg::new(19), Reg::new(20), SP, 0, true);
    a.pair(true, Reg::new(29), LR, SP, 80, true);
    a.pair(false, Reg::new(9), Reg::new(10), Reg::new(28), -8, false);
    a.pair_writeback(false, Reg::new(2), Reg::new(6), Reg::new(29), 16, false);
    a.pair_writeback(true, Reg::new(0), Reg::new(1), Reg::new(29), -16, true);
    for (offset, size) in [(255, 0), (2, 1), (-256, 2), (6, 3)] {
        a.store_unscaled(Reg::new(7), Reg::new(8), offset, size);
    }
    let back = a.label();
    let forward = a.label();
    a.bind(back);
    a.b_cond(Cond::Eq, forward);
    a.cbz(Reg::new(5), false, false, back);
    a.cbz(Reg::new(6), true, true, forward);
    a.tbz(Reg::new(7), 0, false, back);
    a.tbz(Reg::new(8), 5, true, forward);
    a.tbz(Reg::new(15), 31, false, forward);
    a.tbz(Reg::new(16), 63, true, back);
    a.b(back);
    a.bl(back);
    a.bl(forward);
    a.bind(forward);
    a.br(Reg::new(16));
    a.blr(Reg::new(17));
    a.ret();
    a.ret_to(Reg::new(1));
    a
}

#[test]
fn encodings_match_external_assembler() {
    let expected: Vec<u32> = include_str!("../../../../tests/fixtures/jit_arm64.hex")
        .split_whitespace()
        .map(|word| u32::from_str_radix(word, 16).unwrap())
        .collect();
    assert_eq!(fixture().finish().unwrap(), expected);
}

#[test]
fn branch_retargeting_preserves_operands_and_rejects_out_of_range_targets() {
    assert_eq!(retarget_branch(0x9400_0000, 4), Some(0x9400_0001));
    assert_eq!(retarget_branch(0x1400_0000, -4), Some(0x17ff_ffff));
    for (word, bits, high, low) in [
        (0x5400_000d, 19, 0x547f_ffed, 0x5480_000d), // B.LE.
        (0xb500_0003, 19, 0xb57f_ffe3, 0xb580_0003), // CBNZ X3.
        (0x37f0_0007, 14, 0x37f3_ffe7, 0x37f4_0007), // TBNZ W7,#30.
    ] {
        let bound = 1 << (bits + 1);
        assert_eq!(retarget_branch(word, bound - 4), Some(high));
        assert_eq!(retarget_branch(word, -bound), Some(low));
        assert_eq!(retarget_branch(word, bound), None);
        assert_eq!(retarget_branch(word, -bound - 4), None);
    }
}

#[test]
fn unbound_target_is_a_compile_error() {
    let mut a = Assembler::new();
    let missing = a.label();
    a.b(missing);
    assert_eq!(a.finish(), Err(EmitError::UnboundLabel));
}

#[cfg(a64_jit)]
mod native {
    use super::*;
    use crate::x86::cpu::Cpu;
    use crate::x86::jit::code::{CodeArena, CodeError};
    use crate::x86::jit::frame::{self, BUDGET, FRAME, Frame, guest_reg};
    use core::mem::offset_of;

    fn arithmetic_block(increment: u16) -> Vec<u32> {
        let mut a = Assembler::new();
        frame::emit_entry(&mut a);
        let again = a.label();
        let done = a.label();
        a.cbz(BUDGET, true, false, done);
        a.bind(again);
        a.add_sub_imm(guest_reg(0), guest_reg(0), increment, false, false, false);
        a.logical(2, guest_reg(1), guest_reg(1), guest_reg(0), false);
        a.add_sub_imm(BUDGET, BUDGET, 1, true, true, true);
        a.b_cond(Cond::Ne, again);
        a.bind(done);
        frame::emit_return(&mut a);
        a.finish().unwrap()
    }

    #[test]
    fn native_loop_preserves_registers_and_obeys_budget() {
        let mut arena = CodeArena::new(16 * 1024).unwrap();
        let block = arena.publish(&arithmetic_block(7)).unwrap();
        for budget in [0, 1, 2, 257] {
            let mut cpu = Cpu::new();
            cpu.regs = [u32::MAX - 10, 0xa5a5_1234, 2, 3, 4, 5, 6, 7];
            let mut expected = cpu.regs;
            for _ in 0..budget {
                expected[0] = expected[0].wrapping_add(7);
                expected[1] ^= expected[0];
            }
            let mut frame = Frame::from_cpu(&cpu, budget);
            // SAFETY: locally emitted entry uses only frame storage, resident
            // GPRs and a bounded loop, and preserves the platform calling ABI.
            unsafe { arena.invoke(block, &mut frame).unwrap() };
            assert_eq!(frame.regs, expected);
            assert_eq!(frame.remaining, 0);
        }
    }

    #[test]
    fn native_entry_installs_an_aligned_frame_record() {
        let mut a = Assembler::new();
        frame::emit_entry(&mut a);
        a.add_sub_imm(Reg::new(0), SP, 80, true, false, false);
        a.add_sub(guest_reg(0), Reg::new(29), Reg::new(0), true, true, false);
        a.load_store(true, Reg::new(1), Reg::new(29), 8, 3);
        a.add_sub(guest_reg(1), Reg::new(1), LR, true, true, false);
        a.bitfield(guest_reg(2), Reg::new(0), 0, 3, false, false);
        frame::emit_return(&mut a);
        let mut arena = CodeArena::new(4096).unwrap();
        let block = arena.publish(&a.finish().unwrap()).unwrap();
        let mut frame = Frame::from_cpu(&Cpu::new(), 0);
        // SAFETY: fixture reads only its own entry frame and returns normally.
        unsafe { arena.invoke(block, &mut frame).unwrap() };
        assert_eq!(&frame.regs[..3], &[0, 0, 0]);
    }

    #[test]
    fn native_entry_preserves_host_callee_saved_d_registers() {
        // Entry/return must honor the low-64-bit preservation rule in AAPCS64
        // 6.1.2: https://github.com/ARM-software/abi-aa/blob/main/aapcs64/aapcs64.rst
        let mut a = Assembler::new();
        frame::emit_entry(&mut a);
        frame::emit_return(&mut a);
        let mut arena = CodeArena::new(4096).unwrap();
        let block = arena.publish(&a.finish().unwrap()).unwrap();
        let entry = arena.address(block, 0).unwrap();
        let mut frame = Frame::from_cpu(&Cpu::new(), 10);
        let mut link = crate::x86::jit::links::Link::EMPTY;
        frame.links = &mut link;
        frame.link_context = 0x1234_abcd_0000_0000;
        let first = 0x0123_4567_89ab_cdef_u64;
        let second = 0xfedc_ba98_7654_3210_u64;
        let actual_first: u64;
        let actual_second: u64;
        #[cfg(target_os = "macos")]
        arena.enter_execute();
        // SAFETY: the generated fixture only loads and stores this live frame.
        // Preserve our caller's registers even if a broken JIT fails the check.
        // The temporary stack reservation maintains 16-byte alignment.
        unsafe {
            core::arch::asm!(
                "stp d8, d9, [sp, #-16]!",
                "fmov d8, x1",
                "fmov d9, x2",
                "blr x16",
                "fmov x0, d8",
                "fmov x1, d9",
                "ldp d8, d9, [sp], #16",
                inlateout("x0") &mut frame => actual_first,
                inlateout("x1") first => actual_second,
                in("x2") second,
                in("x16") entry,
                clobber_abi("C"),
            );
        }
        assert_eq!((actual_first, actual_second), (first, second));
    }

    #[test]
    fn native_flags_and_partial_register_write() {
        let mut a = Assembler::new();
        frame::emit_entry(&mut a);
        a.add_sub(guest_reg(0), guest_reg(0), guest_reg(1), false, false, true);
        a.mrs_nzcv(Reg::new(0));
        a.load_store(
            false,
            Reg::new(0),
            FRAME,
            offset_of!(Frame, repeat) as u32,
            2,
        );
        a.bfi32(guest_reg(2), guest_reg(3), 8, 8);
        frame::emit_return(&mut a);
        let mut arena = CodeArena::new(4096).unwrap();
        let block = arena.publish(&a.finish().unwrap()).unwrap();
        for (lhs, rhs, flags) in [
            (u32::MAX, 1, 0x6000_0000),
            (0x7fff_ffff, 1, 0x9000_0000),
            (0x8000_0000, 0x8000_0000, 0x7000_0000),
        ] {
            let mut cpu = Cpu::new();
            cpu.regs = [lhs, rhs, 0x1234_5678, 0xa5, 4, 5, 6, 7];
            let mut frame = Frame::from_cpu(&cpu, 1);
            // SAFETY: this generated fixture obeys the frame ABI and stores
            // only inside the supplied frame; no guest memory is dereferenced.
            unsafe { arena.invoke(block, &mut frame).unwrap() };
            assert_eq!(frame.regs[0], lhs.wrapping_add(rhs));
            assert_eq!(frame.regs[2], 0x1234_a578);
            assert_eq!(frame.repeat.control, flags);
        }
    }

    #[test]
    fn publication_keeps_old_entries_and_reclamation_rejects_old_handles() {
        let words = arithmetic_block(1);
        let mut arena = CodeArena::new(words.len() * 8).unwrap();
        let first = arena.publish(&words).unwrap();
        let second = arena.publish(&arithmetic_block(2)).unwrap();
        assert!(matches!(arena.publish(&words), Err(CodeError::Capacity)));
        let mut frame = Frame::from_cpu(&Cpu::new(), 10);
        // SAFETY: the two fixtures have the same checked frame ABI as above.
        unsafe { arena.invoke(first, &mut frame).unwrap() };
        assert_eq!(frame.regs[0], 10);
        frame.remaining = 10;
        unsafe { arena.invoke(second, &mut frame).unwrap() };
        assert_eq!(frame.regs[0], 30);
        arena.clear();
        let fresh = arena.publish(&arithmetic_block(3)).unwrap();
        assert!(matches!(
            unsafe { arena.invoke(first, &mut frame) },
            Err(CodeError::StaleHandle)
        ));
        frame.remaining = 10;
        unsafe { arena.invoke(fresh, &mut frame).unwrap() };
        assert_eq!(frame.regs[0], 60);
    }
}
