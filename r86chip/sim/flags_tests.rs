//! Observable status publication and arithmetic checks for the executable ISA.
use super::*;

fn control(f: u32, d: u32, a: u32) -> u32 {
    0x0b | d << 7 | f << 12 | a << 15
}
fn alu(d: u32, a: u32, b: u32, lane: u32, k: u32) -> u32 {
    control(lane, d, a) | b << 20 | k << 25
}
fn put(m: &mut Memory, address: u32, words: &[u32]) {
    m.write(
        address,
        &words
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<_>>(),
    )
    .unwrap();
}

// Independent integer mathematics: overflow is signed range overflow, AF is
// nibble range overflow, and CF is unsigned range overflow. This deliberately
// does not reproduce the model's bitwise OF/AF expressions.
fn arithmetic_expected(a: u32, b: u32, bits: u32, kind: u32, cf: u32) -> (u32, u32) {
    let modulus = 1i64 << bits;
    let mask = modulus - 1;
    let a = i64::from(a) & mask;
    let b = i64::from(b) & mask;
    let carry = if kind == 2 || kind == 3 {
        i64::from(cf)
    } else {
        0
    };
    let signed = |x: i64| if x >= modulus / 2 { x - modulus } else { x };
    let op = |a: i64, b: i64| {
        if kind == 3 || kind == 5 {
            a - b - carry
        } else {
            a + b + carry
        }
    };
    let wide = op(a, b);
    let value = (wide & mask) as u32;
    let flags = u32::from(!(0..modulus).contains(&wide))
        | u32::from((value as u8).count_ones() % 2 == 0) << 2
        | u32::from(!(0..16).contains(&op(a & 15, b & 15))) << 4
        | u32::from(value == 0) << 6
        | u32::from(i64::from(value) >= modulus / 2) << 7
        | u32::from(!(-modulus / 2..modulus / 2).contains(&op(signed(a), signed(b)))) << 11;
    (value, flags)
}

#[test]
fn xalu_exhaustive_byte_carry_borrow_and_wider_edges() {
    for kind in [0, 2, 3, 5] {
        for cf in 0..=1 {
            for a in 0..=255 {
                for b in 0..=255 {
                    assert_eq!(
                        xalu(a, b, 2, kind, cf),
                        arithmetic_expected(a, b, 8, kind, cf)
                    );
                }
            }
            let edges = [
                0,
                1,
                15,
                16,
                0x7fff,
                0x8000,
                0xffff,
                0x7fffffff,
                0x80000000,
                u32::MAX,
            ];
            for (lane, bits) in [(0, 32), (1, 16)] {
                for a in edges {
                    for b in edges {
                        let (v, f) = arithmetic_expected(a, b, bits, kind, cf);
                        let mask = u32::MAX >> (32 - bits);
                        assert_eq!(xalu(a, b, lane, kind, cf), ((a & !mask) | v, f));
                    }
                }
            }
        }
    }
}

#[test]
fn xflags_publish_together_with_one_result_and_preserve_control_bits() {
    let mut m = Memory::new(4096);
    put(
        &mut m,
        0,
        &[
            control(4, 0, 1),
            alu(3, 2, 2, 0, 0),
            control(5, 4, 1),
            control(7, 5, 4),
            control(6, 6, 3),
            control(5, 7, 1),
            control(7, 8, 4),
            0x00100073,
        ],
    );
    let mut c = Core::new(0);
    c.x[1] = u32::MAX & !0x40;
    c.x[2] = 0x80000000;
    assert_eq!(c.run(&mut m, 20).unwrap().cause, Cause::Breakpoint);
    assert_eq!(c.x[4], c.x[1], "pending flags escaped before commit");
    assert_eq!(c.x[5], 0);
    assert_eq!(c.x[6], 0);
    assert_eq!(c.x[7], (c.x[1] & !STATUS_MASK) | 0x845);
    assert_eq!(c.x[8], 1);
}

#[test]
fn xflags_fault_after_alu_preserves_committed_state() {
    let mut m = Memory::new(4096);
    put(
        &mut m,
        0,
        &[
            control(4, 0, 1),
            alu(3, 2, 2, 0, 0),
            0x00022203,
            control(5, 5, 1),
            control(4, 0, 5),
            control(6, 0, 0),
            control(5, 6, 1),
            0x00100073,
        ],
    );
    let mut c = Core::new(0);
    c.x[1] = 0x203;
    c.x[2] = 0x80000000;
    c.x[4] = 4096;
    let fault = c.run(&mut m, 20).unwrap();
    assert_eq!(
        (fault.pc, fault.cause, fault.value),
        (8, Cause::LoadAccess, 4096)
    );
    assert_eq!(c.x[2], 0x80000000, "committed guest operand overwritten");
    assert_eq!(
        c.x[3], 0,
        "native temporary is not automatically rolled back"
    );
    assert_eq!(c.x[4], 4096, "failing load wrote its destination");
    // Host-directed observation/recovery, not a claim of native trap vectoring
    // or a checked x86-memory instruction. XFLOAD explicitly discards pending.
    c.pc = 12;
    assert_eq!(c.run(&mut m, 20).unwrap().cause, Cause::Breakpoint);
    assert_eq!((c.x[5], c.x[6]), (0x203, 0x203));
}

#[test]
fn xflags_reserved_forms_do_not_change_results_or_pending_status() {
    for bad in [
        alu(3, 1, 0, 0, 47),
        alu(3, 1, 2, 0, 65), // XMERGE reserves the arithmetic-operation bits
        alu(3, 1, 2, 1, 16),
        alu(3, 1, 2, 2, 33),
        alu(3, 1, 2, 0, 7),
        alu(3, 1, 0, 2, 23),
        control(4, 1, 1),
        control(5, 3, 1) | 1 << 20,
        control(6, 3, 1) | 1 << 25,
        control(7, 3, 16),
    ] {
        let mut m = Memory::new(4096);
        put(
            &mut m,
            0,
            &[
                control(4, 0, 1),
                alu(0, 0, 0, 0, 0),
                bad,
                control(5, 5, 1),
                control(6, 0, 0),
                control(5, 6, 1),
                0x00100073,
            ],
        );
        let mut c = Core::new(0);
        c.x[1] = 0x203;
        c.x[3] = 123;
        let t = c.run(&mut m, 20).unwrap();
        assert_eq!(
            (t.pc, t.cause, t.value),
            (8, Cause::IllegalInstruction, bad)
        );
        assert_eq!(c.x[3], 123);
        c.pc = 12;
        assert_eq!(c.run(&mut m, 20).unwrap().cause, Cause::Breakpoint);
        assert_eq!((c.x[5], c.x[6]), (0x203, 0x246));
    }
}

#[test]
fn xflags_aliases_high_bytes_carry_and_bank_exit() {
    let mut m = Memory::new(4096);
    put(
        &mut m,
        0,
        &[
            control(4, 0, 1),
            alu(2, 2, 2, 3, 0x10),
            control(6, 0, 0),
            control(5, 3, 1),
            0x00100073,
        ],
    );
    let mut c = Core::new(0);
    c.x[1] = 0x202;
    c.x[2] = 0x12348056;
    assert_eq!(c.run(&mut m, 20).unwrap().cause, Cause::Breakpoint);
    assert_eq!(c.x[2], 0x12340056);
    assert_eq!(c.x[3], 0xa47);

    put(
        &mut m,
        0x40,
        &[
            control(4, 0, 1),
            0x0001407b,
            control(6, 0, 0),
            control(5, 3, 1),
            0x00100073,
        ],
    );
    put(&mut m, 0x100, &[alu(0, 0, 0, 0, 0), 0x0000507b]);
    let mut c = Core::new(0x40);
    c.x[1] = 0x203;
    c.x[2] = 0x100;
    assert_eq!(c.run(&mut m, 20).unwrap().cause, Cause::Breakpoint);
    assert_eq!(c.x[3], 0x203, "XLEAVE must discard unfinished status");
}

#[test]
fn xalu_commit_form_publishes_result_and_flags_without_an_extra_word() {
    let mut m = Memory::new(4096);
    put(
        &mut m,
        0,
        &[
            control(4, 0, 1),
            alu(2, 2, 3, 0, 8 | 2),
            control(5, 4, 1),
            0x00100293,
            control(5, 5, 1),
            0x00100073,
        ],
    );
    let mut c = Core::new(0);
    c.x[1] = 0x203;
    c.x[2] = u32::MAX;
    assert_eq!(c.run(&mut m, 20).unwrap().cause, Cause::Breakpoint);
    assert_eq!(c.x[2], 0);
    assert_eq!((c.x[4], c.x[5]), (0x257, 0x257));
}
