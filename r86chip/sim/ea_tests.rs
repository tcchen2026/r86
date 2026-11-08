//! Raw addressing-field tests, independent of the PC interpreter and its
//! compiler. All setup and observations use real native instructions.
use super::*;

fn word(f: u32, rd: u32, a: u32, b: u32) -> u32 {
    0x7b | rd << 7 | f << 12 | a << 15 | b << 20
}
fn step(c: &mut Core, m: &mut Memory, instruction: u32) -> Option<Trap> {
    m.write(c.pc, &instruction.to_le_bytes()).unwrap();
    c.step(m).unwrap()
}
fn issue(c: &mut Core, m: &mut Memory, instruction: u32) {
    assert_eq!(step(c, m, instruction), None);
}
fn setup() -> (Core, Memory) {
    let mut c = Core::new(0x1000);
    let mut m = Memory::new(0x20000);
    for instruction in [
        0x2b | 4 << 12 | 1 << 15 | 2 << 20, // native guest window
        0x2b | 5 << 12 | 3 << 15 | 4 << 20, // PE/PG/WP/CPL3
        0x2b | 6 << 12 | 1 << 7 | 5 << 15 | 6 << 20 | 4 << 25, // based CS/X
        0x2b | 1 << 12 | 7 << 15 | 8 << 20 | 1 << 25, // fetch-only proof
        word(2, 0, 9, 0),                   // native GPR array, distinct from guest RAM
    ] {
        c.x[1] = 0x10000;
        c.x[2] = 0x18000;
        c.x[3] = 0x37;
        c.x[4] = 3;
        c.x[5] = 0x1001;
        c.x[6] = u32::MAX;
        c.x[7] = 0x4000;
        c.x[8] = 0x10004;
        c.x[9] = 0x8000;
        issue(&mut c, &mut m, instruction);
    }
    c.flags = 0x8d5;
    c.pending_flags = 0x40;
    (c, m)
}
fn regs(m: &mut Memory, values: &[u32; 8]) {
    for (i, value) in values.iter().enumerate() {
        m.write(0x8000 + i as u32 * 4, &value.to_le_bytes())
            .unwrap();
    }
}
fn calculate(c: &mut Core, m: &mut Memory, modrm: u32, start: u32) -> (u32, u32) {
    c.x[10] = modrm;
    c.x[11] = start;
    c.x[12] = 0xdeadbeef;
    let before = c.retired_steps;
    issue(c, m, word(0, 12, 10, 11));
    assert_eq!(c.retired_steps, before + 1);
    issue(c, m, word(1, 13, 0, 0));
    assert_eq!((c.flags, c.pending_flags), (0x8d5, 0x40));
    assert!(!c.guest_service_active);
    (c.x[12], c.x[13])
}

#[test]
fn all_memory_modrm_sib_forms_have_correct_offset_length_and_default_segment() {
    // Distinct values make every register/scale visible, including u32 wrap.
    let values = [
        0xfffffff0, 0x12345678, 0x10203040, 0x2468abcd, 0x80808080, 0x87654321, 0x11111111,
        0x77777777,
    ];
    for mode in 0..3u32 {
        for rm in 0..8u32 {
            for sib in 0..if rm == 4 { 256 } else { 1 } {
                for reg_field in [0, 3, 7] {
                    for lane in 0..4 {
                        let (mut c, mut m) = setup();
                        regs(&mut m, &values);
                        // The known addressing matrix is the oracle: it does
                        // not call the interpreter's address decoder.
                        let base = if rm == 4 { sib & 7 } else { rm };
                        let absent = mode == 0 && base == 5;
                        let index = (sib >> 3) & 7;
                        let index_value = if rm != 4 || index == 4 {
                            0
                        } else {
                            values[index as usize].wrapping_mul(1 << (sib >> 6))
                        };
                        let displacement: u32 = match mode {
                            1 => 0xffffff80,
                            2 => 0xfedcba98,
                            _ if absent => 0x89abcdef,
                            _ => 0,
                        };
                        let count = match mode {
                            1 => 1,
                            2 => 4,
                            _ => u32::from(absent) * 4,
                        };
                        let mut bytes = Vec::new();
                        if rm == 4 {
                            bytes.push(sib as u8);
                        }
                        bytes.extend_from_slice(&displacement.to_le_bytes()[..count as usize]);
                        m.write(0x10000 + lane, &bytes).unwrap();
                        let expected = (if absent { 0 } else { values[base as usize] })
                            .wrapping_add(index_value)
                            .wrapping_add(displacement);
                        let info = (bytes.len() as u32 * 2)
                            | u32::from(!absent && (base == 4 || base == 5));
                        assert_eq!(
                            calculate(
                                &mut c,
                                &mut m,
                                mode << 6 | reg_field << 3 | rm,
                                0x2fff + lane
                            ),
                            (expected, info),
                            "mode={mode} rm={rm} sib={sib:02x}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn current_gprs_and_code_are_reread_and_inputs_may_alias_destination() {
    for destination in [0, 10, 11, 12] {
        let (mut c, mut m) = setup();
        regs(&mut m, &[0, 0, 0, 0, 100, 200, 0, 0]);
        for (sib, displacement, expected, info) in [(0x24, 1, 101, 5), (0x2d, 0x80, 272, 5)] {
            // [esp+disp8], then [ebp+ebp+disp8]; no cached decoded form.
            m.write(0x10001, &[sib, displacement]).unwrap();
            c.x[10] = 0x44;
            c.x[11] = 0x3000;
            issue(&mut c, &mut m, word(0, destination, 10, 11));
            assert_eq!(
                c.x[destination as usize],
                if destination == 0 { 0 } else { expected }
            );
            issue(&mut c, &mut m, word(1, 13, 0, 0));
            assert_eq!(c.x[13], info);
        }
        m.write(0x8014, &300u32.to_le_bytes()).unwrap();
        assert_eq!(calculate(&mut c, &mut m, 0x44, 0x3000), (472, 5));
    }
}

#[test]
fn no_code_bytes_need_no_execute_mapping_but_context_and_array_are_required() {
    let (mut c, mut m) = setup();
    regs(&mut m, &[1, 2, 3, 4, 5, 6, 7, 8]);
    issue(&mut c, &mut m, 0x2b | 3 << 12); // flush both tables
    assert_eq!(
        calculate(&mut c, &mut m, 5, u32::MAX),
        (0xdeadbeef, u32::MAX)
    );
    assert_eq!(calculate(&mut c, &mut m, 0, u32::MAX), (1, 0));
    c.x[1] = 0;
    c.x[2] = 3;
    issue(&mut c, &mut m, 0x2b | 5 << 12 | 1 << 15 | 2 << 20);
    assert_eq!(calculate(&mut c, &mut m, 0, 0), (0xdeadbeef, u32::MAX));
    let mut fresh = Core::new(0x2000);
    fresh.x[10] = 0;
    fresh.x[11] = 0;
    issue(&mut fresh, &mut m, word(0, 12, 10, 11));
    issue(&mut fresh, &mut m, word(1, 13, 0, 0));
    assert_eq!(fresh.x[13], u32::MAX);
}

#[test]
fn guest_rejections_precede_native_gpr_reads_and_do_not_redirect_service() {
    for condition in 0..7 {
        let (mut c, mut m) = setup();
        c.x[9] = 0x30000;
        issue(&mut c, &mut m, word(2, 0, 9, 0)); // outside native RAM
        // Mandatory SIB plus disp32, based CS. GPR access must never occur on
        // these misses; it would trap instead of returning the service result.
        let mut start = 0x3000;
        m.write(0x10001, &[0x24, 1, 2, 3, 4]).unwrap();
        match condition {
            0 => c.guest_context = None,
            1 => c.guest_context = Some((9, 1)), // v8086
            2 => c.guest_segments[1].rights = 1, // data R is not execute
            3 => c.guest_segments[1].limit = 0x3003,
            4 => c.guest_itlb.fill(None),
            5 => {
                start = 0x3ffd;
                m.write(0x10ffe, &[0x24, 0]).unwrap();
            }
            _ => {
                start = u32::MAX;
                c.guest_segments[1].base = 0x4001;
                m.write(0x10000, &[0x24]).unwrap();
            }
        }
        assert_eq!(
            calculate(&mut c, &mut m, 0x84, start),
            (0xdeadbeef, u32::MAX)
        );
    }
}

#[test]
fn native_faults_preserve_ea_info_and_destination_at_each_read_phase() {
    for phase in 0..4 {
        let (mut c, mut m) = setup();
        c.ea_info = 7;
        let (modrm, start, bad) = match phase {
            0 => {
                m.ram.truncate(0x10001);
                (0x44, 0x3000, 0x10001)
            } // SIB
            1 => {
                m.write(0x10001, &[0x24, 1, 2, 3, 4]).unwrap();
                m.ram.truncate(0x10004);
                (0x84, 0x3000, 0x10002)
            }
            2 => {
                c.x[9] = 0x30000;
                issue(&mut c, &mut m, word(2, 0, 9, 0));
                (0, 0, 0x30000)
            }
            _ => {
                c.x[9] = 0x1fffc;
                issue(&mut c, &mut m, word(2, 0, 9, 0));
                m.write(0x10001, &[0x08]).unwrap();
                (4, 0x3000, 0x20000)
            }
        };
        c.ea_info = 7;
        c.x[10] = modrm;
        c.x[11] = start;
        c.x[12] = 0xdeadbeef;
        let before = c.retired_steps;
        assert_eq!(
            step(&mut c, &mut m, word(0, 12, 10, 11)),
            Some(c.trap(Cause::LoadAccess, bad))
        );
        assert_eq!(
            (c.x[12], c.ea_info, c.retired_steps),
            (0xdeadbeef, 7, before)
        );
    }
}

#[test]
fn reserved_fields_and_bad_array_metadata_trap_before_any_effect() {
    for (instruction, operand) in [
        (word(0, 12, 10, 11), 256),
        (word(0, 12, 10, 11), 0xc0),
        (word(0, 12, 10, 11) | 1 << 25, 0),
        (word(1, 12, 10, 0), 0),
        (word(1, 12, 0, 1), 0),
        (word(2, 12, 10, 0), 0x8000),
        (word(2, 0, 10, 1), 0x8000),
        (word(2, 0, 10, 0), 0x8001),
        (word(2, 0, 10, 0), 0xffffffe4),
    ] {
        let (mut c, mut m) = setup();
        c.x[10] = operand;
        c.x[12] = 123;
        c.ea_info = 7;
        let before = c.retired_steps;
        assert_eq!(
            step(&mut c, &mut m, instruction),
            Some(c.trap(Cause::IllegalInstruction, instruction))
        );
        assert_eq!(
            (c.x[12], c.ea_info, c.ea_regs, c.retired_steps),
            (123, 7, Some(0x8000), before)
        );
    }
}
