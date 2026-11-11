use super::{
    memory::Memory,
    rv32::{Cause, Core},
};
use r86::chipjit::compile;
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
const BACKING: usize = 0x10000;
fn word(core: &mut Core, memory: &mut Memory, instruction: u32, a: u32, b: u32) {
    words(memory, 0x7fc, &[0x0000100f, instruction]);
    core.pc = 0x7fc;
    core.x[1] = a;
    core.x[2] = b;
    assert!(core.step(memory).unwrap().is_none()); // FENCE.I publishes the replaced setup word.
    let trap = core.step(memory).unwrap();
    assert!(
        trap.is_none(),
        "word={instruction:08x} a={a:08x} b={b:08x}: {trap:?}"
    );
}
fn r(op: u32, f: u32, k: u32, rd: u32, a: u32, b: u32) -> u32 {
    op | rd << 7 | f << 12 | a << 15 | b << 20 | k << 25
}
fn i(op: u32, f: u32, rd: u32, a: u32, imm: u32) -> u32 {
    op | rd << 7 | f << 12 | a << 15 | (imm & 4095) << 20
}
fn native_fixture(
    block: &compile::Block,
    regs: [u32; 8],
    flags: u32,
    ram: &[u8],
    bases: [u32; 6],
    ticks: u32,
) -> (Core, Memory) {
    let mut core = Core::new(0x1000);
    let mut memory = Memory::new(0x20000);
    memory.ram[BACKING..BACKING + ram.len()].copy_from_slice(ram);
    words(&mut memory, 0x2000, &block.words);
    word(
        &mut core,
        &mut memory,
        r(0x2b, 4, 0, 0, 1, 2),
        BACKING as u32,
        0x20000,
    );
    word(&mut core, &mut memory, r(0x2b, 5, 0, 0, 1, 2), 1, 1);
    for (seg, base) in bases.into_iter().enumerate() {
        word(
            &mut core,
            &mut memory,
            r(0x2b, 6, 3, seg as u32, 1, 2),
            base,
            u32::MAX,
        );
    }
    for page in (0..0x10000).step_by(4096) {
        word(
            &mut core,
            &mut memory,
            r(0x2b, 1, 0, 0, 1, 2),
            page,
            (BACKING as u32 + page) | 3,
        );
    }
    for (n, value) in regs.into_iter().enumerate() {
        word(
            &mut core,
            &mut memory,
            i(0x7b, 7, n as u32 + 8, 1, 0),
            value,
            0,
        );
    }
    word(&mut core, &mut memory, i(0x7b, 7, 16, 1, 0), 0x1000, 0);
    word(&mut core, &mut memory, i(0x0b, 4, 0, 1, 0), flags, 0);
    word(&mut core, &mut memory, i(0x6b, 5, 0, 1, 0), ticks, 0);
    words(&mut memory, 0x1000, &[i(0x7b, 4, 0, 10, 0), 0x00100073]);
    core.pc = 0x1000;
    core.x[10] = 0x2000;
    (core, memory)
}
fn differential(bytes: &[u8], regs: [u32; 8], flags: u32, limit: usize) {
    differential_memory(bytes, regs, flags, limit, &[], [0; 6]);
}
fn differential_memory(
    bytes: &[u8],
    regs: [u32; 8],
    flags: u32,
    limit: usize,
    data: &[(usize, u32)],
    bases: [u32; 6],
) {
    let block = compile::block(bytes, 0x1000, limit).expect("nonempty native block");
    let mut oracle = Cpu::new();
    oracle.enter_protected_flat(0x1000, 0);
    oracle.regs = regs;
    oracle.replace_eflags(flags);
    for (n, seg) in [
        r86::x86::SegmentReg::Es,
        r86::x86::SegmentReg::Cs,
        r86::x86::SegmentReg::Ss,
        r86::x86::SegmentReg::Ds,
        r86::x86::SegmentReg::Fs,
        r86::x86::SegmentReg::Gs,
    ]
    .into_iter()
    .enumerate()
    {
        let mut cache = oracle.seg(seg);
        cache.base = bases[n];
        oracle.set_seg_cache(seg, cache);
    }
    let initial_flags = oracle.effective_eflags();
    let mut bus = Ram(vec![0; 65536]);
    bus.0[0x1000..0x1000 + bytes.len()].copy_from_slice(bytes);
    for &(at, value) in data {
        bus.0[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    let (mut native, mut memory) = native_fixture(
        &block,
        regs,
        initial_flags,
        &bus.0,
        bases,
        block.instructions,
    );
    for _ in 0..block.instructions {
        Interpreter::step(&mut oracle, &mut bus).unwrap();
    }
    let original_bank = native.x;
    assert_eq!(
        native.run(&mut memory, 100000).unwrap().cause,
        Cause::Breakpoint,
        "code={bytes:02x?}"
    );
    assert_eq!(native.x, original_bank, "runtime bank clobbered");
    assert_eq!(native.bank_enters, 1);
    let banks = native.register_banks();
    word(
        &mut native,
        &mut memory,
        i(0x0b, 5, 3, 1, 0),
        initial_flags,
        0,
    );
    assert_eq!(
        (&banks[1][8..16], banks[1][16], native.x[3]),
        (&oracle.regs[..], oracle.eip, oracle.effective_eflags()),
        "code={bytes:02x?} input={regs:08x?} flags={initial_flags:x}"
    );
    assert_eq!(
        &memory.ram[BACKING..BACKING + 65536],
        &bus.0,
        "guest memory: {bytes:02x?}"
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
fn b1_native_stops_before_unsupported_and_instruction_limits() {
    for bytes in [
        &[0xf7, 0xd8][..],
        &[0xf7, 0xd0],
        &[0xcd, 0x80],
        &[0xfb],
        &[0xf4],
        &[0xd0, 0xe0],
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
        32
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

#[test]
fn b1_memory_mov_extend_alu_and_stack_match_interpreter() {
    let mut programs = vec![
        vec![0x8b, 0x00],
        vec![0x89, 0x18],
        vec![0x88, 0x20],
        vec![0x8a, 0x20],
        vec![0x66, 0x89, 0x08],
        vec![0x66, 0x8b, 0x08],
        vec![0xc7, 0x00, 0xff, 0xff, 0xff, 0xff],
        vec![0xc6, 0x00, 0x80],
        vec![0x66, 0xc7, 0x00, 0x34, 0x12],
        vec![0x8b, 0x44, 0x8b, 0xfc],
        vec![0x89, 0x44, 0x8b, 0xfc],
        vec![0x8b, 0x85, 0x00, 0x10, 0, 0],
        vec![0x89, 0x85, 0x00, 0x10, 0, 0],
        vec![0xff, 0x00],
        vec![0xff, 0x08],
        vec![0xfe, 0x00],
        vec![0xfe, 0x08],
        vec![0x66, 0xff, 0x00],
        vec![0x66, 0xff, 0x08],
        vec![0xff, 0x30],
        vec![0x68, 0xff, 0xff, 0xff, 0xff],
        vec![0x6a, 0x80],
        vec![0xff, 0x74, 0x24, 0x04], // PUSH [ESP+4] reads before changing ESP.
    ];
    for op in [0xb6, 0xb7, 0xbe, 0xbf] {
        programs.push(vec![0x0f, op, 0x08]);
        programs.push(vec![0x66, 0x0f, op, 0x08]);
        programs.push(vec![0x0f, op, 0xcc]); // high/word source alias
    }
    for group in 0..8 {
        for (op, modrm) in [
            (group * 8 + 1, 0x08),
            (group * 8 + 3, 0x08),
            (group * 8, 0x20),
            (group * 8 + 2, 0x20),
        ] {
            programs.push(vec![op, modrm]);
            if op & 1 != 0 {
                programs.push(vec![0x66, op, modrm]);
            }
        }
        programs.push(vec![0x83, group << 3, 0xff]);
        programs.push(vec![0x66, 0x83, group << 3, 0xff]);
        programs.push(vec![0x80, group << 3, 0xff]);
    }
    programs.extend([
        vec![0x85, 0x08],
        vec![0x84, 0x20],
        vec![0xf7, 0x00, 0xff, 0, 0, 0],
        vec![0x66, 0xf7, 0x00, 0xff, 0],
        vec![0xf6, 0x00, 0x80],
    ]);
    for reg in 0..8 {
        programs.push(vec![0x50 + reg]);
        programs.push(vec![0x58 + reg]);
    }
    for value in [0, 1, 0x80, 0xffff, 0x80000000, 0xffffffff] {
        for flags in [0x202, 0xad7] {
            for program in &programs {
                let regs = [0x4080, 7, value, 0x4200, 0x4800, 0x4300, value, value];
                let data = [
                    (0x4080, value),
                    (0x4800, value),
                    (0x4804, value),
                    (0x4218, value),
                    (0x5300, value),
                ];
                differential_memory(program, regs, flags, 16, &data, [0; 6]);
            }
        }
    }
    // Other-segment XLD/XSW and based DS/SS use the same guest offset rules.
    for (prefix, seg) in [(0x26, 0), (0x36, 2), (0x3e, 3), (0x64, 4), (0x65, 5)] {
        let mut bases = [0; 6];
        bases[seg] = 0x1000;
        for program in [
            [prefix, 0x8b, 0x08],
            [prefix, 0x89, 0x08],
            [prefix, 0x01, 0x08],
            [prefix, 0x02, 0x20],
        ] {
            differential_memory(
                &program,
                [0x4080, 0xfeca1234, 0, 0, 0x4800, 0, 0, 0],
                0xad7,
                16,
                &[(0x5080, 0x817fff80)],
                bases,
            );
        }
    }
}

#[test]
fn b1_dword_shifts_rotates_and_setcc_match_interpreter() {
    for group in [0, 1, 4, 5, 7] {
        for count in [0, 1, 2, 7, 16, 31, 32, 33, 255] {
            for value in [0, 1, 0x80000000, 0x7fffffff, 0xffffffff] {
                for flags in [0x202, 0xad7] {
                    let regs = [value, count, 0, 0, 0, 0, 0, 0];
                    differential(&[0xc1, 0xc0 | group << 3, count as u8], regs, flags, 16);
                    differential(&[0xd3, 0xc0 | group << 3], regs, flags, 16);
                }
            }
        }
    }
    for cc in 0..16 {
        for state in 0..64 {
            let flags = [0, 2, 4, 6, 7, 11]
                .into_iter()
                .enumerate()
                .fold(0x202, |f, (n, bit)| f | ((state >> n) & 1) << bit);
            for reg in 0..8 {
                differential(&[0x0f, 0x90 + cc, 0xc0 | reg], [0xaabbccdd; 8], flags, 16);
            }
        }
    }
}

#[test]
fn b1_call_jump_return_and_stack_aliases_match_interpreter() {
    for bytes in [
        &[0xe8, 0x40, 0, 0, 0][..],
        &[0xff, 0xd0],
        &[0xff, 0xd4],
        &[0xff, 0x10],
        &[0xff, 0x54, 0x24, 4],
        &[0xc3],
        &[0xc2, 0x34, 0x12],
        &[0xff, 0xe0],
        &[0xff, 0x20],
    ] {
        differential_memory(
            bytes,
            [0x4080, 0, 0, 0, 0x4800, 0, 0, 0],
            0xad7,
            16,
            &[(0x4080, 0x2000), (0x4800, 0x3000), (0x4804, 0x4000)],
            [0; 6],
        );
    }
    // CALL target; ADD EAX,2; HLT; target: ADD EAX,3; RET.
    // The hardware return stack returns into the caller's adjacent stable stub,
    // which dispatches the return block without copying any guest registers.
    let code = [0xe8, 4, 0, 0, 0, 0x83, 0xc0, 2, 0xf4, 0x83, 0xc0, 3, 0xc3];
    let entry = compile::block(&code, 0x1000, 16).unwrap();
    let target = compile::block(&code[9..], 0x1009, 16).unwrap();
    let ret = compile::block(&code[5..9], 0x1005, 16).unwrap();
    let regs = [7, 0, 0, 0, 0x4800, 0, 0, 0];
    let mut ram = vec![0; 65536];
    ram[0x1000..0x1000 + code.len()].copy_from_slice(&code);
    let (mut native, mut memory) = native_fixture(&entry, regs, 0x203, &ram, [0; 6], 4);
    words(&mut memory, 0x6000, &target.words);
    words(&mut memory, 0x7000, &ret.words);
    word(
        &mut native,
        &mut memory,
        r(0x6b, 1, 0, 0, 1, 2),
        0x1009,
        0x6000,
    );
    word(
        &mut native,
        &mut memory,
        r(0x6b, 1, 0, 0, 1, 2),
        0x1005,
        0x7000,
    );
    native.pc = 0x1000;
    assert_eq!(
        native.run(&mut memory, 1000).unwrap().cause,
        Cause::Breakpoint
    );
    assert_eq!(native.bank_enters, 1);
    let mut oracle = Cpu::new();
    oracle.enter_protected_flat(0x1000, 0x4800);
    oracle.regs = regs;
    oracle.replace_eflags(0x203);
    let mut bus = Ram(ram);
    for _ in 0..4 {
        Interpreter::step(&mut oracle, &mut bus).unwrap();
    }
    let banks = native.register_banks();
    word(&mut native, &mut memory, i(0x0b, 5, 3, 1, 0), 0x203, 0);
    assert_eq!(
        (&banks[1][8..16], banks[1][16], native.x[3]),
        (&oracle.regs[..], oracle.eip, oracle.effective_eflags())
    );
    assert_eq!(&memory.ram[BACKING..BACKING + 65536], &bus.0);
}

#[test]
fn b1_tick_and_service_keep_precise_instruction_boundaries() {
    let code = [0x40, 0x01, 0x0b, 0x42]; // INC; ADD [EBX],ECX; INC
    let block = compile::block(&code, 0x1000, 16).unwrap();
    let regs = [0x7fffffff, 1, 7, 0x4080, 0x4800, 0, 0, 0];
    for ticks in [0, 2] {
        let (mut core, mut memory) =
            native_fixture(&block, regs, 0x203, &[0; 65536], [0; 6], ticks);
        assert_eq!(
            core.run(&mut memory, 1000).unwrap().cause,
            Cause::Breakpoint
        );
        assert_eq!(&core.register_banks()[1][8..16], &regs);
        word(&mut core, &mut memory, r(0x7b, 6, 1, 3, 0, 0), 0, 0);
        assert_eq!(core.x[3], compile::TICK);
        word(&mut core, &mut memory, r(0x7b, 6, 1, 3, 1, 0), 0, 0);
        assert_eq!(core.x[3], 0x2000);
    }
    let (mut core, mut memory) = native_fixture(&block, regs, 0x203, &[0; 65536], [0; 6], 3);
    // Read-only data proof forces the RMW load to request service before either
    // its flags or store commit. The side table identifies instruction 1.
    word(
        &mut core,
        &mut memory,
        r(0x2b, 1, 0, 0, 1, 2),
        0x4000,
        0x14001,
    );
    words(&mut memory, 0x9000, &[i(0x7b, 5, 0, 0, compile::SERVICE)]);
    word(&mut core, &mut memory, r(0x2b, 7, 0, 0, 1, 0), 0x9000, 0);
    core.pc = 0x1000;
    assert_eq!(
        core.run(&mut memory, 1000).unwrap().cause,
        Cause::Breakpoint
    );
    let bank = core.register_banks()[1];
    assert_eq!(bank[8], 0x80000000);
    assert_eq!(bank[10], 7);
    assert_eq!(memory.read(0x14080, 32), 0);
    word(&mut core, &mut memory, r(0x2b, 7, 1, 3, 0, 0), 0, 0);
    let site = block.site(core.x[3] - 0x2000);
    assert_eq!((site.eip, site.index), (0x1001, 1));
    word(&mut core, &mut memory, i(0x0b, 5, 3, 1, 0), 0x203, 0);
    assert_eq!(core.x[3] & 0x8d5, 0x895);
}
