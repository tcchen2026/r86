//! Run the same x86 bytes through the interpreter and one native block. The
//! interpreter is the oracle for registers, all EFLAGS bits and EIP; inputs
//! cover carries, overflow, parity, partial registers and untouched flags.
//! Run under `qemu-arm -cpu cortex-a7`; `asm::tests` pins the encodings to
//! Armv8-M Mainline.

use super::arena::Arena;
use super::lower::{self, Frame, STACK_FACT};
use crate::x86::jit::decode;
use crate::x86::{BatchStop, Bus, Cpu, Interpreter, MemError, PhysAddr};
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
        let decoded = decode::block(bytes, 0x100, 64);
        assert_eq!(decoded.byte_len, bytes.len(), "decode {bytes:02x?}");
        let compiled = lower::block(&decoded, ALL_FACTS)
            .unwrap_or_else(|| panic!("native block {bytes:02x?}"));
        assert_eq!(
            compiled.instructions as usize,
            decoded.instructions.len(),
            "partial block {bytes:02x?}"
        );
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
        let mut frame = Frame {
            regs: cpu.regs,
            eip: cpu.eip,
            remaining: budget,
            table: self.table.as_ptr() as usize as u32,
            dispatch: self.dispatch as u32,
            ..Frame::default()
        };
        frame.set_eflags(cpu.effective_eflags());
        let (tlb, state) = cpu.translation_cache.jit_view();
        frame.tlb = tlb as usize as u32;
        frame.tlb_state = state as u32;
        // SAFETY: test-owned arena, table and frame; the TLB view is empty,
        // so memory instructions take their precise exits.
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
