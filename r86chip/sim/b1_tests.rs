use super::{
    memory::Memory,
    rv32::{Cause, Core},
};
use r86::chipjit::compile::{self, Frame};
use r86::x86::{Bus, Cpu, Interpreter, MemError, PhysAddr};

struct Ram(Vec<u8>);
impl Bus for Ram {
    fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
        self.0
            .get(addr.0 as usize)
            .copied()
            .ok_or(MemError::OutOfRange { addr })
    }
    fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError> {
        self.0
            .get(addr.0 as usize)
            .map(|_| ())
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
fn words(memory: &mut Memory, at: u32, data: &[u32]) {
    memory
        .write(
            at,
            &data
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect::<Vec<_>>(),
        )
        .unwrap();
}
fn differential(bytes: &[u8], regs: [u32; 8], flags: u32, limit: usize) {
    let block = compile::block(bytes, 0x1000, limit).expect("nonempty native block");
    let mut oracle = Cpu::new();
    oracle.enter_protected_flat(0x1000, 0);
    oracle.regs = regs;
    oracle.replace_eflags(flags);
    let initial_flags = oracle.effective_eflags();
    let mut bus = Ram(vec![0; 65536]);
    bus.0[0x1000..0x1000 + bytes.len()].copy_from_slice(bytes);
    for _ in 0..block.instructions {
        Interpreter::step(&mut oracle, &mut bus).unwrap();
    }
    let mut native = Core::new(0x1000);
    let mut memory = Memory::new(65536);
    words(
        &mut memory,
        0x1000,
        &[
            0x7b | 31 << 7 | 7 << 12 | 11 << 15, // XWRITE bank1.x31, a1
            0x7b | 4 << 12 | 10 << 15,           // XENTER a0
            0x00100073,
        ],
    );
    words(&mut memory, 0x2000, &block.words);
    words(&mut memory, 0x8000, &regs);
    words(&mut memory, 0x8020, &[initial_flags, 0x1000, 0]);
    native.x[10] = 0x2000;
    native.x[11] = 0x8000;
    let original_bank = native.x;
    assert_eq!(
        native.run(&mut memory, 10000).unwrap().cause,
        Cause::Breakpoint
    );
    assert_eq!(native.x, original_bank, "runtime bank clobbered");
    assert_eq!(native.bank_enters, 1);
    let result = Frame {
        regs: core::array::from_fn(|n| memory.read(0x8000 + n as u32 * 4, 32)),
        flags: memory.read(0x8020, 32),
        eip: memory.read(0x8024, 32),
        executed: memory.read(0x8028, 32),
    };
    assert_eq!(
        result,
        Frame {
            regs: oracle.regs,
            flags: oracle.effective_eflags(),
            eip: oracle.eip,
            executed: block.instructions
        },
        "code={bytes:02x?} input={regs:08x?} flags={initial_flags:x}"
    );
}
fn random(seed: &mut u32) -> u32 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    *seed
}

#[test]
fn b1_native_register_lowering_matches_shared_interpreter() {
    let mut programs: Vec<Vec<u8>> = vec![
        vec![0x90],
        vec![0xb8, 0xff, 0xff, 0xff, 0xff],
        vec![0x66, 0xb8, 0x34, 0x12],
        vec![0xb4, 0xa5],
        vec![0x89, 0xc8],
        vec![0x88, 0xe0],
        vec![0x88, 0xc4],
        vec![0x8d, 0x44, 0x8b, 0xfc],
        vec![0x66, 0x8d, 0x44, 0x8b, 0xfc],
        vec![0xf7, 0xd0],
        vec![0xf7, 0xd8],
        vec![0xf6, 0xd4],
        vec![0xf6, 0xdc],
        vec![0x40],
        vec![0x48],
        vec![0x66, 0x40],
        vec![0xfe, 0xc4],
        vec![0xfe, 0xcc],
        vec![0x85, 0xc8],
        vec![0x84, 0xe0],
        vec![0x83, 0xc0, 0xff, 0x89, 0xc3, 0x43, 0x39, 0xc3, 0x75, 0xf7],
    ];
    for op in [
        0x00, 0x01, 0x02, 0x03, 0x08, 0x09, 0x10, 0x11, 0x12, 0x13, 0x18, 0x19, 0x1a, 0x1b, 0x20,
        0x21, 0x28, 0x29, 0x30, 0x31, 0x38, 0x39,
    ] {
        programs.push(vec![op, 0xc8]);
        programs.push(vec![op, 0xe0]);
        if op & 1 != 0 {
            programs.push(vec![0x66, op, 0xc8]);
        }
    }
    let edges = [
        0, 1, 0x7f, 0x80, 0xff, 0x100, 0x7fff, 0x8000, 0xffff, 0x10000, 0x7fffffff, 0x80000000,
        0xffffffff,
    ];
    let mut seed = 0x87344;
    for program in &programs {
        for &a in &edges {
            for &b in &edges {
                let mut regs = core::array::from_fn(|_| random(&mut seed));
                regs[0] = a;
                regs[1] = b;
                differential(program, regs, 0x202 | (random(&mut seed) & 0x8d5), 16);
            }
        }
    }
}

#[test]
fn b1_native_branches_match_all_status_combinations() {
    for condition in 0..16 {
        for bits in 0..64 {
            let flags = [0, 2, 4, 6, 7, 11]
                .iter()
                .enumerate()
                .fold(0x202, |flags, (n, shift)| {
                    flags | ((bits >> n) & 1) << shift
                });
            differential(&[0x70 + condition, 0x80], [0; 8], flags, 16);
        }
    }
    differential(&[0xe9, 0xfb, 0xef, 0xff, 0xff], [0; 8], 0x202, 16);
}

#[test]
fn b1_carry_chains_cross_partial_updates_and_plain_rv_operations() {
    // ADD sets carry; INC and MOV must preserve it; ADC/SBB consume the most
    // recently committed status. Finish with each condition to observe flags.
    let mut seed = 0x578abcd;
    for value in [0, 1, 0xff, 0xffff, 0x7fffffff, 0x80000000, u32::MAX] {
        for condition in 0..16 {
            for prefix in [&[][..], &[0x66][..]] {
                let mut bytes = vec![
                    0xb8, 0xff, 0xff, 0xff, 0xff, 0x83, 0xc0, 1, 0x41, 0x89, 0xff,
                ];
                bytes.extend_from_slice(prefix);
                bytes.extend_from_slice(&[0x83, 0xd2, 0, 0x80, 0xdc, 0, 0x70 + condition, 0x10]);
                let mut regs = core::array::from_fn(|_| random(&mut seed));
                regs[2] = value;
                differential(&bytes, regs, 0x203, 16);
            }
        }
    }
}

#[test]
fn b1_sign_extended_immediates_use_guest_width_for_carry_and_borrow() {
    for group in [0u8, 1, 2, 3, 4, 5, 6, 7] {
        for immediate in [0x80, 0xff, 0, 1, 0x7f] {
            for value in [
                0u32,
                1,
                0x7f,
                0x80,
                0xff,
                0x7fff,
                0x8000,
                0xff80,
                0xffff,
                u32::MAX,
            ] {
                let regs = [value, 0, 0, 0, 0, 0, 0, 0];
                for flags in [2, 0x8d7] {
                    differential(&[0x66, 0x83, 0xc0 | group << 3, immediate], regs, flags, 16);
                    differential(&[0x83, 0xc0 | group << 3, immediate], regs, flags, 16);
                    differential(&[0x80, 0xc0 | group << 3, immediate], regs, flags, 16);
                }
            }
        }
    }
}

#[test]
fn b1_native_stops_before_memory_unsupported_and_instruction_limits() {
    for bytes in [
        &[0x8b, 0x00][..],
        &[0x89, 0x00],
        &[0xcd, 0x80],
        &[0xfb],
        &[0xf4],
        &[0xd1, 0xe0],
    ] {
        assert!(compile::block(bytes, 0x1000, 16).is_none(), "{bytes:02x?}");
        let mut prefix = vec![0x40];
        prefix.extend_from_slice(bytes);
        let block = compile::block(&prefix, 0x1000, 16).unwrap();
        assert_eq!((block.instructions, block.source_bytes), (1, 1));
        differential(&prefix, [0; 8], 0x202, 16);
    }
    assert!(compile::block(&[0x90], 0x1000, 0).is_none());
    assert_eq!(
        compile::block(&[0x40; 32], 0x1000, 100)
            .unwrap()
            .instructions,
        16
    );
    differential(&[0x40; 32], [0; 8], 0x203, 3);
}

#[test]
fn b1_moves_preserve_other_lanes_and_flags_for_all_register_aliases() {
    let regs = [
        0x807f12a5, 0xfecdbc01, 0x01020304, 0xffeeddcc, 0x12345678, 0x87654321, 0x7fff8000,
        0x0000ffff,
    ];
    for dst in 0..8 {
        for src in 0..8 {
            for flags in [2, 0x8d7] {
                differential(&[0x88, 0xc0 | src << 3 | dst], regs, flags, 16);
                differential(&[0x66, 0x89, 0xc0 | src << 3 | dst], regs, flags, 16);
                differential(&[0x89, 0xc0 | src << 3 | dst], regs, flags, 16);
            }
        }
    }
}

#[test]
fn b1_scaled_lea_preserves_flags_wraps_and_reads_old_aliases() {
    let regs = [
        0x80000001, 0x7fffffff, 0xffff0001, 0x12345678, 0xabcdefff, 0xfffffff0, 0x01020304,
        0xdeadbeef,
    ];
    for base in 0..8 {
        for index in 0..8 {
            for scale in 0..4 {
                for dst in [base, index] {
                    let instruction = [0x8d, 0x44 | dst << 3, scale << 6 | index << 3 | base, 0x80];
                    differential(&instruction, regs, 0x8d7, 16);
                    let mut narrow = vec![0x66];
                    narrow.extend_from_slice(&instruction);
                    differential(&narrow, regs, 0x8d7, 16);
                }
            }
        }
    }
    // Absent base with scaled index, and absolute disp32; neither may read a
    // nonexistent guest base or lose the upper half of a word destination.
    for bytes in [
        &[0x8d, 0x04, 0xc5, 0xfc, 0xff, 0xff, 0xff][..],
        &[0x66, 0x8d, 0x05, 0xff, 0xff, 0xff, 0xff],
    ] {
        differential(bytes, regs, 0x8d7, 16);
    }
}
