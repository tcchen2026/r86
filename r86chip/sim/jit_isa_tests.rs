//! Direct JIT primitives: aliases, untouched flags and precise branch faults.
use super::*;

fn put(m: &mut Memory, pc: u32, word: u32) {
    m.write(pc, &word.to_le_bytes()).unwrap();
}

fn xjcc(cc: u32, offset: i32) -> u32 {
    let n = offset as u32;
    0x307b
        | cc << 15
        | ((n >> 12) & 1) << 31
        | ((n >> 11) & 1) << 7
        | ((n >> 5) & 63) << 25
        | ((n >> 1) & 15) << 8
}

#[test]
fn xjcc_observes_committed_flags_and_preserves_both_banks() {
    for combination in 0..64 {
        let [cf, pf, af, zf, sf, of] = core::array::from_fn(|n| combination & (1 << n) != 0);
        let status = u32::from(cf)
            | u32::from(pf) << 2
            | u32::from(af) << 4
            | u32::from(zf) << 6
            | u32::from(sf) << 7
            | u32::from(of) << 11;
        let expected = [
            of,
            !of,
            cf,
            !cf,
            zf,
            !zf,
            cf || zf,
            !cf && !zf,
            sf,
            !sf,
            pf,
            !pf,
            sf != of,
            sf == of,
            zf || sf != of,
            !zf && sf == of,
        ];
        for bank in [false, true] {
            for (cc, take) in expected.into_iter().enumerate() {
                for offset in [-4096, -4, 0, 4, 8, 4092] {
                    let mut m = Memory::new(0x4000);
                    let mut c = Core::new(0x2000);
                    c.bank = bank;
                    c.flags = status;
                    c.pending_flags = status ^ STATUS_MASK;
                    c.x[9] = 0x12345678;
                    c.inactive[9] = 0xfedcba98;
                    let active = c.x;
                    let inactive = c.inactive;
                    put(&mut m, c.pc, xjcc(cc as u32, offset));
                    assert_eq!(c.step(&mut m).unwrap(), None);
                    assert_eq!(
                        c.pc,
                        if take {
                            0x2000u32.wrapping_add_signed(offset)
                        } else {
                            0x2004
                        }
                    );
                    assert_eq!((c.x, c.inactive, c.bank), (active, inactive, bank));
                    assert_eq!((c.flags, c.pending_flags), (status, status ^ STATUS_MASK));
                    assert_eq!(c.retired_steps, 1);
                }
            }
        }
    }
}

#[test]
fn xjcc_checks_encoding_before_condition_and_target_only_when_taken() {
    for (word, flags, cause, value) in [
        (xjcc(4, 2), 0x40, Cause::InstructionMisaligned, 0x102),
        (xjcc(16, 2), 0, Cause::IllegalInstruction, xjcc(16, 2)),
        (
            xjcc(4, 2) | 1 << 20,
            0,
            Cause::IllegalInstruction,
            xjcc(4, 2) | 1 << 20,
        ),
    ] {
        let mut m = Memory::new(4096);
        let mut c = Core::new(0x100);
        c.flags = flags;
        c.pending_flags = 0x801;
        c.x[1] = 123;
        let before = c.x;
        put(&mut m, c.pc, word);
        let trap = c.step(&mut m).unwrap().unwrap();
        assert_eq!((trap.cause, trap.value, c.pc), (cause, value, 0x100));
        assert_eq!(
            (c.x, c.flags, c.pending_flags, c.retired_steps),
            (before, flags, 0x801, 0)
        );
    }
    let mut m = Memory::new(4096);
    let mut c = Core::new(0);
    // ZF=0: the halfword target is not checked.
    put(&mut m, 0, xjcc(4, 2));
    assert_eq!(c.step(&mut m).unwrap(), None);
    assert_eq!(c.pc, 4);
    put(&mut m, 4, xjcc(5, -8));
    assert_eq!(c.step(&mut m).unwrap(), None);
    assert_eq!(c.pc, 0xffff_fffc);
    let trap = c.step(&mut m).unwrap().unwrap();
    assert_eq!(
        (trap.cause, trap.value),
        (Cause::InstructionAccess, 0xffff_fffc)
    );
    assert_eq!(c.retired_steps, 2);
}

#[test]
fn xmerge_replaces_only_the_selected_bytes_from_old_operands() {
    for (lane, source_high) in [
        (0, false),
        (1, false),
        (2, false),
        (2, true),
        (3, false),
        (3, true),
    ] {
        for (a, b) in [(5, 6), (5, 5), (0, 6), (5, 0)] {
            for rd in [0, 5, 6, 7] {
                for source in [0, 1, 0x807fff01, 0xffff0000, u32::MAX] {
                    let mut m = Memory::new(4096);
                    let mut c = Core::new(0);
                    c.x[5] = 0xa593c76e;
                    c.x[6] = source;
                    c.x[7] = 0x12345678; // preserved bits must not come from rd
                    c.flags = 0x841;
                    c.pending_flags = 0x94;
                    let mut expected = c.x;
                    let mut bytes = c.x[a].to_le_bytes();
                    let source = c.x[b].to_le_bytes();
                    let width = [4, 2, 1, 1][lane as usize];
                    let start = usize::from(lane == 3);
                    let from = usize::from(source_high);
                    bytes[start..start + width].copy_from_slice(&source[from..from + width]);
                    if rd != 0 {
                        expected[rd] = u32::from_le_bytes(bytes);
                    }
                    let word = 0x0b
                        | (rd as u32) << 7
                        | lane << 12
                        | (a as u32) << 15
                        | (b as u32) << 20
                        | (0x40 | u32::from(source_high) << 4) << 25;
                    put(&mut m, 0, word);
                    assert_eq!(c.step(&mut m).unwrap(), None);
                    assert_eq!(c.x, expected, "{word:08x}");
                    assert_eq!((c.flags, c.pending_flags, c.pc), (0x841, 0x94, 4));
                    assert_eq!(c.xalu_ops, 0, "a merge is not a flag calculation");
                }
            }
        }
    }
}

#[test]
fn xmerge_reserved_controls_trap_without_publishing_data_or_status() {
    for lane in 0..4 {
        for control in 0x40..0x80 {
            if control == 0x40 || (control == 0x50 && lane >= 2) {
                continue;
            }
            for rd in [0, 3] {
                let mut c = Core::new(0);
                let mut m = Memory::new(4096);
                c.flags = 0x801;
                c.pending_flags = 0x44;
                c.x[3] = u32::MAX;
                let word = 0x0b | rd << 7 | lane << 12 | control << 25;
                put(&mut m, 0, word);
                let trap = c.step(&mut m).unwrap().unwrap();
                assert_eq!((trap.cause, trap.value), (Cause::IllegalInstruction, word));
                assert_eq!(
                    (c.pc, c.x[3], c.flags, c.pending_flags),
                    (0, u32::MAX, 0x801, 0x44)
                );
            }
        }
    }
}

#[test]
fn zba_scales_then_adds_with_wrapping_and_old_operand_aliases() {
    for shift in 1..=3 {
        for (a, b) in [(5, 6), (5, 5), (0, 6), (5, 0)] {
            for rd in [0, 5, 6, 7] {
                for source in [0, 1, 0x80000000, 0xffffffff, 0xf2345678] {
                    let mut c = Core::new(0);
                    let mut m = Memory::new(4096);
                    c.x[5] = source;
                    c.x[6] = 0x87654321;
                    c.flags = 0x40;
                    c.pending_flags = 0x801;
                    let mut expected = c.x;
                    let value = (u64::from(c.x[a]) * (1 << shift) + u64::from(c.x[b])) as u32;
                    if rd != 0 {
                        expected[rd] = value;
                    }
                    put(
                        &mut m,
                        0,
                        0x20000033
                            | (rd as u32) << 7
                            | (shift * 2) << 12
                            | (a as u32) << 15
                            | (b as u32) << 20,
                    );
                    assert_eq!(c.step(&mut m).unwrap(), None);
                    assert_eq!(c.x, expected);
                    assert_eq!((c.flags, c.pending_flags), (0x40, 0x801));
                }
            }
        }
    }
    for f in [0, 1, 3, 5, 7] {
        let mut c = Core::new(0);
        let mut m = Memory::new(4096);
        let word = 0x20000033 | f << 12;
        put(&mut m, 0, word);
        let trap = c.step(&mut m).unwrap().unwrap();
        assert_eq!(
            (trap.cause, trap.value, c.pc),
            (Cause::IllegalInstruction, word, 0)
        );
    }
}
