//! Run the same x86 bytes through the interpreter and one native block. The
//! interpreter is the oracle for registers, all EFLAGS bits and EIP; inputs
//! cover carries, overflow, parity, partial registers and untouched flags.
//! Run under `qemu-arm -cpu cortex-a7`; `asm::tests` pins the encodings to
//! Armv8-M Mainline.

use super::arena::Arena;
use super::lower::{self, Frame, STACK_FACT, STACK32_FACT};
use crate::x86::cpu::{SegmentCache, SegmentReg};
use crate::x86::jit::decode;
use crate::x86::{BackingToken, BatchStop, Bus, Cpu, Interpreter, LinearAddr, MemError, PhysAddr};
use alloc::vec::Vec;

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

const ALL_FACTS: u32 = 0b1101 | 0b1101 << 8 | STACK_FACT;
const STATUS: u32 = 0x8d5;

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
        0x1234_5678,
        0x10,
    ];
    let mut random = seed.wrapping_mul(0x9e37_79b9).wrapping_add(17);
    for (index, register) in cpu.regs.iter_mut().enumerate() {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        *register = if seed < 2 * edges.len() as u32 {
            edges[(seed as usize + index * 3) % edges.len()]
        } else {
            random
        };
    }
    // Includes flags these operations never modify, and both incoming carries.
    cpu.replace_eflags(0x2402 | (seed.wrapping_mul(0x155) & STATUS) | (seed & 4) << 8);
    cpu
}

struct Fixture {
    _arena: Arena,
    /// Code addresses carry the Thumb bit.
    entry: usize,
    dispatch: usize,
    block: usize,
    count: u32,
    /// Empty dispatch plane: every chained exit returns to the test.
    table: Vec<[u32; 2]>,
}

impl Fixture {
    fn new(bytes: &[u8]) -> Self {
        Self::at(bytes, 0x100)
    }

    fn at(bytes: &[u8], eip: u32) -> Self {
        Self::with_facts(bytes, eip, ALL_FACTS)
    }

    fn with_facts(bytes: &[u8], eip: u32, facts: u32) -> Self {
        let decoded = decode::block_extended(bytes, eip, 64);
        assert_eq!(decoded.byte_len, bytes.len(), "decode {bytes:02x?}");
        let compiled =
            lower::block(&decoded, facts).unwrap_or_else(|| panic!("native block {bytes:02x?}"));
        assert_eq!(
            compiled.instructions as usize,
            decoded.instructions.len(),
            "partial block {bytes:02x?}"
        );
        Self::publish(compiled)
    }

    fn publish(compiled: lower::Compiled) -> Self {
        let mut arena = Arena::new(1 << 16).unwrap();
        let shared = lower::shared();
        let entry = arena.publish(&shared.code).unwrap();
        let block = arena.publish(&compiled.code).unwrap();
        let miss = (entry + shared.miss * 2) as u32 | 1;
        Self {
            _arena: arena,
            entry: entry | 1,
            dispatch: (entry + shared.dispatch * 2) | 1,
            block: block | 1,
            count: compiled.instructions,
            table: alloc::vec![[0, miss]; 1 << lower::TABLE_BITS],
        }
    }

    /// Execute from `cpu`'s state with `budget`; returns the frame and the
    /// number of completed instructions.
    fn run_with(&self, cpu: &mut Cpu, budget: u32) -> (Frame, u32) {
        self.run_quiet(cpu, budget, false)
    }

    fn run_quiet(&self, cpu: &mut Cpu, budget: u32, quiet: bool) -> (Frame, u32) {
        let mut frame = Frame {
            regs: cpu.regs,
            eip: cpu.eip,
            remaining: budget,
            table: self.table.as_ptr() as usize as u32,
            dispatch: self.dispatch as u32,
            popf_mask: crate::x86::interp::popfd_change_mask(cpu).unwrap_or(0),
            quiet: u32::from(quiet),
            selectors: [
                SegmentReg::Es,
                SegmentReg::Cs,
                SegmentReg::Ss,
                SegmentReg::Ds,
                SegmentReg::Fs,
                SegmentReg::Gs,
            ]
            .map(|seg| u32::from(cpu.seg(seg).selector)),
            ..Frame::default()
        };
        frame.set_eflags(cpu.effective_eflags());
        frame.set_segments(cpu);
        let (tlb, state) = cpu.translation_cache.jit_view();
        frame.tlb = tlb as usize as u32;
        frame.tlb_state = state as u32;
        // SAFETY: test-owned arena, table and frame. TLB rows are either empty
        // or point into a live test buffer that is not resized during the call.
        unsafe {
            let entry: unsafe extern "C" fn(*mut Frame, usize) = core::mem::transmute(self.entry);
            entry(&mut frame, self.block);
        }
        let executed = budget - frame.remaining;
        (frame, executed)
    }

    fn run(&self, cpu: &mut Cpu) -> Frame {
        let (frame, executed) = self.run_with(cpu, self.count);
        if frame.exit == lower::EXIT_BOUNDARY {
            assert_eq!(executed, self.count);
        }
        frame
    }
}

fn compare(bytes: &[u8], seeds: u32) {
    let fixture = Fixture::new(bytes);
    let count = u64::from(fixture.count);
    let mut memory = Bytes(alloc::vec![0; 0x10000]);
    memory.0[0x100..0x100 + bytes.len()].copy_from_slice(bytes);
    for seed in 0..seeds {
        let mut expected = initial(seed);
        let mut start = expected.clone();
        let actual = fixture.run(&mut start);
        let stop = Interpreter::run_batch(&mut expected, &mut memory, 0, count).stop;
        assert_eq!(stop, BatchStop::Budget, "{bytes:02x?}");
        assert_eq!(
            actual.exit,
            lower::EXIT_BOUNDARY,
            "seed={seed} bytes={bytes:02x?}"
        );
        assert_eq!(actual.regs, expected.regs, "seed={seed} bytes={bytes:02x?}");
        assert_eq!(
            actual.eflags() | 2,
            expected.effective_eflags(),
            "seed={seed} bytes={bytes:02x?} start={:#x}",
            initial(seed).effective_eflags()
        );
        assert_eq!(actual.eip, expected.eip, "seed={seed} bytes={bytes:02x?}");
    }
}

#[test]
fn shared_stub_matches_reference() {
    // llvm-mc -triple=thumbv8m.main-none-eabi for: push.w {r4-r12, lr};
    // mov.w lr, r0; ldm.w lr, {r4-r11}; bx r1; dispatch: ldr.w r1, [lr, #60];
    // eor.w r2, r0, r0, lsr #12; lsl.w r2, r2, #19; add.w r1, r1, r2, lsr #16;
    // ldr.w r2, [r1]; cmp.w r2, r0; it eq; ldreq.w pc, [r1, #4]; miss:
    // str.w r0, [lr, #32]; mov.w r0, #0; str.w r0, [lr, #52];
    // stm.w lr, {r4-r11}; pop.w {r4-r12, pc}. It pins the frame offsets too.
    let shared = lower::shared();
    assert_eq!((shared.dispatch, shared.miss), (7, 22));
    assert_eq!(
        &shared.code[..],
        &[
            0xe92d, 0x5ff0, 0xea4f, 0x0e00, 0xe89e, 0x0ff0, 0x4708, 0xf8de, 0x103c, 0xea80, 0x3210,
            0xea4f, 0x42c2, 0xeb01, 0x4112, 0xf8d1, 0x2000, 0xebb2, 0x0f00, 0xbf08, 0xf8d1, 0xf004,
            0xf8ce, 0x0020, 0xf04f, 0x0000, 0xf8ce, 0x0034, 0xe88e, 0x0ff0, 0xe8bd, 0x9ff0
        ]
    );
}

#[test]
fn register_alu_matches_interpreter() {
    for op in [0x00, 0x08, 0x10, 0x18, 0x20, 0x28, 0x30, 0x38] {
        for (dst, src) in [(0, 1), (2, 2), (3, 6), (7, 4), (5, 0)] {
            let modrm = 0xc0 | src << 3 | dst;
            compare(&[op | 1, modrm], 40);
            compare(&[op | 3, modrm], 40);
            // Narrow ADC/SBB stay interpreted.
            if !matches!(op, 0x10 | 0x18) {
                compare(&[0x66, op | 1, modrm], 40);
                compare(&[op, modrm], 40);
            }
        }
        compare(&[op | 5, 0x78, 0x56, 0x34, 0x12], 30);
        compare(&[0x83, 0xc0 | op | 1, 0x80], 30);
        compare(&[0x81, 0xc0 | op | 3, 0x01, 0x00, 0x00, 0x80], 30);
        if !matches!(op, 0x10 | 0x18) {
            compare(&[op | 4, 0x7f], 30);
            compare(&[0x80, 0xc0 | op | 4, 0x81], 30);
            compare(&[0x66, 0x81, 0xc0 | op | 6, 0x34, 0x92], 30);
        }
    }
    for modrm in [0xc0, 0xc8, 0xe3, 0xfc] {
        compare(&[0x85, modrm], 30);
        compare(&[0x84, modrm], 30);
        compare(&[0xa9, 0x00, 0x80, 0x00, 0x00], 10);
        compare(&[0xa8, 0x81], 10);
    }
}

#[test]
fn carry_chains_and_partial_flag_writers_match_interpreter() {
    let sequences: &[&[u8]] = &[
        &[0x01, 0xc8, 0x11, 0xd3],                   // add eax,ecx; adc ebx,edx
        &[0x29, 0xc8, 0x19, 0xd3],                   // sub; sbb
        &[0x01, 0xc8, 0x19, 0xd3],                   // add; sbb
        &[0x29, 0xc8, 0x11, 0xd3],                   // sub; adc
        &[0x40, 0x11, 0xd3],                         // inc eax; adc
        &[0x39, 0xc8, 0x48, 0x19, 0xd3],             // cmp; dec eax; sbb
        &[0x01, 0xc8, 0x41, 0x42],                   // add; inc ecx; inc edx
        &[0x21, 0xc8, 0x11, 0xd3],                   // and; adc
        &[0xf9, 0x11, 0xd3],                         // stc; adc
        &[0xf8, 0x19, 0xd3],                         // clc; sbb
        &[0xf5, 0x11, 0xd3],                         // cmc; adc
        &[0x01, 0xc8, 0xf5],                         // add; cmc
        &[0x29, 0xc8, 0xf5],                         // sub; cmc
        &[0xfe, 0xc4, 0xfe, 0xcb],                   // inc ah; dec bl
        &[0x66, 0x40, 0x66, 0x4b],                   // inc ax; dec bx
        &[0xf7, 0xd8, 0xf6, 0xdc],                   // neg eax; neg ah
        &[0x66, 0xf7, 0xdb],                         // neg bx
        &[0xf7, 0xd0, 0xf6, 0xd4, 0x66, 0xf7, 0xd1], // not
        &[0xfc, 0xfd, 0xfc],                         // cld; std; cld
    ];
    for sequence in sequences {
        compare(sequence, 60);
    }
}

#[test]
fn conditions_branch_set_and_move_like_the_interpreter() {
    for cc in 0..16u8 {
        for writer in [
            &[0x39, 0xc8][..],
            &[0x01, 0xc8],
            &[0x85, 0xc8],
            &[0x48],
            &[0x40],
            &[0xf9],
            &[0xd1, 0xe0],
        ] {
            let mut setcc = writer.to_vec();
            setcc.extend([0x0f, 0x90 | cc, 0xc2, 0x0f, 0x90 | cc, 0xf7]);
            compare(&setcc, 30);
            let mut cmov = writer.to_vec();
            cmov.extend([0x0f, 0x40 | cc, 0xde]);
            compare(&cmov, 30);
            let mut branch = writer.to_vec();
            branch.extend([0x70 | cc, 0x10]);
            compare(&branch, 30);
            let mut long = writer.to_vec();
            long.extend([0x0f, 0x80 | cc, 0x00, 0x10, 0x00, 0x00]);
            compare(&long, 20);
        }
        compare(&[0x0f, 0x90 | cc, 0xc0], 30);
    }
    compare(&[0xe3, 0x05], 30);
    compare(&[0xeb, 0xfe], 4);
    compare(&[0xe9, 0x00, 0x00, 0x00, 0x80], 4);
}

#[test]
fn arm_jit_extended_loop_decode_is_isolated() {
    for opcode in 0xe0..=0xe2 {
        let bytes = [opcode, 0xfe, 0x90];
        assert!(decode::block(&bytes, 0x100, 3).instructions.is_empty());
        let block = decode::block_extended(&bytes, 0x100, 3);
        assert_eq!(block.byte_len, 2);
        assert_eq!(block.instructions.len(), 1);
        assert_eq!(
            block.instructions[0].op,
            decode::Op::Loop {
                target: 0x100,
                zero: match opcode {
                    0xe0 => Some(false),
                    0xe1 => Some(true),
                    _ => None,
                },
            }
        );
        for prefix in [0x66, 0x67, 0xf0] {
            assert!(
                decode::block_extended(&[prefix, opcode, 0], 0x100, 1)
                    .instructions
                    .is_empty()
            );
        }
    }
}

#[test]
fn arm_jit_loops_preserve_flags_and_wrap_ecx() {
    for opcode in 0xe0..=0xe2 {
        let bytes = [opcode, 0xfe];
        let fixture = Fixture::new(&bytes);
        for count in [1, 2, 0] {
            for flags in [0x402, 0x402 | STATUS] {
                let mut expected = initial(0);
                expected.regs[1] = count;
                expected.replace_eflags(flags);
                let mut start = expected.clone();
                // One step bounds the count-zero wraparound loop.
                let (actual, executed) = fixture.run_with(&mut start, 1);
                let mut memory = Bytes(alloc::vec![0; 0x10000]);
                memory.0[0x100..0x102].copy_from_slice(&bytes);
                assert_eq!(
                    Interpreter::run_batch(&mut expected, &mut memory, 0, 1).ran,
                    1
                );
                assert_eq!((actual.exit, executed), (lower::EXIT_BOUNDARY, 1));
                assert_eq!(
                    actual.regs, expected.regs,
                    "opcode={opcode:x} count={count}"
                );
                assert_eq!(actual.eflags() | 2, expected.effective_eflags());
                assert_eq!(actual.eip, expected.eip);
            }
        }
        // A writer before the loop and consumers in its successor must see
        // the same status, whether ECX reaches zero or the branch is taken.
        let first = [0x01, 0xd8, opcode, 0]; // ADD EAX,EBX; LOOP(E/NE) next
        let second = [0x0f, 0x92, 0xc2, 0x0f, 0x94, 0xc3, 0x11, 0xf8];
        let writer = Fixture::new(&first);
        let consumer = Fixture::at(&second, 0x104); // SETB DL; SETE BL; ADC EAX,EDI
        for seed in 0..30 {
            let mut expected = initial(seed);
            let mut start = expected.clone();
            let frame = writer.run(&mut start);
            start.regs = frame.regs;
            start.eip = frame.eip;
            start.replace_eflags(frame.eflags());
            let actual = consumer.run(&mut start);
            let mut memory = Bytes(alloc::vec![0; 0x10000]);
            memory.0[0x100..0x104].copy_from_slice(&first);
            memory.0[0x104..0x10c].copy_from_slice(&second);
            assert_eq!(
                Interpreter::run_batch(&mut expected, &mut memory, 0, 5).ran,
                5
            );
            assert_eq!(actual.exit, lower::EXIT_BOUNDARY);
            assert_eq!(actual.regs, expected.regs, "opcode={opcode:x} seed={seed}");
            assert_eq!(actual.eflags() | 2, expected.effective_eflags());
            assert_eq!(actual.eip, expected.eip);
        }
    }
}

#[test]
fn arm_jit_pushad_popad_restore_registers_and_skip_saved_esp() {
    for opcode in [0x60, 0x61] {
        assert!(decode::block(&[opcode], 0x100, 1).instructions.is_empty());
        let narrow = decode::block_extended(&[0x66, opcode], 0x100, 1);
        assert_eq!(narrow.instructions.len(), 1);
        assert!(lower::block(&narrow, ALL_FACTS).is_none());
    }
    let push = Fixture::new(&[0x60]);
    let pop = Fixture::at(&[0x61], 0x101);
    for stack in [0x5000, 0x5fff] {
        for seed in [0, 7, 29] {
            let mut expected = initial(seed);
            expected.regs[4] = stack;
            let original = expected.regs;
            let mut start = expected.clone();
            let mut memory = Bytes(alloc::vec![0xa5; 0x10000]);
            memory.0[0x100..0x102].copy_from_slice(&[0x60, 0x61]);
            let mut actual_memory = memory.0.clone();
            let page = (stack - 32) & !4095;
            start.fill_cached_write_translation(
                LinearAddr(page),
                BackingToken(actual_memory.as_mut_ptr() as u64 + u64::from(page)),
                false,
            );
            let frame = push.run(&mut start);
            assert_eq!(frame.exit, lower::EXIT_BOUNDARY);
            assert_eq!(
                Interpreter::run_batch(&mut expected, &mut memory, 0, 1).ran,
                1
            );
            assert_eq!(frame.regs, expected.regs);
            assert_eq!(frame.eflags() | 2, expected.effective_eflags());
            assert_eq!(actual_memory, memory.0);
            for (slot, register) in [7, 6, 5, 4, 3, 2, 1, 0].into_iter().enumerate() {
                let offset = (stack - 32) as usize + slot * 4;
                assert_eq!(
                    &actual_memory[offset..offset + 4],
                    &original[register].to_le_bytes()
                );
            }
            // Change every restorable register and corrupt the skipped ESP
            // slot: POPAD must restore the other seven and advance its own ESP.
            start.regs = initial(seed + 11).regs;
            start.regs[4] = frame.regs[4];
            start.eip = frame.eip;
            start.replace_eflags(frame.eflags());
            expected.regs = start.regs;
            let saved_esp = (stack - 20) as usize;
            actual_memory[saved_esp..saved_esp + 4].fill(0xde);
            memory.0[saved_esp..saved_esp + 4].fill(0xde);
            let frame = pop.run(&mut start);
            assert_eq!(frame.exit, lower::EXIT_BOUNDARY);
            assert_eq!(
                Interpreter::run_batch(&mut expected, &mut memory, 1, 1).ran,
                1
            );
            assert_eq!(frame.regs, original);
            assert_eq!(frame.regs, expected.regs);
            assert_eq!(frame.eip, expected.eip);
            assert_eq!(frame.eflags() | 2, expected.effective_eflags());
            assert_eq!(actual_memory, memory.0);
        }
    }
}

#[test]
fn arm_jit_pushad_popad_page_splits_exit_before_any_effect() {
    for (opcode, stack, base) in [(0x60, 0x500f, 0x4fef), (0x61, 0x4ff0, 0x4ff0)] {
        let fixture = Fixture::new(&[opcode]);
        let mut expected = initial(9);
        expected.regs[4] = stack;
        let mut start = expected.clone();
        let mut memory = Bytes((0..0x10000).map(|i| (i * 37) as u8).collect());
        memory.0[0x100] = opcode;
        let mut actual_memory = Bytes(memory.0.clone());
        // Both pages are cached, but one native guard must reject their range.
        for page in [base & !4095, (base + 31) & !4095] {
            start.fill_cached_write_translation(
                LinearAddr(page),
                BackingToken(actual_memory.0.as_mut_ptr() as u64 + u64::from(page)),
                false,
            );
        }
        let (frame, executed) = fixture.run_with(&mut start, 1);
        assert_eq!((frame.exit, executed), (lower::EXIT_INTERPRET, 0));
        assert_eq!(frame.regs, start.regs);
        assert_eq!(frame.eip, start.eip);
        assert_eq!(frame.eflags() | 2, start.effective_eflags());
        assert_eq!(actual_memory.0, memory.0);
        // Bytes uses physical-address tokens. Discard the native pointer rows
        // before exercising the ordinary interpreter handoff on this bus.
        start.invalidate_translation_cache();
        assert_eq!(
            Interpreter::run_batch(&mut start, &mut actual_memory, 0, 1).ran,
            1
        );
        assert_eq!(
            Interpreter::run_batch(&mut expected, &mut memory, 0, 1).ran,
            1
        );
        assert_eq!(start.regs, expected.regs);
        assert_eq!(start.eip, expected.eip);
        assert_eq!(start.effective_eflags(), expected.effective_eflags());
        assert_eq!(actual_memory.0, memory.0);
    }
}

#[test]
fn arm_jit_pushfd_images_match_interpreter() {
    fn check(bytes: &[u8], mut cpu: Cpu) {
        let fixture = Fixture::new(bytes);
        cpu.regs[4] = 0x5000;
        let mut expected = cpu.clone();
        let mut memory = Bytes(alloc::vec![0; 0x10000]);
        memory.0[0x100..0x100 + bytes.len()].copy_from_slice(bytes);
        let mut actual_memory = memory.0.clone();
        cpu.fill_cached_write_translation(
            LinearAddr(0x4000),
            BackingToken(actual_memory.as_mut_ptr() as u64 + 0x4000),
            false,
        );
        let actual = fixture.run(&mut cpu);
        let batch = Interpreter::run_batch(&mut expected, &mut memory, 0, u64::from(fixture.count));
        assert_eq!(batch.ran, u64::from(fixture.count));
        assert_eq!(actual.exit, lower::EXIT_BOUNDARY);
        assert_eq!(actual.regs, expected.regs);
        assert_eq!(actual.eip, expected.eip);
        assert_eq!(actual.eflags() | 2, expected.effective_eflags());
        assert_eq!(actual_memory, memory.0);
        let image = u32::from_le_bytes(actual_memory[0x4ffc..0x5000].try_into().unwrap());
        assert_eq!(image & (3 << 16), 0, "PUSHFD image clears RF/VM");
    }
    for flag in [
        0,
        1,
        4,
        0x10,
        0x40,
        0x80,
        0x800,
        0x400,
        STATUS | 0x400 | 0x10000,
    ] {
        let mut cpu = initial(7);
        cpu.replace_eflags(2 | flag);
        check(&[0x9c], cpu);
    }
    // Exercise full arithmetic result/auxiliary values, not only the compact
    // representation set at entry. PUSHFD must keep every prior status bit live.
    for seed in 0..30 {
        check(&[0x01, 0xd8, 0x9c], initial(seed)); // ADD EAX,EBX; PUSHFD
    }
}

fn flag_cpu(cpl: u16, flags: u32) -> Cpu {
    let mut cpu = initial(7);
    cpu.regs[4] = 0x5000;
    if cpl == 3 {
        cpu.set_seg_cache(SegmentReg::Cs, SegmentCache::new(0x0b, 0, u32::MAX, 0xc0fb));
        cpu.set_seg_cache(SegmentReg::Ss, SegmentCache::new(0x13, 0, u32::MAX, 0xc0f3));
    }
    cpu.replace_eflags(flags);
    assert_eq!(u16::from(cpu.cpl()), cpl);
    cpu
}

#[test]
fn arm_jit_popfd_bits_privilege_and_precise_exits_match_interpreter() {
    let bytes = [0x90, 0x9d, 0x0f, 0x92, 0xc2]; // NOP; POPFD; SETB DL
    let fixture = Fixture::new(&bytes);
    for cpl in [0, 3] {
        for old_if in [0, 1 << 9] {
            for bit in 0..32 {
                let image = 1u32 << bit;
                let mut cpu = flag_cpu(cpl, 2 | STATUS | 0x400 | 0x10000 | old_if);
                let mut expected = cpu.clone();
                let mut memory = Bytes(alloc::vec![0; 0x10000]);
                memory.0[0x100..0x105].copy_from_slice(&bytes);
                memory.0[0x5000..0x5004].copy_from_slice(&image.to_le_bytes());
                let mut actual_memory = memory.0.clone();
                cpu.fill_cached_read_translation(
                    LinearAddr(0x5000),
                    BackingToken(actual_memory.as_mut_ptr() as u64 + 0x5000),
                    cpl == 3,
                );
                let (actual, completed) = fixture.run_with(&mut cpu, 3);
                let (exit, count) = if bit == 8 {
                    (lower::EXIT_INTERPRET, 1) // TF image: only the preceding NOP ran.
                } else if cpl == 0 && old_if == 0 && bit == 9 {
                    (lower::EXIT_ATTENTION, 2) // POPFD completed; SETB is refunded.
                } else {
                    (lower::EXIT_BOUNDARY, 3)
                };
                assert_eq!(
                    (actual.exit, completed),
                    (exit, count),
                    "cpl={cpl} IF={old_if} bit={bit}"
                );
                assert_eq!(
                    Interpreter::run_batch(&mut expected, &mut memory, 0, u64::from(count)).ran,
                    u64::from(count)
                );
                assert_eq!(actual.regs, expected.regs);
                assert_eq!(actual.eip, expected.eip);
                assert_eq!(
                    actual.eflags() | 2,
                    expected.effective_eflags(),
                    "cpl={cpl} IF={old_if} bit={bit}"
                );
                assert_eq!(actual_memory, memory.0);
                if cpl == 3 {
                    assert_eq!(
                        actual.eflags() & 0x3200,
                        old_if,
                        "IF and IOPL are not writable"
                    );
                }
            }
        }
    }
}

#[test]
fn arm_jit_cli_sti_privilege_and_exit_kinds_match_interpreter() {
    for opcode in [0xfa, 0xfb] {
        let fixture = Fixture::new(&[opcode]);
        for cpl in [0, 3] {
            for old_if in [0, 1 << 9] {
                let mut cpu = flag_cpu(cpl, 2 | STATUS | 0x400 | old_if);
                let mut expected = cpu.clone();
                let (actual, count) = fixture.run_with(&mut cpu, 1);
                let mut memory = Bytes(alloc::vec![0; 0x10000]);
                memory.0[0x100] = opcode;
                if cpl == 3 {
                    assert_eq!((actual.exit, count), (lower::EXIT_INTERPRET, 0));
                    assert_eq!(actual.regs, expected.regs);
                    assert_eq!(actual.eip, expected.eip);
                    assert_eq!(actual.eflags() | 2, expected.effective_eflags());
                    assert!(matches!(
                        Interpreter::step(&mut expected, &mut memory),
                        Err(crate::x86::StepError::GeneralProtection { .. })
                    ));
                } else {
                    let exit = if opcode == 0xfa {
                        lower::EXIT_BOUNDARY
                    } else if old_if == 0 {
                        lower::EXIT_SHADOW
                    } else {
                        lower::EXIT_ATTENTION
                    };
                    assert_eq!((actual.exit, count), (exit, 1));
                    assert_eq!(
                        Interpreter::run_batch(&mut expected, &mut memory, 0, 1).ran,
                        1
                    );
                    assert_eq!(actual.regs, expected.regs);
                    assert_eq!(actual.eip, expected.eip);
                    assert_eq!(actual.eflags() | 2, expected.effective_eflags());
                }
            }
        }
    }
}

// Use the public boot-sector API so this Machine check runs in the supported
// Linux host configuration without exposing Machine internals.
#[test]
fn arm_jit_sti_shadow_and_popfd_pending_irq() {
    use crate::machine::Machine;

    const ENTRY: u32 = 0x7c58;
    const HANDLER: u32 = 0x7c80;

    fn machine(jit: bool, popfd: bool) -> Machine {
        let mut sector = [0u8; 512];
        // CLI; LGDT; LIDT; read/set CR0.PE; far jump to the 32-bit code.
        let setup16 = [
            0xfa, 0x0f, 0x01, 0x16, 0x20, 0x7d, 0x0f, 0x01, 0x1e, 0x26, 0x7d, 0x0f, 0x20, 0xc0,
            0x66, 0x83, 0xc8, 1, 0x0f, 0x22, 0xc0, 0x66, 0xea, 0x40, 0x7c, 0, 0, 8, 0,
        ];
        sector[..setup16.len()].copy_from_slice(&setup16);
        // Set flat data/stack segments, prepare the POPFD image, unmask IRQ1.
        let setup32 = [
            0x66, 0xb8, 0x10, 0, 0x8e, 0xd8, 0x8e, 0xc0, 0x8e, 0xd0, 0xbc, 4, 0x90, 0, 0, 0x68, 2,
            2, 0, 0, 0xb0, 0xfd, 0xe6, 0x21,
        ];
        sector[0x40..0x58].copy_from_slice(&setup32);
        // LAHF ends the compiled POPFD prefix so POPFD itself fits the
        // pending-attention limit of one instruction.
        let loop_steps = if popfd {
            sector[0x58..0x62].copy_from_slice(&[0x9d, 0x9f, 0xfa, 0x68, 2, 2, 0, 0, 0xeb, 0xf6]);
            5
        } else {
            sector[0x58..0x5d].copy_from_slice(&[0xfb, 0x90, 0xfa, 0xeb, 0xfb]);
            4
        };
        sector[0x80] = 0xf4; // IRQ handler: HLT.
        for (offset, descriptor) in [(0x108, 0x00cf9b000000ffffu64), (0x110, 0x00cf93000000ffff)] {
            sector[offset..offset + 8].copy_from_slice(&descriptor.to_le_bytes());
        }
        sector[0x120..0x126].copy_from_slice(&[23, 0, 0, 0x7d, 0, 0]);
        sector[0x126..0x12c].copy_from_slice(&[0x0f, 1, 0xe0, 0x7c, 0, 0]);
        // IDT vector 0x21 at 0x7ce0 + 0x108 points to the handler, selector 8.
        sector[0x1e8..0x1ec].copy_from_slice(&(HANDLER | 8 << 16).to_le_bytes());
        sector[0x1ec..0x1f0].copy_from_slice(&0x8e00u32.to_le_bytes());
        sector[510..512].copy_from_slice(&[0x55, 0xaa]);
        let mut machine = Machine::new(0x10000);
        machine.load_bootsector(&sector).unwrap();
        machine.enter_bootsector();
        machine.set_jit_enabled(jit).unwrap();
        let warm_steps = 7 + 8 + loop_steps * 256;
        assert_eq!(machine.run_steps(warm_steps).unwrap().ran, warm_steps);
        assert_eq!(machine.cpu().eip, ENTRY);
        assert_eq!(machine.cpu().effective_eflags() & (1 << 9), 0);
        machine
    }
    fn native_steps(machine: &Machine) -> u64 {
        machine
            .jit_report()
            .split("native_steps: ")
            .nth(1)
            .unwrap()
            .split(',')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }
    for popfd in [false, true] {
        let mut expected = machine(false, popfd);
        let mut native = machine(true, popfd);
        let before = native_steps(&native);
        assert!(before > 256);
        for machine in [&mut expected, &mut native] {
            machine.inject_scancode_set2(&[0x1c]);
            assert_eq!(machine.run_steps(1).unwrap().ran, 1); // STI or POPFD
            if !popfd {
                assert!(machine.cpu().interrupt_shadow());
                assert_eq!(machine.cpu().eip, ENTRY + 1, "IRQ must not precede NOP");
            }
        }
        assert_eq!(
            native_steps(&native),
            before + 1,
            "the IF-enable instruction must execute natively"
        );
        for machine in [&mut expected, &mut native] {
            if !popfd {
                assert_eq!(machine.run_steps(1).unwrap().ran, 1); // NOP, then IRQ1
            }
            assert_eq!(machine.cpu().eip, HANDLER);
            assert!(!machine.cpu().interrupt_shadow());
            let sp = machine.cpu().regs[4] as usize;
            let return_eip = u32::from_le_bytes(machine.ram()[sp..sp + 4].try_into().unwrap());
            assert_eq!(return_eip, if popfd { ENTRY + 1 } else { ENTRY + 2 });
        }
        assert_eq!(native.cpu().regs, expected.cpu().regs);
        assert_eq!(
            native.cpu().effective_eflags(),
            expected.cpu().effective_eflags()
        );
        assert_eq!(native.cpu().tsc(), expected.cpu().tsc());
        assert_eq!(native.ram(), expected.ram());
    }
}

#[test]
fn shifts_multiplies_and_bit_operations_match_interpreter() {
    for group in [4, 5, 7] {
        for count in [1, 2, 7, 31] {
            for dst in [0, 3, 7] {
                compare(&[0xc1, 0xc0 | group << 3 | dst, count], 40);
            }
        }
        compare(&[0xd1, 0xc0 | group << 3 | 1], 40);
        compare(&[0xc1, 0xc0 | group << 3, 0], 10);
    }
    // Shifts and rotates by CL, including masked zero counts that keep flags.
    for group in [0, 1, 4, 5, 7] {
        for count in [0, 1, 2, 31, 32, 33, 0xff] {
            for dst in [0, 2, 7] {
                compare(&[0xb1, count, 0xd3, 0xc0 | group << 3 | dst], 30);
                compare(
                    &[0xb1, count, 0x01, 0xd8, 0xd3, 0xc0 | group << 3 | dst],
                    20,
                );
            }
        }
        compare(&[0xd3, 0xc0 | group << 3 | 6], 40);
    }
    for group in [0, 1] {
        for count in [1, 2, 31] {
            compare(&[0xc1, 0xc0 | group << 3 | 3, count], 40);
            compare(
                &[
                    0x01,
                    0xc8,
                    0xc1,
                    0xc0 | group << 3 | 3,
                    count,
                    0x0f,
                    0x92,
                    0xc1,
                ],
                30,
            );
        }
    }
    // Narrow SHL/SHR/SAR, including AH and counts beyond the width.
    for group in [4, 5, 7] {
        for dst in [0, 4, 7] {
            for count in [1, 2, 7, 8, 9, 16, 17, 31] {
                compare(&[0xc0, 0xc0 | group << 3 | dst, count], 30);
                compare(&[0x66, 0xc1, 0xc0 | group << 3 | dst, count], 30);
                compare(&[0xb1, count, 0xd2, 0xc0 | group << 3 | dst], 20);
                compare(&[0xb1, count, 0x66, 0xd3, 0xc0 | group << 3 | dst], 20);
            }
            compare(&[0xd0, 0xc0 | group << 3 | dst, 0x01, 0xd8], 20);
            compare(&[0x66, 0xd1, 0xc0 | group << 3 | dst], 30);
        }
    }
    // SHLD/SHRD by immediate and CL, including dst == src and CL operands;
    // the trailing ADD leaves the shift's own flags dead.
    for op in [0xa4, 0xac] {
        for modrm in [0xd0, 0xc0, 0xc9, 0xc1] {
            for count in [0, 1, 2, 31] {
                compare(&[0x0f, op, modrm, count], 30);
                compare(&[0x0f, op, modrm, count, 0x01, 0xd8], 10);
            }
            for count in [0, 1, 2, 31, 32, 33] {
                compare(&[0xb1, count, 0x0f, op | 1, modrm], 30);
                compare(&[0xb1, count, 0x01, 0xd8, 0x0f, op | 1, modrm], 20);
                compare(&[0xb1, count, 0x0f, op | 1, modrm, 0x01, 0xd8], 10);
            }
        }
    }
    for (dst, src) in [(0, 1), (2, 2), (6, 7)] {
        compare(&[0x0f, 0xaf, 0xc0 | dst << 3 | src], 40);
        compare(&[0x6b, 0xc0 | dst << 3 | src, 0x85], 40);
        compare(&[0x69, 0xc0 | dst << 3 | src, 0x01, 0x00, 0x01, 0x00], 40);
    }
    for src in [0, 1, 2, 3] {
        compare(&[0xf7, 0xe0 | src], 40);
        compare(&[0xf7, 0xe8 | src], 40);
    }
    for op in [0xa3, 0xab, 0xb3, 0xbb] {
        compare(&[0x0f, op, 0xc8], 40);
        compare(&[0x0f, op, 0xd9], 40);
        compare(&[0x66, 0x0f, op, 0xc8], 40);
    }
    for group in 4..8 {
        for bit in [0, 5, 31, 33] {
            compare(&[0x0f, 0xba, 0xc0 | group << 3 | 2, bit], 30);
        }
    }
    for op in [0xbc, 0xbd] {
        compare(&[0x0f, op, 0xc1], 40);
        compare(&[0x0f, op, 0xd2], 40);
    }
}

// One memory instruction, with native pointer rows separate from the oracle's
// physical-address bus. A miss must leave every architectural effect untouched.
fn compare_memory_instruction(bytes: &[u8], mut cpu: Cpu, pages: &[(u32, bool)], native: bool) {
    let fixture = Fixture::new(bytes);
    let mut expected = cpu.clone();
    let mut memory = Bytes((0..0x10000).map(|i| (i * 37 + 11) as u8).collect());
    memory.0[0x100..0x100 + bytes.len()].copy_from_slice(bytes);
    let mut actual_memory = Bytes(memory.0.clone());
    for &(page, write) in pages {
        let backing = BackingToken(actual_memory.0.as_mut_ptr() as u64 + u64::from(page));
        if write {
            cpu.fill_cached_write_translation(LinearAddr(page), backing, false);
        } else {
            cpu.fill_cached_read_translation(LinearAddr(page), backing, false);
        }
    }
    let (frame, count) = fixture.run_with(&mut cpu, 1);
    assert_eq!(
        Interpreter::run_batch(&mut expected, &mut memory, 0, 1).ran,
        1
    );
    if native {
        assert_eq!(
            (frame.exit, count),
            (lower::EXIT_BOUNDARY, 1),
            "{bytes:02x?}"
        );
        assert_eq!(frame.regs, expected.regs, "{bytes:02x?}");
        assert_eq!(frame.eip, expected.eip);
        assert_eq!(frame.eflags() | 2, expected.effective_eflags());
    } else {
        assert_eq!(
            (frame.exit, count),
            (lower::EXIT_INTERPRET, 0),
            "{bytes:02x?}"
        );
        assert_eq!(frame.regs, cpu.regs);
        assert_eq!(frame.eip, cpu.eip);
        assert_eq!(frame.eflags() | 2, cpu.effective_eflags());
        let mut original = Bytes((0..0x10000).map(|i| (i * 37 + 11) as u8).collect());
        original.0[0x100..0x100 + bytes.len()].copy_from_slice(bytes);
        assert_eq!(actual_memory.0, original.0);
        cpu.invalidate_translation_cache();
        assert_eq!(
            Interpreter::run_batch(&mut cpu, &mut actual_memory, 0, 1).ran,
            1
        );
        assert_eq!(cpu.regs, expected.regs);
        assert_eq!(cpu.eip, expected.eip);
        assert_eq!(cpu.effective_eflags(), expected.effective_eflags());
    }
    assert_eq!(actual_memory.0, memory.0, "{bytes:02x?}");
}

#[test]
fn arm_jit_xchg16_preserves_high_halves_and_memory_boundaries() {
    for src in 0..8 {
        for dst in 0..8 {
            compare(&[0x66, 0x87, 0xc0 | src << 3 | dst], 30);
        }
        if src != 0 {
            compare(&[0x66, 0x90 | src], 30);
        }
        for address in [0x4008, 0x4ffe, 0x4fff] {
            let mut cpu = initial(7);
            cpu.regs[3] = address;
            compare_memory_instruction(
                &[0x66, 0x87, src << 3 | 3],
                cpu,
                &[(0x4000, true), (0x5000, true)],
                address != 0x4fff,
            );
        }
    }
    let mut cpu = initial(7);
    cpu.regs[3] = 0x4008;
    compare_memory_instruction(&[0x66, 0x87, 3], cpu, &[(0x4000, false)], false);
}

#[test]
fn arm_jit_push_segment_only_writes_the_selector_word() {
    for opcode in [0x0e, 0x16] {
        assert!(decode::block(&[opcode], 0x100, 1).instructions.is_empty());
    }
    for (segment, bytes) in [
        (SegmentReg::Es, &[0x06][..]),
        (SegmentReg::Cs, &[0x0e][..]),
        (SegmentReg::Ss, &[0x16][..]),
        (SegmentReg::Ds, &[0x1e][..]),
        (SegmentReg::Fs, &[0x0f, 0xa0][..]),
        (SegmentReg::Gs, &[0x0f, 0xa8][..]),
    ] {
        for stack in [0x5000, 0x5002, 0x5003] {
            let mut cpu = initial(9);
            cpu.regs[4] = stack;
            cpu.set_seg_cache(
                segment,
                SegmentCache {
                    selector: 0xabc0 + 8 * segment as u16,
                    ..cpu.seg(segment)
                },
            );
            // At 0x5002 the untouched upper word is in an uncached page.
            // At 0x5003 the selector itself crosses the page and must miss.
            compare_memory_instruction(bytes, cpu, &[(0x4000, true)], stack != 0x5003);
        }
    }
}

#[test]
fn arm_jit_rcl_rcr_one_match_interpreter_and_preserve_other_flags() {
    for group in [2, 3] {
        for reg in 0..8 {
            let modrm = 0xc0 | group << 3 | reg;
            compare(&[0xd1, modrm], 40);
            compare(&[0xc1, modrm, 1], 40);
            compare(&[0x01, 0xc8, 0xd1, modrm, 0x11, 0xcb], 40);
            // CF is a value input even with all rotate output flags dead.
            compare(&[0xf9, 0xd1, modrm, 0x31, 0xd2], 30);
        }
    }
}

#[test]
fn arm_jit_lods_stos_width_direction_and_precise_misses() {
    for load in [false, true] {
        for bits in [8, 16, 32] {
            let mut bytes = Vec::new();
            if bits == 16 {
                bytes.push(0x66);
            }
            bytes.push(match (load, bits) {
                (true, 8) => 0xac,
                (true, _) => 0xad,
                (false, 8) => 0xaa,
                (false, _) => 0xab,
            });
            for backward in [false, true] {
                for address in [0x4008, 0x4fff] {
                    let mut cpu = initial(9);
                    cpu.replace_eflags(0x202 | STATUS | if backward { 0x400 } else { 0 });
                    cpu.regs[if load { 6 } else { 7 }] = address;
                    let native = address != 0x4fff || bits == 8;
                    compare_memory_instruction(
                        &bytes,
                        cpu.clone(),
                        &[(0x4000, !load), (0x5000, !load)],
                        native,
                    );
                    compare_memory_instruction(&bytes, cpu, &[], false);
                }
            }
            if load {
                let mut cpu = initial(4);
                cpu.regs[6] = 0x3008;
                cpu.set_seg_cache(
                    SegmentReg::Fs,
                    SegmentCache {
                        base: 0x1000,
                        limit: 0x3fff,
                        ..cpu.seg(SegmentReg::Fs)
                    },
                );
                bytes.insert(0, 0x64);
                compare_memory_instruction(&bytes, cpu, &[(0x4000, false)], true);
            }
        }
    }
}

#[test]
fn moves_extensions_and_exchanges_match_interpreter() {
    let sequences: &[&[u8]] = &[
        &[0x89, 0xc8],
        &[0x88, 0xe0],
        &[0x88, 0xc4],
        &[0x8a, 0xfc],
        &[0x66, 0x89, 0xd8],
        &[0xb4, 0x99, 0xb0, 0x11, 0x66, 0xb9, 0x34, 0x12],
        &[0xbb, 0x78, 0x56, 0x34, 0x12],
        &[0x0f, 0xb6, 0xc4],
        &[0x0f, 0xbe, 0xc4],
        &[0x0f, 0xb7, 0xd9],
        &[0x0f, 0xbf, 0xd9],
        &[0x66, 0x0f, 0xb6, 0xc3],
        &[0x98, 0x66, 0x98, 0x99, 0x66, 0x99],
        &[0x0f, 0xc9, 0x0f, 0xc8],
        &[0x87, 0xd9, 0x91, 0x90],
        &[0x0f, 0xc1, 0xd9],
        &[0x0f, 0xc1, 0xc0],
        &[0x0f, 0xb1, 0xd9],
        &[0x0f, 0xb1, 0xc0],
        &[0x39, 0xc1, 0x0f, 0xb1, 0xd9],
        &[0x8d, 0x44, 0x8b, 0x10],
        &[0x8d, 0x04, 0x05, 0x00, 0x01, 0x00, 0x00],
        &[0x8d, 0x80, 0x78, 0x56, 0x34, 0x12],
        &[0x66, 0x8d, 0x43, 0xff],
        &[0x8d, 0x05, 0x10, 0x00, 0x00, 0x00],
    ];
    for sequence in sequences {
        compare(sequence, 40);
    }
}

#[test]
fn memory_operands_exit_before_the_access_without_a_translation() {
    // INC EAX; MOV EAX,[EBX]; with an empty TLB the block stops at the load.
    let bytes = [0x40, 0x8b, 0x03, 0x89, 0x0a];
    let fixture = Fixture::new(&bytes);
    for seed in 0..8 {
        let mut expected = initial(seed);
        let mut start = expected.clone();
        let actual = fixture.run(&mut start);
        let mut memory = Bytes(alloc::vec![0; 0x10000]);
        memory.0[0x100..0x105].copy_from_slice(&bytes);
        Interpreter::run_batch(&mut expected, &mut memory, 0, 1);
        assert_eq!(actual.exit, lower::EXIT_INTERPRET);
        assert_eq!(actual.regs, expected.regs);
        assert_eq!(actual.eflags() | 2, expected.effective_eflags());
        assert_eq!(actual.eip, 0x101);
    }
    let fixture = Fixture::new(&[0x50]);
    let mut cpu = initial(3);
    let before = cpu.clone();
    let (actual, executed) = fixture.run_with(&mut cpu, 1);
    assert_eq!(
        (actual.exit, executed, actual.regs, actual.eip),
        (lower::EXIT_INTERPRET, 0, before.regs, 0x100)
    );
}

#[test]
fn blocks_reserve_their_whole_length_from_the_budget() {
    let fixture = Fixture::new(&[0x40, 0x41, 0x42]);
    for budget in [0, 1, 2] {
        let mut cpu = initial(5);
        let before = cpu.clone();
        let (frame, executed) = fixture.run_with(&mut cpu, budget);
        assert_eq!(
            (frame.exit, executed, frame.eip, frame.regs),
            (lower::EXIT_BOUNDARY, 0, 0x100, before.regs)
        );
        assert_eq!(frame.eflags() | 2, before.effective_eflags());
    }
    let mut cpu = initial(5);
    let (frame, executed) = fixture.run_with(&mut cpu, 7);
    assert_eq!((frame.remaining, executed, frame.eip), (4, 3, 0x103));
}

/// The block leaves before its last instruction, having completed the others
/// exactly as the interpreter does.
fn leaves_before_last(bytes: &[u8], completed: u32) {
    let fixture = Fixture::new(bytes);
    let mut memory = Bytes(alloc::vec![0; 0x10000]);
    memory.0[0x100..0x100 + bytes.len()].copy_from_slice(bytes);
    for seed in 0..8 {
        let mut expected = initial(seed);
        let mut start = expected.clone();
        let (actual, executed) = fixture.run_with(&mut start, fixture.count);
        Interpreter::run_batch(&mut expected, &mut memory, 0, u64::from(completed));
        assert_eq!(
            (actual.exit, executed),
            (lower::EXIT_INTERPRET, completed),
            "seed={seed} bytes={bytes:02x?}"
        );
        assert_eq!(actual.regs, expected.regs, "seed={seed} bytes={bytes:02x?}");
        assert_eq!(
            actual.eflags() | 2,
            expected.effective_eflags(),
            "seed={seed} bytes={bytes:02x?}"
        );
        assert_eq!(actual.eip, expected.eip, "seed={seed} bytes={bytes:02x?}");
    }
}

#[test]
fn division_matches_interpreter_and_leaves_before_a_fault() {
    for divisor in [
        1u32,
        2,
        3,
        7,
        0x10,
        0x7fff_ffff,
        0x8000_0000,
        0xffff_fffe,
        0x1234_5678,
    ] {
        // mov ecx,divisor; xor edx,edx; div ecx
        let mut unsigned = alloc::vec![0xb9];
        unsigned.extend(divisor.to_le_bytes());
        unsigned.extend([0x31, 0xd2, 0xf7, 0xf1]);
        compare(&unsigned, 40);
        // mov ecx,divisor; cdq; idiv ecx
        let mut signed = alloc::vec![0xb9];
        signed.extend(divisor.to_le_bytes());
        signed.extend([0x99, 0xf7, 0xf9]);
        compare(&signed, 40);
    }
    // The divisor may be the dividend's own registers.
    compare(&[0xb8, 0x39, 0x30, 0x00, 0x00, 0x31, 0xd2, 0xf7, 0xf0], 8); // div eax
    compare(&[0xb8, 0x39, 0x30, 0x00, 0x00, 0x99, 0xf7, 0xf8], 8); // idiv eax
    // mov ecx,0; xor edx,edx; div ecx: #DE belongs to the interpreter.
    leaves_before_last(&[0xb9, 0, 0, 0, 0, 0x31, 0xd2, 0xf7, 0xf1], 2);
    // mov edx,1; div ecx: the dividend does not fit 32 bits.
    leaves_before_last(&[0xba, 1, 0, 0, 0, 0xf7, 0xf1], 1);
    // mov edx,5; idiv ecx: EDX is not the sign of EAX.
    leaves_before_last(&[0xba, 5, 0, 0, 0, 0xf7, 0xf9], 1);
    // mov eax,80000000h; mov ecx,-1; cdq; idiv ecx: the quotient overflows.
    leaves_before_last(
        &[
            0xb8, 0, 0, 0, 0x80, 0xb9, 0xff, 0xff, 0xff, 0xff, 0x99, 0xf7, 0xf9,
        ],
        3,
    );
}

// A sparse two-page bus exercises the top of the 32-bit offset space without
// a 4 GiB allocation. Native TLB rows point at a separate, stable byte buffer.
#[derive(Clone)]
struct SegmentMemory {
    code: Vec<u8>,
    data: Vec<u8>,
    page: u32,
}
impl Bus for SegmentMemory {
    fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
        if let Some(&byte) = self.code.get(addr.0.wrapping_sub(0x100) as usize) {
            return Ok(byte);
        }
        self.data
            .get((addr.0 as u32).wrapping_sub(self.page) as usize)
            .copied()
            .ok_or(MemError::OutOfRange { addr })
    }
    fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError> {
        self.data
            .get((addr.0 as u32).wrapping_sub(self.page) as usize)
            .map(|_| ())
            .ok_or(MemError::OutOfRange { addr })
    }
    fn write_phys_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
        *self
            .data
            .get_mut((addr.0 as u32).wrapping_sub(self.page) as usize)
            .ok_or(MemError::OutOfRange { addr })? = value;
        Ok(())
    }
}

#[test]
fn arm_jit_checked_data_segments_match_interpreter_ranges_and_faults() {
    use crate::x86::StepError;
    for segment in [SegmentReg::Ds, SegmentReg::Ss] {
        for write in [false, true] {
            for width in [1u32, 2, 4] {
                let mut bytes = Vec::new();
                if width == 2 {
                    bytes.push(0x66);
                }
                bytes.push(match (write, width) {
                    (false, 1) => 0x8a,
                    (true, 1) => 0x88,
                    (false, _) => 0x8b,
                    (true, _) => 0x89,
                });
                if segment == SegmentReg::Ss {
                    bytes.extend([0x45, 0]);
                }
                // [EBP]
                else {
                    bytes.push(3);
                } // [EBX]
                // One compiled block must work through all descriptor states,
                // including flat after an earlier non-flat or null segment.
                let fixture = Fixture::with_facts(&bytes, 0x100, 0);
                for (base, limit, flags, selector, offset, inside) in [
                    (0, 0x3fff, 0xc096, 0x10, 0x3fff, false),
                    (0, 0x3fff, 0xc096, 0x10, 0x4000, true),
                    (0, 0x3fff, 0xc096, 0x10, u32::MAX - width + 1, true),
                    (0, 0x3fff, 0xc096, 0x10, u32::MAX, width == 1),
                    (0, u32::MAX, 0xc096, 0x10, u32::MAX, false),
                    (0, 0x3fff, 0x8096, 0x10, 0x3fff, false),
                    (0, 0x3fff, 0x8096, 0x10, 0x4000, true),
                    (0, 0x3fff, 0x8096, 0x10, 0x10000 - width, true),
                    (0, 0x3fff, 0x8096, 0x10, 0xffff, width == 1),
                    (0, 0x3fff, 0x8096, 0x10, 0x10000, false),
                    (0, 0xffff, 0x8096, 0x10, 0xffff, false),
                    (0, 0x1ffff, 0x8096, 0x10, 0x20000, false),
                    (0x2000, 0xff, 0xc092, 0x10, 0x100 - width, true),
                    (0x2000, 0xff, 0xc092, 0x10, 0xff, width == 1),
                    (0x2000, 0xff, 0xc092, 0x10, 0x100, false),
                    (0x2000, 0xff, 0xc090, 0x10, 0x40, !write),
                    (0, 0, 0, 0, 0x4000, false),
                    (0, u32::MAX, 0xc092, 0x10, 0x4000, true),
                ] {
                    let mut cpu = initial(7);
                    cpu.regs[if segment == SegmentReg::Ss { 5 } else { 3 }] = offset;
                    cpu.set_seg_cache(segment, SegmentCache::new(selector, base, limit, flags));
                    let mut expected = cpu.clone();
                    let mut memory = SegmentMemory {
                        code: bytes.clone(),
                        data: (0..8192).map(|i| (i * 37 + 11) as u8).collect(),
                        page: base.wrapping_add(offset) & !0xfff,
                    };
                    let original = memory.clone();
                    let mut native = memory.clone();
                    for page in [0, 4096] {
                        cpu.fill_cached_write_translation(
                            LinearAddr(memory.page.wrapping_add(page)),
                            BackingToken(native.data.as_mut_ptr() as u64 + u64::from(page)),
                            false,
                        );
                    }
                    let (frame, count) = fixture.run_with(&mut cpu, 1);
                    let oracle = Interpreter::step(&mut expected, &mut memory);
                    if inside {
                        assert!(
                            oracle.is_ok(),
                            "{segment:?} {write} {width} {offset:x}: {oracle:?}"
                        );
                        assert_eq!(
                            (frame.exit, count),
                            (lower::EXIT_BOUNDARY, 1),
                            "{segment:?} write={write} width={width} offset={offset:x} flags={flags:x}"
                        );
                        assert_eq!(frame.regs, expected.regs);
                        assert_eq!(frame.eip, expected.eip);
                        assert_eq!(frame.eflags() | 2, expected.effective_eflags());
                    } else {
                        assert_eq!((frame.exit, count), (lower::EXIT_INTERPRET, 0));
                        assert_eq!(frame.regs, cpu.regs);
                        assert_eq!(frame.eip, cpu.eip);
                        assert_eq!(frame.eflags() | 2, cpu.effective_eflags());
                        assert_eq!(native.data, original.data);
                        let fault = if segment == SegmentReg::Ss {
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
                        assert_eq!(oracle, Err(fault.clone()));
                        cpu.invalidate_translation_cache();
                        assert_eq!(Interpreter::step(&mut cpu, &mut native), Err(fault));
                        assert_eq!(cpu.regs, expected.regs);
                        assert_eq!(cpu.eip, expected.eip);
                    }
                    assert_eq!(native.data, memory.data);
                }
            }
        }
    }
}

#[test]
fn arm_jit_quiet_sti_and_popfd_continue_in_the_native_chain() {
    for old_if in [0, 1 << 9] {
        let mut sti = Fixture::new(&[0xfb]);
        let next = Fixture::at(&[0x40], 0x101); // INC EAX must complete before yielding.
        sti.table[lower::table_slot(0x101)] = [0x101, next.block as u32];
        let mut cpu = flag_cpu(0, 2 | STATUS | old_if);
        let mut expected = cpu.clone();
        let mut memory = Bytes(alloc::vec![0; 0x10000]);
        memory.0[0x100..0x102].copy_from_slice(&[0xfb, 0x40]);
        let (frame, ran) = sti.run_quiet(&mut cpu, 2, true);
        Interpreter::step(&mut expected, &mut memory).unwrap();
        Interpreter::step(&mut expected, &mut memory).unwrap();
        assert_eq!((frame.exit, ran), (lower::EXIT_BOUNDARY, 2));
        assert_eq!(frame.regs, expected.regs);
        assert_eq!(frame.eip, expected.eip);
        assert_eq!(frame.eflags() | 2, expected.effective_eflags());
        // No room for the following instruction: IF 0->1 retains the shadow.
        let (frame, ran) = sti.run_quiet(&mut cpu, 1, true);
        assert_eq!(ran, 1);
        assert_eq!(
            frame.exit,
            if old_if == 0 {
                lower::EXIT_SHADOW
            } else {
                lower::EXIT_BOUNDARY
            }
        );
    }
    let popfd = Fixture::new(&[0x9d, 0x40]);
    for old_if in [0, 1 << 9] {
        let mut cpu = flag_cpu(0, 2 | STATUS | old_if);
        cpu.regs[4] = 0x5000;
        let mut expected = cpu.clone();
        let mut memory = Bytes(alloc::vec![0; 0x10000]);
        memory.0[0x100..0x102].copy_from_slice(&[0x9d, 0x40]);
        memory.0[0x5000..0x5004].copy_from_slice(&(0x602u32 | STATUS).to_le_bytes());
        let mut native_memory = memory.0.clone();
        cpu.fill_cached_read_translation(
            LinearAddr(0x5000),
            BackingToken(native_memory.as_mut_ptr() as u64 + 0x5000),
            false,
        );
        let (frame, ran) = popfd.run_quiet(&mut cpu, 2, true);
        Interpreter::step(&mut expected, &mut memory).unwrap();
        Interpreter::step(&mut expected, &mut memory).unwrap();
        assert_eq!((frame.exit, ran), (lower::EXIT_BOUNDARY, 2));
        assert_eq!(frame.regs, expected.regs);
        assert_eq!(frame.eip, expected.eip);
        assert_eq!(frame.eflags() | 2, expected.effective_eflags());
        assert_eq!(native_memory, memory.0);
    }
}

const CHECKED_STACK_OPS: &[&[u8]] = &[
    &[0x50],                         // PUSH EAX
    &[0x54],                         // PUSH ESP
    &[0x68, 0x34, 0x12, 0x78, 0x56], // PUSH imm32
    &[0x6a, 0xff],                   // PUSH sign-extended imm8
    &[0xff, 0x74, 0x24, 4],          // PUSH [ESP+4]
    &[0x58],
    &[0x5c], // POP EAX, POP ESP
    &[0x60],
    &[0x61], // PUSHAD, POPAD
    &[0x9c],
    &[0x9d], // PUSHFD, POPFD
    &[0x06],
    &[0x0e],
    &[0x16],
    &[0x1e],
    &[0x0f, 0xa0],
    &[0x0f, 0xa8],
    &[0xe8, 0x7b, 0, 0, 0], // CALL 180h
    &[0xff, 0xd0],          // CALL EAX
    &[0xff, 0xd4],          // CALL ESP (target read before decrement)
    &[0xff, 0x14, 0x24],    // CALL [ESP]
    &[0xc3],
    &[0xc2, 16, 0], // RET, RET imm16
    &[0xc9],        // LEAVE
];

fn stack_memory(bytes: &[u8], stack: SegmentCache, offset: u32) -> SegmentMemory {
    SegmentMemory {
        code: bytes.to_vec(),
        // A valid POPFD value (TF clear), also a valid near transfer target.
        data: 0x246u32.to_le_bytes().repeat(8192 / 4),
        page: stack.base.wrapping_add(offset.wrapping_sub(32)) & !0xfff,
    }
}

fn checked_stack_case(bytes: &[u8], stack: SegmentCache, esp: u32, ebp: u32, fault: bool) {
    let fixture = Fixture::with_facts(bytes, 0x100, STACK32_FACT);
    let mut cpu = flag_cpu(3, 0x202);
    cpu.set_seg_cache(SegmentReg::Ss, stack);
    cpu.regs[0] = 0x180;
    cpu.regs[4] = esp;
    cpu.regs[5] = ebp;
    let mut expected = cpu.clone();
    let mut memory = stack_memory(bytes, stack, esp);
    let original = memory.data.clone();
    let mut native = memory.clone();
    for offset in [0u32, 4096] {
        cpu.fill_cached_write_translation(
            LinearAddr(memory.page.wrapping_add(offset)),
            BackingToken(native.data.as_mut_ptr() as u64 + u64::from(offset)),
            true,
        );
    }
    let (frame, count) = fixture.run_with(&mut cpu, 1);
    let oracle = Interpreter::step(&mut expected, &mut memory);
    if fault {
        assert_eq!(
            (frame.exit, count),
            (lower::EXIT_INTERPRET, 0),
            "{bytes:02x?}"
        );
        assert_eq!(frame.regs, cpu.regs);
        assert_eq!(frame.eip, cpu.eip);
        assert_eq!(frame.eflags() | 2, cpu.effective_eflags());
        assert_eq!(native.data, original);
        let error = crate::x86::StepError::StackFault {
            eip: 0x100,
            code: 0,
        };
        assert_eq!(oracle, Err(error.clone()), "{bytes:02x?} esp={esp:x}");
        cpu.invalidate_translation_cache();
        assert_eq!(Interpreter::step(&mut cpu, &mut native), Err(error));
        assert_eq!(cpu.regs, expected.regs);
        assert_eq!(cpu.eip, expected.eip);
        assert_eq!(cpu.effective_eflags(), expected.effective_eflags());
    } else {
        assert!(oracle.is_ok(), "{bytes:02x?} esp={esp:x}: {oracle:?}");
        assert_eq!(
            (frame.exit, count),
            (lower::EXIT_BOUNDARY, 1),
            "{bytes:02x?}"
        );
        assert_eq!(frame.regs, expected.regs, "{bytes:02x?}");
        assert_eq!(frame.eip, expected.eip, "{bytes:02x?}");
        assert_eq!(
            frame.eflags() | 2,
            expected.effective_eflags(),
            "{bytes:02x?}"
        );
    }
    assert_eq!(native.data, memory.data, "{bytes:02x?}");
}

#[test]
fn arm_jit_checked_stack_operations_match_ring3_interpreter() {
    for stack in [
        SegmentCache::new(0x013f, 0, 0xce60, 0xc0f6),
        SegmentCache::new(0x013f, 0x0020_0000, 0x000f_ffff, 0xc0f2),
    ] {
        for bytes in CHECKED_STACK_OPS {
            checked_stack_case(bytes, stack, 0x10100, 0x10120, false);
        }
    }
}

#[test]
fn arm_jit_checked_stack_limits_preserve_precise_fault_state() {
    let down = SegmentCache::new(0x013f, 0, 0xce60, 0xc0f6);
    for bytes in [
        &[0x50][..],
        &[0x54],
        &[0x60],
        &[0x9c],
        &[0x1e],
        &[0xe8, 0x7b, 0, 0, 0],
        &[0xff, 0xd0],
        &[0xff, 0xd4],
    ] {
        checked_stack_case(bytes, down, 0xce61, 0xce61, true);
    }
    // Last legal dword: the load succeeds and ESP wraps. One byte later
    // the segment range fails, before any native register or memory commit.
    for bytes in [&[0x58][..], &[0xc3], &[0xc2, 16, 0]] {
        checked_stack_case(bytes, down, u32::MAX - 3, u32::MAX - 3, false);
        checked_stack_case(bytes, down, u32::MAX - 2, u32::MAX - 2, true);
    }
    let up = SegmentCache::new(0x013f, 0x0020_0000, 0x000f_ffff, 0xc0f2);
    for bytes in [&[0x58][..], &[0xc3], &[0x61], &[0x9d], &[0xc9]] {
        checked_stack_case(bytes, up, 0x000f_fffe, 0x000f_fffe, true);
    }
    for bytes in [&[0x50][..], &[0x60]] {
        checked_stack_case(bytes, up, 0x0010_0001, 0x0010_0001, true);
    }
}

#[test]
fn arm_jit_word_stack_or_operand_ends_before_stack_operation() {
    for word_operand in [false, true] {
        let operations = if word_operand {
            // Immediate and relative encodings here have their word width.
            &[
                &[0x50][..],
                &[0x58],
                &[0x60],
                &[0x61],
                &[0x9c],
                &[0x9d],
                &[0x1e],
                &[0xe8, 0, 0],
                &[0xff, 0xd0],
                &[0xc3],
                &[0xc2, 8, 0],
                &[0xc9],
            ][..]
        } else {
            CHECKED_STACK_OPS
        };
        for op in operations {
            let mut bytes = alloc::vec![0x90];
            if word_operand {
                bytes.push(0x66);
            }
            bytes.extend_from_slice(op);
            let facts = if word_operand { STACK32_FACT } else { 0 };
            let decoded = decode::block_extended(&bytes, 0x100, 64);
            assert_eq!(decoded.byte_len, bytes.len());
            let compiled = lower::block(&decoded, facts).unwrap();
            assert_eq!(
                (compiled.instructions, compiled.bytes),
                (1, 1),
                "{bytes:02x?}"
            );
            let fixture = Fixture::publish(compiled);
            let mut cpu = flag_cpu(3, 0x202);
            let stack = SegmentCache::new(
                0x013f,
                0x0020_0000,
                0xffff,
                if word_operand { 0xc0f2 } else { 0x80f2 },
            );
            cpu.set_seg_cache(SegmentReg::Ss, stack);
            cpu.regs[0] = 0x180;
            cpu.regs[4] = if word_operand { 0xed00 } else { 0xabcd_ed00 };
            cpu.regs[5] = if word_operand { 0xed20 } else { 0xabcd_ed20 };
            let mut expected = cpu.clone();
            let mut memory = stack_memory(&bytes, stack, 0xed00);
            let mut native = memory.clone();
            let (frame, count) = fixture.run_with(&mut cpu, 2);
            assert_eq!(
                (frame.exit, count, frame.eip),
                (lower::EXIT_BOUNDARY, 1, 0x101)
            );
            assert_eq!(frame.regs, cpu.regs);
            assert_eq!(frame.eflags() | 2, cpu.effective_eflags());
            cpu.regs = frame.regs;
            cpu.eip = frame.eip;
            cpu.replace_eflags(frame.eflags() | 2);
            Interpreter::step(&mut expected, &mut memory).unwrap(); // NOP
            let oracle = Interpreter::step(&mut expected, &mut memory);
            let fallback = Interpreter::step(&mut cpu, &mut native);
            assert_eq!(fallback, oracle, "{bytes:02x?}");
            if !word_operand && matches!(*op, [0xff, 0x74, 0x24, 4] | [0xff, 0x14, 0x24]) {
                // SS.B selects SP for the implicit stack access, but the
                // explicit memory operand still uses the full 32-bit ESP.
                assert_eq!(
                    oracle,
                    Err(crate::x86::StepError::StackFault {
                        eip: 0x101,
                        code: 0,
                    }),
                    "{bytes:02x?}"
                );
            } else {
                assert!(oracle.is_ok(), "{bytes:02x?}: {oracle:?}");
            }
            assert_eq!(cpu.regs, expected.regs, "{bytes:02x?}");
            assert_eq!(cpu.eip, expected.eip);
            assert_eq!(cpu.effective_eflags(), expected.effective_eflags());
            assert_eq!(native.data, memory.data);
        }
    }
}
