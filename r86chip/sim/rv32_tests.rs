use super::{
    memory::Memory,
    rv32::{self, Cause, Core},
};

fn put(m: &mut Memory, addr: u32, words: &[u32]) {
    m.write(
        addr,
        &words
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<_>>(),
    )
    .unwrap();
}
fn r(d: u32, a: u32, b: u32, f: u32, k: u32) -> u32 {
    0x33 | d << 7 | f << 12 | a << 15 | b << 20 | k << 25
}
fn i(op: u32, d: u32, a: u32, f: u32, imm: i32) -> u32 {
    op | d << 7 | f << 12 | a << 15 | (imm as u32 & 4095) << 20
}
fn branch(a: u32, b: u32, f: u32, offset: i32) -> u32 {
    let v = offset as u32;
    0x63 | ((v >> 11) & 1) << 7
        | ((v >> 1) & 15) << 8
        | f << 12
        | a << 15
        | b << 20
        | ((v >> 5) & 63) << 25
        | ((v >> 12) & 1) << 31
}
fn store(a: u32, b: u32, f: u32, offset: i32) -> u32 {
    let v = offset as u32;
    0x23 | (v & 31) << 7 | f << 12 | a << 15 | b << 20 | ((v >> 5) & 127) << 25
}
fn one(word: u32, a: u32, b: u32) -> Core {
    let mut m = Memory::new(4096);
    put(&mut m, 0x100, &[word]);
    let mut c = Core::new(0x100);
    c.x[17] = a;
    c.x[31] = b;
    assert_eq!(c.step(&mut m).unwrap(), None, "{word:08x}");
    c
}

#[test]
fn b1_contexts_preserve_runtime_and_use_ordinary_rv_semantics() {
    let mut memory = Memory::new(4096);
    let mut core = Core::new(0x100);
    put(
        &mut memory,
        0x100,
        &[
            i(0x13, 1, 0, 0, 0x200),
            i(0x13, 2, 0, 0, 123),
            i(0x7b, 1, 2, 7, 0),  // XWRITE bank1.x1, x2
            i(0x7b, 0, 2, 7, 0),  // bank1.x0 remains zero
            i(0x7b, 0, 1, 4, 0),  // XENTER x1
            i(0x7b, 3, 1, 6, 0),  // XREAD x3, bank1.x1
            i(0x7b, 4, 0, 6, 32), // XINFO reason
            i(0x7b, 5, 1, 6, 32), // XINFO native exit PC
            i(0x7b, 6, 2, 6, 32), // XINFO profile
            i(0x7b, 7, 0, 6, 0),  // XREAD x7, bank1.x0
            0x00100073,
        ],
    );
    put(
        &mut memory,
        0x200,
        &[
            i(0x13, 1, 1, 0, 5),
            i(0x13, 0, 1, 0, 9),
            i(0x7b, 0, 0, 5, 17),
        ],
    );
    assert_eq!(core.run(&mut memory, 100).unwrap().cause, Cause::Breakpoint);
    assert_eq!(
        &core.x[..8],
        &[0, 0x200, 123, 128, 17, 0x20c, 0x4231000a, 0]
    );
    assert_eq!(core.bank_enters, 1);
}

#[test]
fn b1_context_faults_preserve_state() {
    for word in [
        i(0x7b, 0, 1, 1, 0),  // reserved XEAINFO rs1
        i(0x7b, 0, 0, 5, 0),  // XLEAVE from runtime
        i(0x7b, 0, 1, 4, 1),  // reserved XENTER operand
        i(0x7b, 1, 3, 6, 32), // invalid XINFO selector
        i(0x7b, 1, 0, 6, 1),  // nonzero rs2
        i(0x7b, 1, 0, 7, 1),  // nonzero rs2
    ] {
        let mut m = Memory::new(4096);
        put(&mut m, 0x100, &[word]);
        let mut c = Core::new(0x100);
        c.x[1] = 0x200;
        let before = c.x;
        assert_eq!(
            c.step(&mut m).unwrap().unwrap().cause,
            Cause::IllegalInstruction
        );
        assert_eq!(c.x, before);
        assert_eq!(c.pc, 0x100);
        assert_eq!(c.bank_enters, 0);
    }
    let mut m = Memory::new(4096);
    put(&mut m, 0x100, &[i(0x7b, 0, 1, 4, 0)]);
    put(&mut m, 0x200, &[i(0x7b, 0, 1, 4, 0)]);
    let mut c = Core::new(0x100);
    c.x[1] = 0x202;
    assert_eq!(
        c.step(&mut m).unwrap().unwrap().cause,
        Cause::InstructionMisaligned
    );
    assert_eq!(c.x[1], 0x202);
    assert_eq!(c.bank_enters, 0);
    c.x[1] = 0x200;
    c.step(&mut m).unwrap();
    let bank = c.x;
    assert_eq!(
        c.step(&mut m).unwrap().unwrap().cause,
        Cause::IllegalInstruction
    );
    assert_eq!(c.x, bank);
    assert_eq!(c.bank_enters, 1);
}

#[test]
fn rv32_integer_registers_immediates_and_zero() {
    // Exercise high register indices, including x31.
    let a = 0x80000005;
    for (f, k, expected) in [
        (0, 0, 0x80000028),
        (0, 32, 0x7fffffe2),
        (1, 0, 0x28),
        (2, 0, 1),
        (3, 0, 0),
        (4, 0, 0x80000026),
        (5, 0, 0x10000000),
        (5, 32, 0xf0000000),
        (6, 0, 0x80000027),
        (7, 0, 1),
    ] {
        assert_eq!(one(r(30, 17, 31, f, k), a, 35).x[30], expected);
    }
    for (f, imm, expected) in [
        (0, -1, 0x80000004),
        (2, -1, 1),
        (3, -1, 1),
        (4, -1, 0x7ffffffa),
        (6, -1, 0xffffffff),
        (7, -1, a),
        (1, 3, 0x28),
        (5, 3, 0x10000000),
        (5, 1027, 0xf0000000),
    ] {
        assert_eq!(one(i(0x13, 30, 17, f, imm), a, 0).x[30], expected);
    }
    assert_eq!(one(0xdeadb000 | 30 << 7 | 0x37, 0, 0).x[30], 0xdeadb000);
    assert_eq!(one(0xdeadb000 | 30 << 7 | 0x17, 0, 0).x[30], 0xdeadb100);
    assert_eq!(one(i(0x13, 0, 17, 0, 77), a, 0).x[0], 0);
    for dest in 1..32 {
        assert_eq!(
            one(i(0x13, dest, 0, 0, -13), 0, 0).x[dest as usize],
            (-13i32) as u32
        );
    }
}

#[test]
fn rv32_m_all_forms_signed_edges_and_zero_divisors() {
    let mut values = vec![
        0, 1, 2, 3, 0x7fffffff, 0x80000000, 0x80000001, 0xfffffffe, 0xffffffff,
    ];
    let mut seed = 9283u32;
    for _ in 0..40 {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        values.push(seed);
    }
    // Wide integer reference avoids sharing the simulator's wrapping intrinsics.
    for &a in &values {
        for &b in &values {
            let sa = a as i32 as i128;
            let sb = b as i32 as i128;
            let expected = [
                (a as u128 * b as u128) as u32,
                ((sa * sb) >> 32) as u32,
                ((sa * b as i128) >> 32) as u32,
                ((a as u128 * b as u128) >> 32) as u32,
                if b == 0 { u32::MAX } else { (sa / sb) as u32 },
                if b == 0 {
                    u32::MAX
                } else {
                    (a as u128 / b as u128) as u32
                },
                if b == 0 { a } else { (sa % sb) as u32 },
                if b == 0 {
                    a
                } else {
                    (a as u128 % b as u128) as u32
                },
            ];
            for (f, expected) in expected.into_iter().enumerate() {
                assert_eq!(
                    one(r(30, 17, 31, f as u32, 1), a, b).x[30],
                    expected,
                    "M{f} {a:08x} {b:08x}"
                );
            }
        }
    }
}

#[test]
fn rv32_branches_links_alignment_and_fault_order() {
    for (f, take) in [
        (0, false),
        (1, true),
        (4, true),
        (5, false),
        (6, false),
        (7, true),
    ] {
        assert_eq!(
            one(branch(17, 31, f, -16), u32::MAX, 1).pc,
            if take { 0xf0 } else { 0x104 }
        );
        // Reverse operands to cover the other outcomes of ordered comparisons.
        if f >= 4 {
            assert_eq!(
                one(branch(17, 31, f, 16), 1, u32::MAX).pc,
                if take { 0x104 } else { 0x110 }
            );
        }
    }
    assert_eq!(one(branch(17, 31, 0, 8), 1, 1).pc, 0x108);
    assert_eq!(one(branch(17, 31, 1, 8), 1, 1).pc, 0x104);
    let c = one(0x008000ef, 0, 0); // JAL x1,+8
    assert_eq!((c.pc, c.x[1]), (0x108, 0x104));
    let c = one(i(0x67, 17, 17, 0, -1), 0x202, 0); // overlapping rs1/rd, clear bit zero
    assert_eq!((c.pc, c.x[17]), (0x200, 0x104));
}

#[test]
fn rv32_precise_traps_and_reserved_encodings() {
    let mut m = Memory::new(4096);
    for word in [
        0,
        0xffffffff,
        r(1, 17, 31, 0, 2),
        i(0x13, 1, 17, 1, 32),
        0x30001073,
        0x0000_305b, // guest-memory width 3 is reserved
    ] {
        put(&mut m, 0, &[word]);
        let mut c = Core::new(0);
        assert_eq!(
            c.step(&mut m).unwrap().unwrap().cause,
            Cause::IllegalInstruction
        );
        assert_eq!(c.pc, 0);
    }
    for word in [0x002000ef, branch(0, 0, 0, 2), i(0x67, 1, 17, 0, 0)] {
        put(&mut m, 0, &[word]);
        let mut c = Core::new(0);
        c.x[17] = 2;
        c.x[1] = 99;
        assert_eq!(
            c.step(&mut m).unwrap().unwrap().cause,
            Cause::InstructionMisaligned
        );
        assert_eq!((c.pc, c.x[1]), (0, 99));
    }
    assert_eq!(one(branch(17, 31, 0, 2), 1, 2).pc, 0x104); // not taken does not fault
    for (word, cause) in [(0x73, Cause::Ecall), (0x100073, Cause::Breakpoint)] {
        put(&mut m, 0, &[word]);
        let mut c = Core::new(0);
        assert_eq!(c.step(&mut m).unwrap().unwrap().cause, cause);
        assert_eq!(c.pc, 0);
    }
    assert_eq!(
        Core::new(2).step(&mut m).unwrap().unwrap().cause,
        Cause::InstructionMisaligned
    );
    assert_eq!(
        Core::new(4096).step(&mut m).unwrap().unwrap().cause,
        Cause::InstructionAccess
    );
    // Access fault at a JALR target belongs to the next fetch; link already commits.
    put(&mut m, 0, &[i(0x67, 1, 17, 0, 0)]);
    let mut c = Core::new(0);
    c.x[17] = 4096;
    assert_eq!(c.step(&mut m).unwrap(), None);
    assert_eq!(c.x[1], 4);
    assert_eq!(c.step(&mut m).unwrap().unwrap().pc, 4096);
}

#[test]
fn rv32_memory_widths_sign_extension_and_precise_failures() {
    let mut m = Memory::new(4096);
    m.write(0x800, &[0x80, 0xff, 0x34, 0x92]).unwrap();
    for (f, expected) in [
        (0, 0xffffff80),
        (1, 0xffffff80),
        (2, 0x9234ff80),
        (4, 0x80),
        (5, 0xff80),
    ] {
        put(&mut m, 0, &[i(3, 30, 17, f, -4)]);
        let mut c = Core::new(0);
        c.x[17] = 0x804;
        assert_eq!(c.step(&mut m).unwrap(), None);
        assert_eq!(c.x[30], expected);
    }
    for (f, bytes) in [(0, 1), (1, 2), (2, 4)] {
        put(&mut m, 0, &[store(17, 31, f, -4)]);
        let mut c = Core::new(0);
        c.x[17] = 0x904;
        c.x[31] = 0x12345678;
        assert_eq!(c.step(&mut m).unwrap(), None);
        assert_eq!(
            &m.ram[0x900..0x900 + bytes],
            &[0x78, 0x56, 0x34, 0x12][..bytes]
        );
    }
    for (word, addr, cause) in [
        (i(3, 0, 17, 2, 0), 4096, Cause::LoadAccess), // loads to zero still fault
        (i(3, 1, 17, 2, 0), 4095, Cause::LoadMisaligned),
        (store(17, 31, 2, 0), 4096, Cause::StoreAccess),
        (store(17, 31, 1, 0), 4095, Cause::StoreMisaligned),
    ] {
        put(&mut m, 0, &[word]);
        let before = m.ram.clone();
        let mut c = Core::new(0);
        c.x[17] = addr;
        c.x[1] = 99;
        c.x[31] = u32::MAX;
        let fault = c.step(&mut m).unwrap().unwrap();
        assert_eq!((fault.cause, fault.value), (cause, addr));
        assert_eq!((c.pc, c.x[1]), (0, 99));
        assert_eq!(m.ram, before);
    }
}

#[test]
fn rv32_fence_i_publishes_modified_code() {
    let mut m = Memory::new(4096);
    let mut c = Core::new(0);
    put(&mut m, 0, &[i(0x13, 10, 0, 0, 1), 0x0000100f]);
    c.step(&mut m).unwrap();
    assert_eq!(c.x[10], 1);
    put(&mut m, 0, &[i(0x13, 10, 0, 0, 2)]);
    c.pc = 0;
    c.step(&mut m).unwrap();
    assert_eq!(c.x[10], 1);
    c.step(&mut m).unwrap();
    assert_eq!(c.fences, 1);
    c.pc = 0;
    c.step(&mut m).unwrap();
    assert_eq!(c.x[10], 2);
    // FENCE does not modify GPRs even with nonzero reserved operand fields.
    let c = one(i(0x0f, 30, 17, 0, -1), 7, 8);
    assert_eq!(c.x[30], 0);
}

#[test]
fn elf_loader_validates_before_mutating_memory() {
    let mut elf = vec![0u8; 88];
    elf[..7].copy_from_slice(b"\x7fELF\x01\x01\x01");
    for (n, v) in [(16, 2u16), (18, 243), (40, 52), (42, 32), (44, 1)] {
        elf[n..n + 2].copy_from_slice(&v.to_le_bytes());
    }
    for (n, v) in [
        (20, 1u32),
        (24, 0x100),
        (28, 52),
        (52, 1),
        (56, 84),
        (60, 0x100),
        (64, 0x100),
        (68, 4),
        (72, 8),
        (76, 5),
        (84, 0x73),
    ] {
        elf[n..n + 4].copy_from_slice(&v.to_le_bytes());
    }
    let mut m = Memory::new(4096);
    assert_eq!(rv32::load_elf(&elf, &mut m).unwrap(), 0x100);
    assert_eq!(m.read(0x100, 32), 0x73);
    assert_eq!(m.read(0x104, 32), 0);
    let before = m.ram.clone();
    for (n, v) in [
        (18, 0u8),
        (36, 1),
        (60, 255),
        (68, 255),
        (72, 0),
        (76, 4),
        (44, 255),
    ] {
        let mut bad = elf.clone();
        bad[n] = v;
        assert!(rv32::load_elf(&bad, &mut m).is_err(), "byte {n}");
        assert_eq!(m.ram, before);
    }
    for end in 0..elf.len() {
        assert!(rv32::load_elf(&elf[..end], &mut m).is_err());
    }
}
