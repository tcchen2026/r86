//! Guest operations select a view without changing the meaning of native RV
//! accesses. Setup and service use real native words, not a host refill callback.
use super::*;

fn r(op: u32, f: u32, d: u32, a: u32, b: u32, k: u32) -> u32 {
    op | d << 7 | f << 12 | a << 15 | b << 20 | k << 25
}
fn put(m: &mut Memory, pc: u32, words: &[u32]) {
    m.write(
        pc,
        &words
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect::<Vec<_>>(),
    )
    .unwrap();
}
fn execute(c: &mut Core, m: &mut Memory, word: u32) {
    put(m, c.pc, &[word]);
    assert_eq!(c.step(m).unwrap(), None, "word={word:08x}");
}
fn configure(c: &mut Core, m: &mut Memory, f: u32, d: u32, a: u32, b: u32, k: u32) {
    c.x[1] = a;
    c.x[2] = b;
    execute(c, m, r(0x2b, f, d, 1, 2, k));
}
fn mapping(c: &mut Core, m: &mut Memory, linear: u32, page_rights: u32) {
    configure(c, m, 1, 0, linear, page_rights & !4, 0);
    configure(c, m, 1, 0, linear, page_rights & !3, 1);
}
fn setup() -> (Core, Memory) {
    let mut c = Core::new(0x1000);
    let mut m = Memory::new(0x20000);
    configure(&mut c, &mut m, 4, 0, 0x10000, 0x18000, 0);
    configure(&mut c, &mut m, 5, 0, 0x37, 17, 0); // PE,PG,WP,CPL3
    for seg in 0..6 {
        configure(&mut c, &mut m, 6, seg, 0, u32::MAX, 7);
    }
    mapping(&mut c, &mut m, 0x4000, 0x10007);
    c.x[3] = 0x4004;
    c.x[4] = 0xa1b2c3d4;
    c.flags = 0x80;
    c.pending_flags = 0x55;
    (c, m)
}
fn service_vector(c: &mut Core, m: &mut Memory) {
    c.x[1] = 0x8000;
    execute(c, m, r(0x2b, 7, 0, 1, 0, 0));
}

#[test]
fn native_and_guest_addresses_are_distinct_and_widths_zero_extend() {
    let (mut c, mut m) = setup();
    m.write(0x4004, &0x12345678u32.to_le_bytes()).unwrap();
    m.write(0x10004, &0x89abcdefu32.to_le_bytes()).unwrap();
    execute(&mut c, &mut m, r(0x03, 2, 5, 3, 0, 0)); // native LW
    assert_eq!(c.x[5], 0x12345678);
    for (width, value) in [(0, 0xef), (1, 0xcdef), (2, 0x89abcdef)] {
        execute(&mut c, &mut m, r(0x5b, width, 5, 3, 0, 3));
        assert_eq!(c.x[5], value);
    }
    for (width, value) in [(0, 0x89abcdd4), (1, 0x89abc3d4), (2, 0xa1b2c3d4)] {
        execute(&mut c, &mut m, r(0x5b, width + 4, 0, 3, 4, 3));
        assert_eq!(m.read(0x10004, 32), value);
        assert_eq!(m.read(0x4004, 32), 0x12345678);
    }
    // The old rs1 must supply the offset even when rd aliases it.
    execute(&mut c, &mut m, r(0x5b, 2, 3, 3, 0, 3));
    assert_eq!(c.x[3], 0xa1b2c3d4);
    assert_eq!((c.flags, c.pending_flags, c.bank_enters), (0x80, 0x55, 0));
}

#[test]
fn unaligned_fetch_reads_current_bytes_at_every_lane_without_data_rights() {
    for width in 0..3 {
        for lane in 0..4 {
            for dynamic in [false, true] {
                for destination in [0, 3, 4, 5] {
                    let (mut c, mut m) = setup();
                    // The execute proof alone suffices. Even based CS selects
                    // the linear byte lane, not the offset's low bits.
                    configure(&mut c, &mut m, 6, 1, 1, 0x4fff, 4);
                    mapping(&mut c, &mut m, 0x4000, 0x10004);
                    m.write(0x10000, &[0x80, 0x91, 0xa2, 0xb3, 0xc4, 0xd5, 0xe6, 0xf7])
                        .unwrap();
                    c.x[3] = 0x3fff + lane;
                    c.x[4] = 1;
                    let before = c.retired_steps;
                    execute(
                        &mut c,
                        &mut m,
                        r(
                            0x5b,
                            width,
                            destination,
                            3,
                            u32::from(dynamic) * 4,
                            if dynamic { 112 } else { 49 },
                        ),
                    );
                    let expected = match width {
                        0 => [0x80, 0x91, 0xa2, 0xb3],
                        1 => [0x9180, 0xa291, 0xb3a2, 0xc4b3],
                        _ => [0xb3a29180, 0xc4b3a291, 0xd5c4b3a2, 0xe6d5c4b3],
                    }[lane as usize];
                    assert_eq!(
                        c.x[destination as usize],
                        if destination == 0 { 0 } else { expected }
                    );
                    assert_eq!(c.retired_steps - before, 1);
                    assert_eq!((c.flags, c.pending_flags, c.bank_enters), (0x80, 0x55, 0));
                    assert!(!c.guest_service_active);
                }
            }
        }
    }
    let (mut c, mut m) = setup();
    c.x[3] = 0x4003;
    m.write(0x10003, &[1, 2, 3, 4]).unwrap();
    execute(&mut c, &mut m, r(0x5b, 2, 5, 3, 0, 49));
    assert_eq!(c.x[5], 0x04030201);
    // This is a guest data-bus fetch. A native writer changes its next result
    // immediately; FENCE.I only governs native RV instruction fetches.
    m.write(0x10003, &[5, 6, 7, 8]).unwrap();
    execute(&mut c, &mut m, r(0x5b, 2, 5, 3, 0, 49));
    assert_eq!(c.x[5], 0x08070605);
}

#[test]
fn unaligned_fetch_preserves_context_segment_span_and_mapping_rejection_order() {
    // Last required byte, not an aligned bus word's unused lanes, determines
    // the guest segment check. Cross-page accesses return before either page
    // can be consumed, even when both have execute proofs.
    for (base, limit, offset, width, reason) in [
        (0, 0x4004, 0x4003, 1, 0),
        (0, 0x4003, 0x4003, 1, 2),
        (0, 0x4fff, 0x4ffc, 2, 0),
        (0, 0x4fff, 0x4ffd, 2, 2),
        (0, u32::MAX, 0x4ffd, 2, 3),
        (0, u32::MAX, 0x4fff, 1, 3),
        (3, u32::MAX, 0x4ff9, 2, 0),
        (3, u32::MAX, 0x4ffb, 2, 3),
        (0, u32::MAX, 0x8001, 2, 4),
        (2, u32::MAX, u32::MAX - 1, 2, 2),
    ] {
        let (mut c, mut m) = setup();
        configure(&mut c, &mut m, 6, 1, base, limit, 4);
        mapping(&mut c, &mut m, 0x5000, 0x11004);
        service_vector(&mut c, &mut m);
        c.x[3] = offset;
        c.x[5] = 0xfeedface;
        execute(&mut c, &mut m, r(0x5b, width, 5, 3, 0, 49));
        assert_eq!(c.guest_service_active, reason != 0);
        if reason != 0 {
            assert_eq!(
                c.guest_service[3], reason,
                "base={base:x}, offset={offset:x}"
            );
            assert_eq!(c.x[5], 0xfeedface);
        }
        assert_eq!((c.flags, c.pending_flags), (0x80, 0x55));
    }
    for (control, rights, reason) in [(0, 0, 1), (1, 0, 2), (1, 4, 3)] {
        let (mut c, mut m) = setup();
        configure(&mut c, &mut m, 5, 0, control, 19, 0);
        configure(&mut c, &mut m, 6, 1, 0, u32::MAX, rights);
        service_vector(&mut c, &mut m);
        c.x[3] = 0x4fff; // also an absent mapping and a split dword
        execute(&mut c, &mut m, r(0x5b, 2, 5, 3, 0, 49));
        assert_eq!(c.guest_service[3], reason);
    }
}

#[test]
fn unaligned_fetch_native_failure_preserves_destination_and_retirement() {
    let (mut c, mut m) = setup();
    m.ram.truncate(0x10006);
    // The qualified page intentionally has broken native backing. The first
    // bus word exists but the required second word is incomplete. This is a
    // native error, not a guest refill, partial result, or aligned-address error.
    c.x[3] = 0x4003;
    c.x[5] = 0xfeedface;
    let instruction = r(0x5b, 2, 5, 3, 0, 49);
    put(&mut m, c.pc, &[instruction]);
    let before = (
        c.x,
        c.flags,
        c.pending_flags,
        c.retired_steps,
        m.ram.clone(),
    );
    assert_eq!(
        c.step(&mut m).unwrap(),
        Some(Trap {
            pc: c.pc,
            cause: Cause::LoadAccess,
            value: 0x10003,
        })
    );
    assert_eq!(
        (c.x, c.flags, c.pending_flags, c.retired_steps, m.ram),
        before
    );
    assert!(!c.guest_service_active);
}

#[test]
fn register_segment_loads_select_all_segments_and_read_old_operands() {
    for segment in 0..6 {
        for (width, value) in [(0, 0xef), (1, 0xcdef), (2, 0x89abcdef)] {
            for destination in [0, 3, 4, 5] {
                let (mut c, mut m) = setup();
                // Give every selector a distinct mapping. Merely truncating or
                // ignoring rs2 must not accidentally hit the expected address.
                for other in 0..6 {
                    configure(&mut c, &mut m, 6, other, 0x1000 * other, u32::MAX, 7);
                }
                mapping(&mut c, &mut m, segment * 0x1000, 0x10007);
                c.x[3] = 4;
                c.x[4] = segment;
                m.write(0x10004, &0x89abcdefu32.to_le_bytes()).unwrap();
                let before = c.x;
                execute(&mut c, &mut m, r(0x5b, width, destination, 3, 4, 64));
                for reg in 0..32 {
                    assert_eq!(
                        c.x[reg],
                        if reg != 0 && reg == destination as usize {
                            value
                        } else {
                            before[reg]
                        }
                    );
                }
                assert_eq!((c.flags, c.pending_flags, c.bank_enters), (0x80, 0x55, 0));
            }
        }
    }
}

#[test]
fn register_segment_load_rejects_full_invalid_selector_before_service() {
    for selector in [6, 7, 8, 0x103, 0xffff_fffbu32, u32::MAX] {
        let (mut c, mut m) = setup();
        service_vector(&mut c, &mut m);
        c.x[4] = selector;
        let word = r(0x5b, 2, 5, 3, 4, 64);
        put(&mut m, c.pc, &[word]);
        let before = (c.x, c.pc, c.retired_steps, c.guest_service, m.ram.clone());
        assert_eq!(
            c.step(&mut m).unwrap(),
            Some(Trap {
                pc: c.pc,
                cause: Cause::IllegalInstruction,
                value: word,
            })
        );
        assert_eq!((c.x, c.pc, c.retired_steps, c.guest_service, m.ram), before);
        assert!(!c.guest_service_active);
    }
}

#[test]
fn register_segment_load_keeps_intent_checks_and_can_rejoin_same_native_operation() {
    for (segment, intent, rights, reason) in [
        (3, 0, 1, 0),
        (3, 8, 1, 4),
        (3, 8, 3, 0),
        (1, 16, 1, 4),
        (1, 16, 4, 0),
    ] {
        let (mut c, mut m) = setup();
        mapping(&mut c, &mut m, 0x4000, 0x10000 | rights);
        service_vector(&mut c, &mut m);
        // Same adapter used by firmware: only t6 is clobbered, and return is
        // always to the instruction following the failed load. A caller can
        // therefore place the whole region in one inline-asm block with an
        // early t6 output; a miss does not escape a compiler-visible boundary.
        put(
            &mut m,
            0x8000,
            &[
                r(0x2b, 7, 31, 0, 0, 1),  // XGGET t6, saved native PC
                r(0x13, 0, 31, 31, 4, 0), // ADDI t6,t6,4
                r(0x2b, 7, 0, 31, 0, 2),  // XGRET t6
            ],
        );
        m.write(0x10004, &0x89abcdefu32.to_le_bytes()).unwrap();
        c.x[4] = segment;
        c.x[31] = 0;
        let old_pc = c.pc;
        let before = c.x;
        let word = r(0x5b, 2, 5, 3, 4, 64 | intent);
        execute(&mut c, &mut m, word);
        if reason != 0 {
            assert_eq!(c.guest_service, [old_pc, 0x4004, word, reason]);
            assert_eq!(c.x, before);
            for _ in 0..3 {
                assert_eq!(c.step(&mut m).unwrap(), None);
            }
        }
        assert_eq!(c.pc, old_pc + 4);
        for reg in 0..32 {
            assert_eq!(
                c.x[reg],
                if reason != 0 && reg == 31 {
                    old_pc + 4
                } else if reason == 0 && reg == 5 {
                    0x89abcdef
                } else {
                    before[reg]
                }
            );
        }
        assert!(!c.guest_service_active);
        assert_eq!((c.flags, c.pending_flags), (0x80, 0x55));
    }
}

#[test]
fn segments_and_pages_require_independent_access_proofs() {
    // Read, write, fetch and RMW have different proof requirements. A successful
    // page lookup cannot bypass segment rights, and vice versa.
    for segment in 0..6 {
        for intent in 0..4 {
            if intent == 2 && segment != 1 {
                continue;
            }
            let required = [1, 2, 4, 3][intent];
            let f = if intent == 1 { 6 } else { 2 };
            let k = segment | [0, 0, 16, 8][intent];
            let word = r(
                0x5b,
                f,
                if intent == 1 { 0 } else { 5 },
                3,
                if intent == 1 { 4 } else { 0 },
                k,
            );
            for segment_rights in 0..8 {
                for page_rights in 0..8 {
                    let (mut c, mut m) = setup();
                    configure(&mut c, &mut m, 6, segment, 0x3000, 0x1007, segment_rights);
                    mapping(&mut c, &mut m, 0x4000, 0x10000 | page_rights);
                    c.x[3] = 0x1004;
                    service_vector(&mut c, &mut m);
                    m.write(0x10004, &0x89abcdefu32.to_le_bytes()).unwrap();
                    let before_regs = c.x;
                    let old_pc = c.pc;
                    execute(&mut c, &mut m, word);
                    let reason = if segment_rights & required != required {
                        2
                    } else if page_rights & required != required {
                        4
                    } else {
                        0
                    };
                    if reason == 0 {
                        assert_eq!(c.pc, old_pc + 4);
                        if intent == 1 {
                            assert_eq!(m.read(0x10004, 32), 0xa1b2c3d4);
                        } else {
                            assert_eq!(c.x[5], 0x89abcdef);
                        }
                    } else {
                        assert_eq!(c.pc, 0x8000);
                        assert_eq!(c.x, before_regs);
                        assert_eq!(c.guest_service, [old_pc, 0x1004, word, reason]);
                        assert_eq!(m.read(0x10004, 32), 0x89abcdef);
                    }
                    assert_eq!((c.flags, c.pending_flags), (0x80, 0x55));
                }
            }
        }
    }
}

#[test]
fn segment_limits_linear_wrap_and_service_order() {
    // Offset validation is widened, but adding the segment base wraps at 32b.
    // Service reasons describe a rejected proof; software still decides x86 faults.
    for (base, limit, offset, reason) in [
        (0, 0x4007, 0x4004, 0),
        (0, 0x4006, 0x4004, 2),
        (0, u32::MAX, 0xffff_fffe, 2),
        (0, 0x4004, 0x4003, 2), // segment rejection before alignment
        (0, u32::MAX, 0x4003, 3),
        (0, u32::MAX, 0x5000, 4),
        (0xffff_c000, u32::MAX, 0x8004, 0),
    ] {
        let (mut c, mut m) = setup();
        configure(&mut c, &mut m, 6, 3, base, limit, 3);
        c.x[3] = offset;
        service_vector(&mut c, &mut m);
        let old_pc = c.pc;
        execute(&mut c, &mut m, r(0x5b, 2, 0, 3, 0, 3)); // x0 still checked
        assert_eq!(c.x[0], 0);
        assert_eq!(c.pc, if reason == 0 { old_pc + 4 } else { 0x8000 });
        assert_eq!(c.guest_service[3], reason);
    }
}

#[test]
fn changed_context_retires_page_and_segment_proofs() {
    for (control, identity) in [
        (0x37, 17),
        (0x33, 17),
        (0x37, 18),
        (1, 17),
        (0, 17),
        (9, 17),
    ] {
        let (mut c, mut m) = setup();
        configure(&mut c, &mut m, 5, 0, control, identity, 0);
        c.x[3] = 0x4004;
        let unchanged = (control, identity) == (0x37, 17);
        execute(&mut c, &mut m, r(0x2b, 0, 5, 3, 2, 0));
        assert_eq!(c.x[5], if unchanged { 0x10004 } else { u32::MAX });
        if !unchanged {
            // Restoring just the page is insufficient: segment proofs expired too.
            mapping(&mut c, &mut m, 0x4000, 0x10007);
        }
        service_vector(&mut c, &mut m);
        execute(&mut c, &mut m, r(0x5b, 2, 5, 3, 0, 3));
        assert_eq!(
            c.guest_service[3],
            if unchanged {
                0
            } else if control & 9 != 1 {
                1
            } else {
                2
            }
        );
    }
    // Protected mode without paging may use a software-qualified RAM mapping.
    let (mut c, mut m) = setup();
    configure(&mut c, &mut m, 5, 0, 1, 0, 0);
    configure(&mut c, &mut m, 6, 3, 0, u32::MAX, 3);
    mapping(&mut c, &mut m, 0x4000, 0x10003);
    execute(&mut c, &mut m, r(0x5b, 6, 0, 3, 4, 3));
    assert_eq!(m.read(0x10004, 32), 0xa1b2c3d4);
}

#[test]
fn first_context_installation_expires_preconfigured_proofs() {
    let mut c = Core::new(0x1000);
    let mut m = Memory::new(0x20000);
    configure(&mut c, &mut m, 4, 0, 0x10000, 0x18000, 0);
    configure(&mut c, &mut m, 6, 3, 0, u32::MAX, 3);
    mapping(&mut c, &mut m, 0x4000, 0x10003);
    configure(&mut c, &mut m, 5, 0, 0, 0, 0); // unconfigured differs from (0,0)
    c.x[3] = 0x4004;
    execute(&mut c, &mut m, r(0x2b, 0, 5, 3, 2, 0));
    assert_eq!(c.x[5], u32::MAX);
    assert_eq!(c.guest_segments, [GuestSegment::default(); 6]);
}

#[test]
fn guest_fetch_observes_current_bytes_after_alias_and_native_writes() {
    let (mut c, mut m) = setup();
    mapping(&mut c, &mut m, 0x5000, 0x10003);
    execute(&mut c, &mut m, r(0x5b, 2, 5, 3, 0, 17)); // CS fetch
    assert_eq!(c.x[5], 0);
    c.x[6] = 0x5004;
    execute(&mut c, &mut m, r(0x5b, 6, 0, 6, 4, 3)); // writable linear alias
    execute(&mut c, &mut m, r(0x5b, 2, 5, 3, 0, 17));
    assert_eq!(c.x[5], 0xa1b2c3d4);
    c.x[6] = 0x10004;
    c.x[4] = 0x90909090;
    execute(&mut c, &mut m, r(0x23, 2, 0, 6, 4, 0)); // trusted native writer
    execute(&mut c, &mut m, r(0x5b, 2, 5, 3, 0, 17));
    assert_eq!(c.x[5], 0x90909090);
    assert_eq!(c.fences, 0); // no native code publication involved
}

#[test]
fn native_service_fills_and_retries_on_the_same_register_bank() {
    let (mut c, mut m) = setup();
    service_vector(&mut c, &mut m);
    execute(&mut c, &mut m, r(0x2b, 3, 0, 0, 0, 0)); // cold table
    let pc = c.pc;
    let load = r(0x5b, 2, 5, 3, 0, 3);
    put(&mut m, pc, &[load, 0x00100073]);
    // This tiny handler's declared scratch set is x20..x23. It uses ordinary RV
    // to read native metadata and install a mapping; no x86 host callback runs.
    c.x[23] = 0x9000;
    m.write(0x9000, &0x10003u32.to_le_bytes()).unwrap();
    m.write(0x10004, &0x87654321u32.to_le_bytes()).unwrap();
    put(
        &mut m,
        0x8000,
        &[
            r(0x2b, 7, 20, 1, 0, 1),  // original guest offset
            r(0x03, 2, 21, 23, 0, 0), // native LW of software-qualified page
            r(0x2b, 1, 0, 20, 21, 0),
            r(0x2b, 7, 22, 0, 0, 1), // retry PC
            r(0x2b, 7, 0, 0, 0, 2),  // XGRET x0
        ],
    );
    let retired = c.retired_steps;
    let before = c.x;
    assert_eq!(c.step(&mut m).unwrap(), None);
    assert_eq!(c.pc, 0x8000);
    assert_eq!(c.x, before);
    assert_eq!((c.flags, c.pending_flags), (0x80, 0x55));
    assert_eq!(c.guest_service, [pc, 0x4004, load, 4]);
    assert_eq!(c.run(&mut m, 20).unwrap().cause, Cause::Breakpoint);
    assert_eq!(c.x[5], 0x87654321);
    assert_eq!(c.x[22], pc);
    assert_eq!(c.bank_enters, 0);
    assert!(!c.guest_service_active);
    assert_eq!(c.retired_steps - retired, 7); // miss + five service words + retry
    assert_eq!(c.host_steps, c.retired_steps + 1); // terminal EBREAK excluded
}

#[test]
fn store_commit_waits_for_service_retry_and_native_bus_success() {
    let (mut c, mut m) = setup();
    service_vector(&mut c, &mut m);
    // A write-intent read must not consume a read-only proof.
    mapping(&mut c, &mut m, 0x4000, 0x10001);
    let retry = c.pc;
    let read = r(0x5b, 2, 5, 3, 0, 3 | 8);
    execute(&mut c, &mut m, read);
    assert_eq!(c.guest_service[3], 4);
    // Both service visits reuse the same data-fill/return words; no native
    // code patch or FENCE.I is needed between the read and store retries.
    configure(&mut c, &mut m, 1, 0, 0x4000, 0x10003, 0);
    execute(&mut c, &mut m, r(0x2b, 7, 0, 0, 0, 2));
    assert_eq!(c.pc, retry);
    m.write(0x10004, &u32::MAX.to_le_bytes()).unwrap();
    assert_eq!(c.step(&mut m).unwrap(), None);
    c.x[6] = 1;
    execute(&mut c, &mut m, r(0x0b, 0, 7, 5, 6, 0)); // stage -1 + 1
    assert_eq!((c.flags, c.pending_flags, c.x[7]), (0x80, 0x55, 0));
    execute(&mut c, &mut m, r(0x2b, 3, 0, 0, 0, 0));
    let store = c.pc;
    execute(&mut c, &mut m, r(0x5b, 6, 0, 3, 7, 3 | 32));
    assert_eq!(m.read(0x10004, 32), u32::MAX);
    assert_eq!(c.flags, 0x80);
    // Refill only the data view and return, as two words at 0x8000.
    configure(&mut c, &mut m, 1, 0, 0x4000, 0x10003, 0);
    execute(&mut c, &mut m, r(0x2b, 7, 0, 0, 0, 2));
    assert_eq!(c.pc, store);
    assert_eq!(c.step(&mut m).unwrap(), None);
    assert_eq!((m.read(0x10004, 32), c.flags), (0, 0x55));

    for f in [2, 6] {
        let (mut c, mut m) = setup();
        service_vector(&mut c, &mut m);
        configure(&mut c, &mut m, 4, 0, 0x20000, 0x21000, 0);
        mapping(&mut c, &mut m, 0x4000, 0x20003);
        let word = r(
            0x5b,
            f,
            if f == 2 { 5 } else { 0 },
            3,
            if f == 2 { 0 } else { 4 },
            if f == 2 { 3 } else { 35 },
        );
        put(&mut m, c.pc, &[word]);
        let before = (
            c.x,
            c.flags,
            c.pending_flags,
            c.retired_steps,
            m.ram.clone(),
        );
        assert_eq!(
            c.step(&mut m).unwrap(),
            Some(Trap {
                pc: c.pc,
                cause: if f == 2 {
                    Cause::LoadAccess
                } else {
                    Cause::StoreAccess
                },
                value: 0x20004
            })
        );
        assert_eq!(
            (c.x, c.flags, c.pending_flags, c.retired_steps, m.ram),
            before
        );
        assert!(!c.guest_service_active);
    }
}

#[test]
fn unhandled_or_nested_service_is_precise_and_return_is_checked() {
    for nested in [false, true] {
        let (mut c, mut m) = setup();
        if nested {
            service_vector(&mut c, &mut m);
        }
        execute(&mut c, &mut m, r(0x2b, 3, 0, 0, 0, 0));
        let word = r(0x5b, 2, 0, 3, 0, 3);
        if nested {
            execute(&mut c, &mut m, word);
        }
        put(&mut m, c.pc, &[word]);
        let before = (
            c.x,
            c.flags,
            c.pending_flags,
            c.guest_service,
            c.retired_steps,
        );
        assert_eq!(
            c.step(&mut m).unwrap(),
            Some(Trap {
                pc: c.pc,
                cause: Cause::GuestService,
                value: 0x4004
            })
        );
        assert_eq!(
            (
                c.x,
                c.flags,
                c.pending_flags,
                c.guest_service,
                c.retired_steps
            ),
            before
        );
    }
    for target in [0x7002, 0x7000, 0x20000] {
        let (mut c, mut m) = setup();
        service_vector(&mut c, &mut m);
        execute(&mut c, &mut m, r(0x2b, 3, 0, 0, 0, 0));
        execute(&mut c, &mut m, r(0x5b, 2, 5, 3, 0, 3));
        for selector in 0..8 {
            execute(&mut c, &mut m, r(0x2b, 7, 8 + selector, selector, 0, 1));
        }
        assert_eq!(&c.x[8..12], &c.guest_service);
        assert_eq!(&c.x[12..16], &[0x37, 17, 0x8000, 1]);
        c.x[1] = target;
        put(&mut m, c.pc, &[r(0x2b, 7, 0, 1, 0, 2)]);
        let result = c.step(&mut m).unwrap();
        if target & 3 != 0 {
            assert_eq!(
                result,
                Some(Trap {
                    pc: c.pc,
                    cause: Cause::InstructionMisaligned,
                    value: target
                })
            );
            assert!(c.guest_service_active);
        } else {
            assert_eq!(result, None);
            assert_eq!(c.pc, target);
            assert!(!c.guest_service_active);
            if target == 0x20000 {
                assert_eq!(
                    c.step(&mut m).unwrap().unwrap().cause,
                    Cause::InstructionAccess
                );
            }
        }
    }
}

#[test]
fn invalid_guest_encodings_leave_all_architectural_effects_untouched() {
    let mut bad = vec![
        r(0x2b, 5, 1, 1, 2, 0),
        r(0x2b, 5, 0, 1, 2, 1),
        r(0x2b, 6, 6, 1, 2, 0),
        r(0x2b, 6, 3, 1, 2, 8),
        r(0x2b, 7, 1, 0, 0, 0),
        r(0x2b, 7, 0, 0, 1, 0),
        r(0x2b, 7, 5, 8, 0, 1),
        r(0x2b, 7, 5, 0, 1, 1),
        r(0x2b, 7, 0, 0, 0, 2), // no active service
        r(0x2b, 7, 0, 0, 0, 3),
        r(0x5b, 2, 5, 3, 1, 3),
        r(0x5b, 6, 5, 3, 4, 3),
    ];
    for f in 0..8 {
        for k in 0..128 {
            let store = f & 4 != 0;
            if f & 3 == 3
                || k & 7 >= 6
                || (k & 64 != 0 && (store || k & 7 != 0))
                || (store && k & 24 != 0)
                || (!store && k & 32 != 0 && k & 16 == 0)
                || (k & 16 != 0 && (k & 7 != 1 || k & 8 != 0))
            {
                bad.push(r(
                    0x5b,
                    f,
                    if store { 0 } else { 5 },
                    3,
                    if store { 4 } else { 0 },
                    k,
                ));
            }
        }
    }
    for word in bad {
        let (mut c, mut m) = setup();
        put(&mut m, c.pc, &[word]);
        let before = (
            c.x,
            c.pc,
            c.flags,
            c.pending_flags,
            c.guest_dtlb.clone(),
            c.guest_itlb,
            c.guest_segments,
            c.guest_context,
            c.guest_service,
            c.retired_steps,
            m.ram.clone(),
        );
        assert_eq!(
            c.step(&mut m).unwrap(),
            Some(Trap {
                pc: c.pc,
                cause: Cause::IllegalInstruction,
                value: word
            })
        );
        assert_eq!(
            (
                c.x,
                c.pc,
                c.flags,
                c.pending_flags,
                c.guest_dtlb.clone(),
                c.guest_itlb,
                c.guest_segments,
                c.guest_context,
                c.guest_service,
                c.retired_steps,
                m.ram
            ),
            before
        );
    }
    for (f, a) in [(5, 64), (7, 0x8002)] {
        let (mut c, mut m) = setup();
        c.x[1] = a;
        let word = r(0x2b, f, 0, 1, 0, 0);
        put(&mut m, c.pc, &[word]);
        let before = (
            c.guest_context,
            c.guest_dtlb.clone(),
            c.guest_itlb,
            c.guest_segments,
            c.guest_service_vector,
        );
        assert_eq!(
            c.step(&mut m).unwrap().unwrap().cause,
            Cause::IllegalInstruction
        );
        assert_eq!(
            (
                c.guest_context,
                c.guest_dtlb.clone(),
                c.guest_itlb,
                c.guest_segments,
                c.guest_service_vector
            ),
            before
        );
    }
}
