//! Focused EL1 read-boundary checks, excluded from ordinary/timing builds.
//! The paged monitor smoke provides real shadow tables and fixed guest RAM.
//! Compare observable CPU, RAM and guest faults with shared interpreter runs;
//! explicit results and native counters also establish which path was exercised.

use super::*;
use crate::monitor::DefaultMonitorShadowRuntime;
use crate::x86::cpu::{CR0_PG, CR0_WP, SegmentCache};
use crate::x86::jit::runtime::Engine;
use crate::x86::mem;

fn word(bus: &mut HostBus, at: usize, value: u32) {
    bus.ram[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn setup(machine: &mut Machine, user: bool, code: &[u8]) {
    machine.cpu = Cpu::new();
    let cpu = &mut machine.cpu;
    cpu.enter_protected_flat(0x1000, 0x6ffc);
    cpu.cr0 |= CR0_PG | CR0_WP;
    cpu.cr3 = 0x4000;
    cpu.cr2 = 0xdead_beef;
    cpu.replace_eflags(0x8d7);
    cpu.regs[0] = 1;
    cpu.regs[1] = 4;
    cpu.regs[2] = 0;
    cpu.regs[3] = 0x6001;
    cpu.regs[6] = 0x6ff8;
    cpu.regs[7] = 0xa000;
    if user {
        cpu.set_seg_cache(SegmentReg::Cs, SegmentCache::flat(11, 0xc0fb));
        for reg in [SegmentReg::Ds, SegmentReg::Es, SegmentReg::Ss] {
            cpu.set_seg_cache(reg, SegmentCache::flat(19, 0xc0f3));
        }
    }
    let bus = &mut machine.bus;
    bus.ram.fill(0);
    bus.attention = false;
    bus.next_attention_step = u64::MAX;
    word(bus, 0x4000, 0x5007);
    word(bus, 0x5004, 0x3007); // code
    word(bus, 0x5014, 0x5007); // writable alias of the page table
    word(bus, 0x5018, 0x8007); // read data
    word(bus, 0x501c, 0x9007); // next page
    word(bus, 0x5028, 0xd007); // MOVS destination
    bus.ram[0x8000..0x9000].fill(0x17);
    bus.ram[0x9000..0xa000].fill(0x39);
    bus.ram[0x3000..0x3000 + code.len()].copy_from_slice(code);
    // Stop decoding at the intended sequence, so a short instruction budget
    // can enter the complete block instead of interpreting an oversized tail.
    bus.ram[0x3000 + code.len()] = 0xf4;
}

/// The caller transfers exclusive ownership of real guest RAM and the active
/// shadow runtime. Both Machines and all generated invocations are destroyed
/// before returning the runtime to the other hardware smoke checks.
pub fn run_read_checks(
    ram: &'static mut [u8],
    shadow: &'static mut DefaultMonitorShadowRuntime,
    mut report: impl FnMut(&str),
) -> &'static mut DefaultMonitorShadowRuntime {
    let mut actual = Machine::from_preloaded(Cpu::new(), ram);
    let mut expected = Machine::new(actual.ram().len());
    actual.enable_hardmmu(shadow);
    actual.set_jit_enabled(true).unwrap();
    let mut engine: Engine = actual.jit.take().unwrap();
    // Warm real ARM translations, change several PTEs before the next native
    // access, then require misses and new values. Cover a short deferred batch
    // and overflow, with an unrelated live leaf keeping the parent allocated.
    for user in [false, true] {
        for count in [2u32, 12] {
            engine.clear(&mut actual.bus).unwrap();
            setup(&mut actual, user, &[0x90]);
            actual.bus.shadow.as_deref_mut().unwrap().reset(&actual.cpu);
            for page in 0..=count {
                let linear = LinearAddr(0x6000 + page * 4096);
                word(
                    &mut actual.bus,
                    0x5018 + page as usize * 4,
                    0x10007 + page * 4096,
                );
                word(
                    &mut actual.bus,
                    0x10000 + page as usize * 4096,
                    0x11110000 + page,
                );
                assert_eq!(
                    mmu::read_linear_u32(
                        &mut actual.cpu,
                        &mut actual.bus,
                        linear,
                        AccessKind::Read
                    )
                    .unwrap(),
                    0x11110000 + page
                );
                assert_eq!(
                    actual.bus.try_native_read_linear_u32(
                        linear,
                        AccessKind::Read,
                        mmu::AccessPriv::from_cpl(actual.cpu.cpl()),
                        actual.cpu.cr0,
                        actual.cpu.cr3,
                        actual.cpu.cr4,
                        actual.cpu.eflags
                    ),
                    mem::NativeRead::Hit(0x11110000 + page)
                );
            }
            word(&mut actual.bus, 0x30000, 0x77777777);
            for page in 0..count {
                mmu::write_probed_phys_u32(
                    &mut actual.cpu,
                    &mut actual.bus,
                    PhysAddr(0x5018 + u64::from(page) * 4),
                    0x30007,
                )
                .unwrap();
                actual
                    .cpu
                    .invalidate_translation_page(LinearAddr(0x6000 + page * 4096));
            }
            for page in 0..count {
                let linear = LinearAddr(0x6000 + page * 4096);
                assert_eq!(
                    actual.bus.try_native_read_linear_u32(
                        linear,
                        AccessKind::Read,
                        mmu::AccessPriv::from_cpl(actual.cpu.cpl()),
                        actual.cpu.cr0,
                        actual.cpu.cr3,
                        actual.cpu.cr4,
                        actual.cpu.eflags
                    ),
                    mem::NativeRead::Miss
                );
                assert_eq!(
                    mmu::read_linear_u32(
                        &mut actual.cpu,
                        &mut actual.bus,
                        linear,
                        AccessKind::Read
                    )
                    .unwrap(),
                    0x77777777
                );
            }
            assert_eq!(
                mmu::read_linear_u32(
                    &mut actual.cpu,
                    &mut actual.bus,
                    LinearAddr(0x6000 + count * 4096),
                    AccessKind::Read
                )
                .unwrap(),
                0x11110000 + count
            );
            report(&format!(
                "JIT_TLBI user={user} changed_pages={count} remap=ok\n"
            ));
        }
    }
    // A native refill must not discover a writable page-table source while
    // earlier software range proofs remain live in the block. Once the shared
    // walk protects the sources, a refill must retain their write dependencies.
    for user in [false, true] {
        engine.clear(&mut actual.bus).unwrap();
        setup(&mut actual, user, &[0x8b, 0x03]);
        word(&mut actual.bus, 0x4000, 0x5027);
        word(&mut actual.bus, 0x5018, 0x8067);
        actual.bus.shadow.as_deref_mut().unwrap().reset(&actual.cpu);
        let code = actual.bus.cached_backing_token(PhysAddr(0x3000));
        actual.bus.jit_code_stamp(&mut actual.cpu, code);
        assert!(
            mmu::try_resident_native_translation(
                &actual.cpu,
                &mut actual.bus,
                LinearAddr(0x6000),
                false
            )
            .is_some()
        );
        assert!(
            !actual
                .bus
                .jit_refill_hard(&actual.cpu, LinearAddr(0x6000), false)
        );
        assert!(
            actual
                .bus
                .cached_write_translation_allowed(PhysAddr(0x5000))
        );
        assert_eq!(
            mmu::read_linear_u32(
                &mut actual.cpu,
                &mut actual.bus,
                LinearAddr(0x6000),
                AccessKind::Read
            )
            .unwrap(),
            0x17171717
        );
        assert!(
            !actual
                .bus
                .cached_write_translation_allowed(PhysAddr(0x5000))
        );
        actual
            .bus
            .shadow
            .as_deref_mut()
            .unwrap()
            .flush_for_physical_map_change();
        assert!(
            actual
                .bus
                .jit_refill_hard(&actual.cpu, LinearAddr(0x6000), false)
        );
        mmu::write_probed_phys_u32(&mut actual.cpu, &mut actual.bus, PhysAddr(0x5018), 0x9067)
            .unwrap();
        // Refill itself must have recorded the dependency: a PTE write already
        // retires its native mapping, before the guest's explicit INVLPG.
        let window = if user {
            crate::monitor::shadow::ShadowWindow::User
        } else {
            crate::monitor::shadow::ShadowWindow::Supervisor
        };
        assert!(
            actual
                .bus
                .shadow
                .as_deref_mut()
                .unwrap()
                .descriptor_for(window, LinearAddr(0x6000))
                .is_none()
        );
        actual.cpu.invalidate_translation_page(LinearAddr(0x6000));
        actual.bus.after_invlpg(LinearAddr(0x6000));
        assert_eq!(
            mmu::read_linear_u32(
                &mut actual.cpu,
                &mut actual.bus,
                LinearAddr(0x6000),
                AccessKind::Read
            )
            .unwrap(),
            0x39393939
        );
        report(&format!(
            "JIT_REFILL source_protection user={user} remap=ok\n"
        ));
    }
    // An RMW's faulting LDR still requires guest write permission. Even with
    // A/D already set, a read-only SS page must fault before the arithmetic
    // commits, and must never qualify for the in-vector refill.
    for user in [false, true] {
        engine.clear(&mut actual.bus).unwrap();
        for machine in [&mut expected, &mut actual] {
            setup(machine, user, &[0x43, 0x83, 0x04, 0x24, 1, 0x31, 0xc9]);
            word(&mut machine.bus, 0x5018, 0x8065);
        }
        actual.bus.shadow.as_deref_mut().unwrap().reset(&actual.cpu);
        let refills = engine.stats.hard_refills;
        let aborts = engine.stats.hard_read_aborts;
        for round in 0..3 {
            for machine in [&mut expected, &mut actual] {
                machine.cpu.eip = 0x1000;
                machine.cpu.regs[4] = 0x6000;
                machine.cpu.replace_eflags(0x8d7);
            }
            actual
                .bus
                .shadow
                .as_deref_mut()
                .unwrap()
                .flush_for_physical_map_change();
            let reference =
                Interpreter::run_batch(&mut expected.cpu, &mut expected.bus, round * 1000, 1000);
            let native = engine
                .run_batch(&mut actual.cpu, &mut actual.bus, round * 1000, 1000)
                .unwrap();
            assert_eq!(native, reference);
            assert_eq!(actual.cpu.regs, expected.cpu.regs);
            assert_eq!(actual.cpu.eip, expected.cpu.eip);
            assert_eq!(actual.cpu.cr2, 0x6000);
            assert_eq!(actual.cpu.tsc(), expected.cpu.tsc());
            assert_eq!(
                actual.cpu.effective_eflags(),
                expected.cpu.effective_eflags()
            );
            assert_eq!(actual.bus.ram, expected.bus.ram);
        }
        assert_eq!(engine.stats.hard_refills, refills);
        assert!(engine.stats.hard_read_aborts > aborts);
        report(&format!(
            "JIT_REFILL rmw_write_intent user={user} rejected=ok\n"
        ));
    }
    for user in [false, true] {
        engine.clear(&mut actual.bus).unwrap();
        for machine in [&mut expected, &mut actual] {
            setup(machine, user, &[0x50, 0x51]);
            word(&mut machine.bus, 0x5014, 0);
        }
        actual.bus.shadow.as_deref_mut().unwrap().reset(&actual.cpu);
        let aborts = engine.stats.hard_write_aborts;
        for round in 0..3 {
            for machine in [&mut expected, &mut actual] {
                machine.cpu.eip = 0x1000;
                machine.cpu.regs[4] = 0x6004;
                machine.cpu.cr2 = 0xdead_beef;
                machine.cpu.replace_eflags(0x8d7);
            }
            let reference =
                Interpreter::run_batch(&mut expected.cpu, &mut expected.bus, round * 2, 2);
            let native = engine
                .run_batch(&mut actual.cpu, &mut actual.bus, round * 2, 2)
                .unwrap();
            assert_eq!(native, reference);
            assert_eq!(actual.cpu.regs, expected.cpu.regs);
            assert_eq!(actual.cpu.regs[4], 0x6000);
            assert_eq!(actual.cpu.eip, expected.cpu.eip);
            assert!(matches!(
                native.stop,
                crate::x86::interp::BatchStop::Step {
                    instruction_eip: 0x1001,
                    reason: crate::x86::interp::BatchStepStop::Error(
                        crate::x86::interp::StepError::PageFault(_)
                    ),
                    ..
                }
            ));
            assert_eq!(actual.cpu.cr2, 0x5ffc);
            assert_eq!(actual.cpu.tsc(), expected.cpu.tsc());
            assert_eq!(
                actual.cpu.effective_eflags(),
                expected.cpu.effective_eflags()
            );
            assert_eq!(actual.bus.ram, expected.bus.ram);
            assert_eq!(&actual.bus.ram[0x8000..0x8004], &1u32.to_le_bytes());
        }
        assert!(engine.stats.hard_write_aborts > aborts);
        report(&format!(
            "JIT_SS partial_push user={user} first_committed=1\n"
        ));
    }
    for user in [false, true] {
        engine.clear(&mut actual.bus).unwrap();
        for machine in [&mut expected, &mut actual] {
            // PUSH/POP and an explicit SS RMW share the hardware address.
            setup(
                machine,
                user,
                &[
                    0x50, 0x5a, 0x89, 0x4c, 0x24, 0xfc, 0x83, 0x44, 0x24, 0xfc, 1, 0xeb, 0xf3,
                ],
            );
            machine.cpu.regs[4] = 0x6ffc;
        }
        actual.bus.shadow.as_deref_mut().unwrap().reset(&actual.cpu);
        for phase in 0..5 {
            if phase == 1 {
                // Keep the compiled loop and software caches, but replace its
                // native RW leaf with a real read-only mapping.
                actual
                    .bus
                    .shadow
                    .as_deref_mut()
                    .unwrap()
                    .flush_for_physical_map_change();
                mmu::translate_detailed(
                    &mut actual.cpu,
                    &mut actual.bus,
                    LinearAddr(0x6ff8),
                    AccessKind::Read,
                    if user { 3 } else { 0 },
                )
                .unwrap();
            }
            let mut watched_version = None;
            if phase == 2 {
                let token = actual.bus.cached_backing_token(PhysAddr(0x8000));
                let (_, version, pointer) = actual.bus.jit_code_stamp(&mut actual.cpu, token);
                watched_version = Some((version, pointer));
                assert!(
                    actual
                        .bus
                        .shadow
                        .as_deref_mut()
                        .unwrap()
                        .descriptor_for(
                            crate::monitor::shadow::ShadowWindow::Supervisor,
                            LinearAddr(0x6000)
                        )
                        .is_none()
                );
            }
            if phase == 3 {
                // The writable data frame becomes a real table for a different
                // linear region. Its aliases must be protected before admission.
                for machine in [&mut expected, &mut actual] {
                    mem::write_phys_u32(&mut machine.bus, PhysAddr(0x4004), 0x8007).unwrap();
                    mem::write_phys_u32(&mut machine.bus, PhysAddr(0x8010), 0xc007).unwrap();
                    mmu::read_linear_u32(
                        &mut machine.cpu,
                        &mut machine.bus,
                        LinearAddr(0x404000),
                        AccessKind::Read,
                    )
                    .unwrap();
                }
            }
            if phase == 4 {
                for machine in [&mut expected, &mut actual] {
                    mmu::write_probed_phys_u32(
                        &mut machine.cpu,
                        &mut machine.bus,
                        PhysAddr(0x5018),
                        0x8005,
                    )
                    .unwrap();
                    machine.cpu.invalidate_translation_page(LinearAddr(0x6000));
                    machine.bus.after_invlpg(LinearAddr(0x6000));
                    mmu::read_linear_u32(
                        &mut machine.cpu,
                        &mut machine.bus,
                        LinearAddr(0x6ff8),
                        AccessKind::Read,
                    )
                    .unwrap();
                }
            }
            let aborts = engine.stats.hard_write_aborts;
            let writes = engine.stats.native_memory_writes;
            let initial_esp = actual.cpu.regs[4];
            let reference =
                Interpreter::run_batch(&mut expected.cpu, &mut expected.bus, phase * 1000, 1000);
            let native = engine
                .run_batch(&mut actual.cpu, &mut actual.bus, phase * 1000, 1000)
                .unwrap();
            assert_eq!(native, reference, "SS phase={phase} user={user}");
            assert_eq!(actual.cpu.regs, expected.cpu.regs);
            assert_eq!(actual.cpu.eip, expected.cpu.eip);
            assert_eq!(actual.cpu.tsc(), expected.cpu.tsc());
            assert_eq!(actual.cpu.cr2, expected.cpu.cr2);
            assert_eq!(
                actual.cpu.effective_eflags(),
                expected.cpu.effective_eflags()
            );
            assert_eq!(actual.bus.ram, expected.bus.ram);
            report(&format!(
                "JIT_SS progress user={user} phase={phase} writes={} aborts={} compiled={} fills={} interp={}\n",
                engine.stats.native_memory_writes - writes,
                engine.stats.hard_write_aborts - aborts,
                engine.stats.compiled_blocks,
                actual.hardmmu_stats().write_fills,
                engine.stats.interpreted_steps
            ));
            if phase <= 2 {
                assert!(engine.stats.native_memory_writes > writes + 100);
            }
            if phase != 0 {
                assert!(engine.stats.hard_write_aborts > aborts);
            }
            if let Some((old, pointer)) = watched_version {
                assert_ne!(
                    unsafe { *pointer },
                    old,
                    "code watch observed the first stack write"
                );
            }
            if phase == 4 {
                assert_eq!(actual.cpu.regs[4], initial_esp);
            }
            report(&format!(
                "JIT_SS user={user} phase={phase} native_writes={} aborts={}\n",
                engine.stats.native_memory_writes - writes,
                engine.stats.hard_write_aborts - aborts
            ));
        }
    }
    for user in [false, true] {
        engine.clear(&mut actual.bus).unwrap();
        for machine in [&mut expected, &mut actual] {
            setup(machine, user, &[0x8b, 0x03, 0xeb, 0xfc]);
            word(&mut machine.bus, 0x5018, 0xfd00_0005);
            machine.bus.io_write_u32(0xcf8, 0x8000_1010).unwrap();
            machine.bus.io_write_u32(0xcfc, 0xfd00_0000).unwrap();
            machine.bus.io_write_u32(0xcf8, 0x8000_1004).unwrap();
            machine.bus.io_write_u32(0xcfc, 2).unwrap();
        }
        actual.bus.shadow.as_deref_mut().unwrap().reset(&actual.cpu);
        for phase in 0..3 {
            let value = 0x1234_5678 + phase as u32;
            let phys = if phase == 2 { 0xfc00_0000 } else { 0xfd00_0000 };
            for machine in [&mut expected, &mut actual] {
                if phase == 2 {
                    machine.bus.io_write_u32(0xcf8, 0x8000_1010).unwrap();
                    machine.bus.io_write_u32(0xcfc, phys).unwrap();
                    mmu::write_probed_phys_u32(
                        &mut machine.cpu,
                        &mut machine.bus,
                        PhysAddr(0x5018),
                        phys | 5,
                    )
                    .unwrap();
                    machine.cpu.invalidate_translation_page(LinearAddr(0x6000));
                    machine.bus.after_invlpg(LinearAddr(0x6000));
                }
                machine
                    .bus
                    .write_phys_u32(PhysAddr(u64::from(phys) + 1), value)
                    .unwrap();
                machine.bus.attention = false;
                if phase == 0 {
                    assert_eq!(
                        mmu::read_linear_u32(
                            &mut machine.cpu,
                            &mut machine.bus,
                            LinearAddr(0x6001),
                            AccessKind::Read
                        )
                        .unwrap(),
                        value
                    );
                }
            }
            let aborts = engine.stats.hard_read_aborts;
            let reads = engine.stats.native_memory_reads;
            let reference =
                Interpreter::run_batch(&mut expected.cpu, &mut expected.bus, phase * 1000, 1000);
            let native = engine
                .run_batch(&mut actual.cpu, &mut actual.bus, phase * 1000, 1000)
                .unwrap();
            assert_eq!(native, reference);
            assert_eq!(actual.cpu.regs, expected.cpu.regs);
            assert_eq!(actual.cpu.regs[0], value);
            assert_eq!(actual.cpu.eip, expected.cpu.eip);
            assert_eq!(actual.cpu.tsc(), expected.cpu.tsc());
            assert_eq!(
                actual.cpu.effective_eflags(),
                expected.cpu.effective_eflags()
            );
            assert_eq!(actual.bus.ram, expected.bus.ram);
            assert!(engine.stats.native_memory_reads > reads + 400);
            assert_eq!(
                engine.stats.hard_read_aborts - aborts,
                u64::from(phase == 2)
            );
            assert!(
                actual
                    .cpu
                    .cached_read_translation(LinearAddr(0x6001), user)
                    .is_none()
            );
        }
        report(&format!(
            "JIT_READ framebuffer user={user} relocated_aborts=1\n"
        ));
    }
    for user in [false, true] {
        engine.clear(&mut actual.bus).unwrap();
        for machine in [&mut expected, &mut actual] {
            setup(machine, user, &[0x8b, 0x03, 0xeb, 0xfc]);
        }
        actual.bus.shadow.as_deref_mut().unwrap().reset(&actual.cpu);
        for machine in [&mut expected, &mut actual] {
            for addr in [0x6000, 0x7000] {
                mmu::read_linear_u32(
                    &mut machine.cpu,
                    &mut machine.bus,
                    LinearAddr(addr),
                    AccessKind::Read,
                )
                .unwrap();
            }
        }
        for phase in 0..3 {
            if phase == 1 {
                for machine in [&mut expected, &mut actual] {
                    // Changing the second PTE and executing its INVLPG must
                    // preserve the first page's real hardware translation.
                    mmu::write_probed_phys_u32(
                        &mut machine.cpu,
                        &mut machine.bus,
                        PhysAddr(0x501c),
                        0xb007,
                    )
                    .unwrap();
                    machine.cpu.invalidate_translation_page(LinearAddr(0x7000));
                    machine.bus.after_invlpg(LinearAddr(0x7000));
                }
            }
            if phase == 2 {
                expected.cpu.regs[3] = 0x7000;
                actual.cpu.regs[3] = 0x7000;
            }
            let aborts = engine.stats.hard_read_aborts;
            let reference =
                Interpreter::run_batch(&mut expected.cpu, &mut expected.bus, phase * 1000, 1000);
            let native = engine
                .run_batch(&mut actual.cpu, &mut actual.bus, phase * 1000, 1000)
                .unwrap();
            assert_eq!(native, reference);
            assert_eq!(actual.cpu.regs, expected.cpu.regs);
            assert_eq!(actual.cpu.eip, expected.cpu.eip);
            assert_eq!(actual.cpu.tsc(), expected.cpu.tsc());
            assert_eq!(
                actual.cpu.effective_eflags(),
                expected.cpu.effective_eflags()
            );
            assert_eq!(actual.bus.ram, expected.bus.ram);
            assert_eq!(
                engine.stats.hard_read_aborts - aborts,
                u64::from(phase == 2)
            );
        }
        report(&format!(
            "JIT_READ retained_neighbor user={user} changed_page_aborts=1\n"
        ));
    }
    for user in [false, true] {
        for case in 0..9 {
            // The last XOR kills earlier flags in ordinary liveness analysis.
            // Every preceding memory exit must nevertheless retain them.
            let code: &[u8] = match case {
                0 => &[0x40, 0x8b, 0x03, 0x31, 0xc9, 0xeb, 0xf9],
                1..=3 => &[0x43, 0x8b, 0x03, 0x31, 0xc9],
                4 => &[0x43, 0x83, 0x03, 1, 0x31, 0xc9], // RMW needs write permission
                5 => &[0x43, 0xf7, 0x33, 0x31, 0xc9],    // DIV operand faults first
                6 => &[0x43, 0xf3, 0xad, 0x31, 0xc9],    // partial REP LODS
                7 => &[0x43, 0xf3, 0xa5, 0x31, 0xc9],    // partial REP MOVS
                _ => &[0x43, 0x64, 0x8b, 0x03, 0x31, 0xc9], // segment before paging
            };
            engine.clear(&mut actual.bus).unwrap();
            for machine in [&mut expected, &mut actual] {
                setup(machine, user, code);
                match case {
                    1 | 2 => {
                        machine.cpu.regs[3] = 0x6ffc; // INC makes the dword split
                        if case == 2 {
                            word(&mut machine.bus, 0x501c, 0);
                        }
                    }
                    3 => word(&mut machine.bus, 0x5018, if user { 0x8003 } else { 0 }),
                    4 => word(&mut machine.bus, 0x5018, 0x8005),
                    5 => word(&mut machine.bus, 0x5018, 0),
                    6 | 7 => word(&mut machine.bus, 0x501c, 0),
                    8 => {
                        machine.cpu.regs[3] = 0x2ffe;
                        machine.cpu.set_seg_cache(
                            SegmentReg::Fs,
                            SegmentCache::new(16, 0x4000, 0x2fff, 0xc093),
                        );
                        word(&mut machine.bus, 0x5018, 0);
                    }
                    _ => {}
                }
            }
            actual.bus.shadow.as_deref_mut().unwrap().reset(&actual.cpu);
            // Qualify the known code page through the real execute walker so
            // these short fault cases enter native code on their first opcode.
            // Otherwise the cold fetch handoff can interpret the whole case.
            for machine in [&mut expected, &mut actual] {
                let linear = LinearAddr(0x1000);
                let translation = mmu::translate_detailed(
                    &mut machine.cpu,
                    &mut machine.bus,
                    linear,
                    AccessKind::Execute,
                    if user { 3 } else { 0 },
                )
                .unwrap();
                let token = machine.bus.cached_backing_token(translation.phys);
                machine.cpu.fill_fetch_cache_with_backing(
                    linear,
                    translation.phys,
                    token,
                    translation.global,
                );
            }
            // Admit the current first page only. A later absent page must be
            // discovered at its real instruction/REP element, never ahead.
            if matches!(case, 4 | 6 | 7) {
                for machine in [&mut expected, &mut actual] {
                    mmu::read_linear_u8(
                        &mut machine.cpu,
                        &mut machine.bus,
                        LinearAddr(0x6000),
                        AccessKind::Read,
                    )
                    .unwrap();
                    if case == 7 {
                        mmu::write_linear_u8(
                            &mut machine.cpu,
                            &mut machine.bus,
                            LinearAddr(0xa000),
                            0,
                        )
                        .unwrap();
                    }
                }
            }
            let initial_regs = actual.cpu.regs;
            // Production admission compiles an entry on its third visit.
            // Revisit these short semantic cases without bypassing that policy.
            for round in 0..if case == 0 { 2 } else { 3 } {
                if case != 0 {
                    for machine in [&mut expected, &mut actual] {
                        machine.cpu.eip = 0x1000;
                        machine.cpu.regs = initial_regs;
                        machine.cpu.cr2 = 0xdead_beef;
                        machine.cpu.replace_eflags(0x8d7);
                    }
                }
                if case == 0 && round == 1 {
                    // Evict only hardware mappings: the compiled loop and
                    // software data row stay warm. Its next native load must
                    // refill once without a native exit, then hit repeatedly.
                    actual
                        .bus
                        .shadow
                        .as_deref_mut()
                        .unwrap()
                        .flush_for_physical_map_change();
                }
                let aborts = engine.stats.hard_read_aborts;
                let reads = engine.stats.native_memory_reads;
                let partial = engine.stats.partial_repeats;
                let refills = engine.stats.hard_refills;
                let budget = if case == 0 { 1000 } else { 3 };
                let reference = Interpreter::run_batch(
                    &mut expected.cpu,
                    &mut expected.bus,
                    round * budget,
                    budget,
                );
                let native = engine
                    .run_batch(&mut actual.cpu, &mut actual.bus, round * budget, budget)
                    .unwrap();
                report(&format!(
                    "JIT_READ user={user} case={case} reads={} aborts={} refills={} partial={}\n",
                    engine.stats.native_memory_reads - reads,
                    engine.stats.hard_read_aborts - aborts,
                    engine.stats.hard_refills - refills,
                    engine.stats.partial_repeats - partial
                ));
                assert_eq!(native, reference, "user={user} case={case}");
                assert_eq!(
                    actual.cpu.regs, expected.cpu.regs,
                    "user={user} case={case}"
                );
                assert_eq!(actual.cpu.eip, expected.cpu.eip);
                assert_eq!(actual.cpu.cr2, expected.cpu.cr2);
                assert_eq!(actual.cpu.tsc(), expected.cpu.tsc());
                assert_eq!(
                    actual.cpu.effective_eflags(),
                    expected.cpu.effective_eflags()
                );
                assert_eq!(
                    actual.bus.ram, expected.bus.ram,
                    "A/D and partial writes, user={user} case={case}"
                );
                if (2..=7).contains(&case) {
                    let BatchStop::Step {
                        reason: crate::x86::interp::BatchStepStop::Error(StepError::PageFault(page)),
                        ..
                    } = native.stop
                    else {
                        panic!("expected architectural page fault");
                    };
                    let code = u32::from(user) * 4
                        | if case == 4 {
                            3
                        } else if case == 3 && user {
                            1
                        } else {
                            0
                        };
                    assert_eq!(page.code, code);
                }
                match case {
                    0 => {
                        if round == 1 {
                            assert_eq!(engine.stats.hard_read_aborts, aborts);
                            assert_eq!(engine.stats.hard_refills - refills, 1);
                        }
                        assert!(engine.stats.native_memory_reads - reads > 100);
                        assert_eq!(actual.cpu.regs[0], 0x1717_1717);
                    }
                    2 => assert_eq!(actual.cpu.cr2, 0x7000),
                    3..=5 => {
                        assert_eq!(actual.cpu.cr2, actual.cpu.regs[3]);
                        if round == 2 && case != 4 {
                            assert_eq!(engine.stats.hard_read_aborts - aborts, 1);
                        }
                    }
                    6 | 7 => {
                        assert_eq!(actual.cpu.cr2, 0x7000);
                        assert_eq!(actual.cpu.regs[1], 2);
                        if round == 2 {
                            assert!(engine.stats.partial_repeats > partial);
                        }
                    }
                    8 => assert_eq!(actual.cpu.cr2, 0xdead_beef),
                    _ => {}
                }
            }
        }
    }
    // Warm a compiled read loop, then execute a guest PTE write + INVLPG and
    // return to that same loop. Old compiled content may survive; its old data
    // mapping must not. Both phases execute real native loads.
    engine.clear(&mut actual.bus).unwrap();
    let mut code = vec![0x90; 0x110];
    code[..5].copy_from_slice(&[0x8b, 0x03, 0x41, 0xeb, 0xfb]);
    let update = [
        0xc7, 0x05, 0x18, 0x50, 0, 0, 0x07, 0x90, 0, 0, 0x0f, 0x01, 0x3b, 0xe9, 0xee, 0xfe, 0xff,
        0xff,
    ];
    code.resize(0x100 + update.len(), 0);
    code[0x100..].copy_from_slice(&update);
    for machine in [&mut expected, &mut actual] {
        setup(machine, false, &code);
    }
    actual.bus.shadow.as_deref_mut().unwrap().reset(&actual.cpu);
    for phase in 0..2 {
        if phase == 1 {
            expected.cpu.eip = 0x1100;
            actual.cpu.eip = 0x1100;
        }
        let reads = engine.stats.native_memory_reads;
        let reference =
            Interpreter::run_batch(&mut expected.cpu, &mut expected.bus, phase * 100, 100);
        let native = engine
            .run_batch(&mut actual.cpu, &mut actual.bus, phase * 100, 100)
            .unwrap();
        assert_eq!(native, reference);
        assert_eq!(
            actual.cpu.regs[0],
            if phase == 0 { 0x1717_1717 } else { 0x3939_3939 }
        );
        assert_eq!(actual.cpu.regs, expected.cpu.regs);
        assert_eq!(actual.cpu.eip, expected.cpu.eip);
        assert_eq!(actual.cpu.tsc(), expected.cpu.tsc());
        assert_eq!(
            actual.cpu.effective_eflags(),
            expected.cpu.effective_eflags()
        );
        assert_eq!(actual.bus.ram, expected.bus.ram);
        assert!(engine.stats.native_memory_reads - reads > 20);
    }
    // The same physical code page can execute before and after CR0.PG changes.
    // Its memory lowering is a compiled fact: retire links on the transition,
    // while retaining separately qualified bodies in the shared code arena.
    engine.clear(&mut actual.bus).unwrap();
    let code = [0x8b, 0x03, 0x41, 0xeb, 0xfb];
    for machine in [&mut expected, &mut actual] {
        setup(machine, false, &code);
        machine.bus.ram[0x1000..0x1005].copy_from_slice(&code);
        machine.bus.ram[0x1005] = 0xf4;
        machine.bus.ram[0x6000..0x7000].fill(0x17);
        word(&mut machine.bus, 0x5004, 0x1007);
    }
    actual.bus.shadow.as_deref_mut().unwrap().reset(&actual.cpu);
    for (phase, paged) in [false, true, false].into_iter().enumerate() {
        for machine in [&mut expected, &mut actual] {
            let old = machine.cpu.cr0;
            machine.cpu.cr0 = (old & !CR0_PG) | if paged { CR0_PG } else { 0 };
            machine.cpu.eip = 0x1000;
            machine.cpu.invalidate_translation_context();
            machine
                .bus
                .after_control_register_write(0, old, machine.cpu.cr0);
        }
        let native_steps = engine.stats.native_steps;
        let aborts = engine.stats.hard_read_aborts;
        let reference = Interpreter::run_batch(
            &mut expected.cpu,
            &mut expected.bus,
            phase as u64 * 1000,
            1000,
        );
        let native = engine
            .run_batch(&mut actual.cpu, &mut actual.bus, phase as u64 * 1000, 1000)
            .unwrap();
        assert_eq!(native, reference);
        assert_eq!(actual.cpu.regs, expected.cpu.regs);
        assert_eq!(actual.cpu.regs[0], 0x1717_1717);
        assert_eq!(actual.cpu.eip, expected.cpu.eip);
        assert_eq!(actual.cpu.tsc(), expected.cpu.tsc());
        assert_eq!(
            actual.cpu.effective_eflags(),
            expected.cpu.effective_eflags()
        );
        assert_eq!(actual.bus.ram, expected.bus.ram);
        assert!(engine.stats.native_steps - native_steps > 900);
        if !paged {
            assert_eq!(engine.stats.hard_read_aborts, aborts);
        }
    }
    report("JIT_READ paging_transitions=3\n");
    actual.bus.shadow.take().unwrap()
}
