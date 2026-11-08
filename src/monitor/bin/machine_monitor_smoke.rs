//! Experimental EL1 runner for the same deterministic PC as userspace.
#![cfg_attr(all(target_arch = "aarch64", target_os = "none"), no_std)]
#![cfg_attr(all(target_arch = "aarch64", target_os = "none"), no_main)]

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn main() {}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod aarch64_monitor {
    #[cfg(not(feature = "jit"))]
    use core::alloc::{GlobalAlloc, Layout};
    #[cfg(not(feature = "jit"))]
    use core::cell::UnsafeCell;
    use core::fmt::Write as _;
    use core::panic::PanicInfo;
    #[cfg(not(feature = "jit"))]
    use core::sync::atomic::AtomicUsize;
    use core::sync::atomic::Ordering;

    use r86::machine::{GuestStepStatus, Machine};
    use r86::monitor::abi::{
        CpuStateV1, DOORBELL_IPA, HostCommandKindV1, MIN_ARM_IPA_BITS, MailboxV1,
        debug::{DebugPhaseV1, DebugStateBlockV1, PanicBlockV1},
    };
    use r86::monitor::{
        DefaultMonitorShadowRuntime, HARDMMU_STATS_PARTS, MONITOR_SHADOW_L3_TABLES, MailboxClient,
        MmioDoorbell, MonitorBoot, MonitorDebugCpuSnapshot, MonitorDebugPublisher,
        MonitorDebugSnapshot, MonitorHardMmuStatsPacket, MonitorShadowStage1Tables,
        MonitorStage1Tables, MonitorStopAckPacket, NULL_HARDMMU_DEFAULT_STEPS,
        NULL_HARDMMU_ERROR_MARKER, NULL_HARDMMU_STEP_MARKER, SERIAL_FLUSH_PACKET_CAPACITY,
        SerialFlushPacket, X86MonitorBusStats, X86MonitorStepReport, cpu_from_state_v1,
        cpu_state_v1_from_cpu, enable_identity_stage1_with_shadow_from_boot,
        install_exception_panic_transport_with_debug, monitor_panic_halt,
    };

    #[cfg(not(feature = "jit"))]
    mod bump_heap {
        use super::*;
        // One VM lives for the lifetime of this experimental image. Device buffers
        // (including the normal 16 MiB VGA backing) come from this bounded arena.
        // This is not a general allocator or a reusable snapshot allocation scheme.
        const ARENA_BYTES: usize = 20 * 1024 * 1024;
        #[repr(align(4096))]
        struct Arena {
            bytes: UnsafeCell<[u8; ARENA_BYTES]>,
            used: AtomicUsize,
        }
        // Allocations reserve disjoint ranges atomically; allocated objects own
        // their bytes for the VM lifetime. The allocator never moves live storage.
        unsafe impl Sync for Arena {}
        unsafe impl GlobalAlloc for Arena {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                let base = self.bytes.get() as usize;
                let mut old = self.used.load(Ordering::Relaxed);
                loop {
                    let Some(start) = base
                        .checked_add(old)
                        .and_then(|p| p.checked_add(layout.align() - 1))
                        .map(|p| (p & !(layout.align() - 1)) - base)
                    else {
                        return core::ptr::null_mut();
                    };
                    let Some(end) = start
                        .checked_add(layout.size())
                        .filter(|&n| n <= ARENA_BYTES)
                    else {
                        return core::ptr::null_mut();
                    };
                    match self.used.compare_exchange_weak(
                        old,
                        end,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => return unsafe { self.bytes.get().cast::<u8>().add(start) },
                        Err(next) => old = next,
                    }
                }
            }
            unsafe fn dealloc(&self, _: *mut u8, _: Layout) {}
        }
        #[global_allocator]
        static ARENA: Arena = Arena {
            bytes: UnsafeCell::new([0; ARENA_BYTES]),
            used: AtomicUsize::new(0),
        };
    }
    static mut STAGE1_TABLES: MonitorStage1Tables = MonitorStage1Tables::new();
    static mut SHADOW_STAGE1: MonitorShadowStage1Tables<MONITOR_SHADOW_L3_TABLES> =
        MonitorShadowStage1Tables::new();

    static mut SHADOW_RUNTIME: DefaultMonitorShadowRuntime =
        DefaultMonitorShadowRuntime::new(core::ptr::null_mut());

    fn bus_stats(machine: &Machine) -> X86MonitorBusStats {
        let s = machine.hardmmu_stats();
        X86MonitorBusStats {
            native_read_attempts: s.native_read_attempts,
            native_read_hits: s.native_read_hits,
            native_read_aborts: s.native_read_aborts,
            native_read_descriptor_aborts: s.native_read_descriptor_aborts,
            native_read_translation_aborts: s.native_read_translation_aborts,
            native_read_access_flag_aborts: s.native_read_access_flag_aborts,
            native_read_permission_aborts: s.native_read_permission_aborts,
            ..Default::default()
        }
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_monitor_main(boot_block_ipa: u64) -> ! {
        unsafe { run(boot_block_ipa) };
        loop {
            unsafe { core::arch::asm!("wfi", options(nomem, nostack, preserves_flags)) };
        }
    }

    fn flush(client: &mut MailboxClient<'_>, doorbell: &mut MmioDoorbell, bytes: &[u8]) {
        for chunk in bytes.chunks(SERIAL_FLUSH_PACKET_CAPACITY) {
            let packet = SerialFlushPacket::from_bytes(chunk).expect("bounded serial packet");
            client
                .serial_flush(doorbell, packet)
                .expect("serial transport");
        }
    }

    unsafe fn run(boot_block_ipa: u64) {
        let boot = unsafe { &*(boot_block_ipa as *const r86::monitor::abi::BootBlockV1) };
        let parsed = MonitorBoot::from_boot_block(boot_block_ipa, boot, MIN_ARM_IPA_BITS)
            .expect("valid boot block");
        unsafe {
            enable_identity_stage1_with_shadow_from_boot(
                core::ptr::addr_of_mut!(STAGE1_TABLES),
                core::ptr::addr_of_mut!(SHADOW_STAGE1),
                boot,
                MIN_ARM_IPA_BITS,
            )
        }
        .expect("stage 1 tables");
        let mailbox = unsafe {
            core::slice::from_raw_parts_mut(
                parsed.mailbox_ipa as *mut u8,
                core::mem::size_of::<MailboxV1>(),
            )
        };
        let debug_state = parsed.debug_state_ipa as *mut DebugStateBlockV1;
        let panic_block = parsed.panic_block_ipa as *mut PanicBlockV1;
        let mut debug = unsafe { MonitorDebugPublisher::new(debug_state) };
        debug.publish_phase(DebugPhaseV1::Stage1Enable);
        install_exception_panic_transport_with_debug(
            mailbox.as_mut_ptr(),
            DOORBELL_IPA as *mut u32,
            debug_state,
            panic_block,
        );
        assert_ne!(
            parsed.feature_bits & r86::monitor::abi::FEATURE_MACHINE_DEVICES,
            0,
            "Machine needs complete backing and shared PC device ownership"
        );
        let mut client = MailboxClient::new(mailbox).expect("mailbox");
        let mut doorbell = MmioDoorbell::new(DOORBELL_IPA as *mut u32);
        client
            .hello(&mut doorbell, parsed.feature_bits, 0)
            .expect("hello transport");
        debug.publish_phase(DebugPhaseV1::Hello);
        let initial = unsafe { &*(parsed.cpu_state_ipa as *const CpuStateV1) };
        // The VMM owns this fixed RAM memslot until the monitor terminates.
        let ram = unsafe {
            core::slice::from_raw_parts_mut(parsed.ram_base as *mut u8, parsed.ram_size as usize)
        };
        let mut machine = Machine::from_preloaded(cpu_from_state_v1(initial), ram);
        let shadow = unsafe { &mut *core::ptr::addr_of_mut!(SHADOW_RUNTIME) };
        shadow.bind_stage1(core::ptr::addr_of_mut!(SHADOW_STAGE1));
        machine.enable_hardmmu(shadow);
        #[cfg(feature = "jit")]
        machine.set_jit_enabled(true).expect("EL1 JIT engine");
        let max_steps = if parsed.step_limit == 0 {
            NULL_HARDMMU_DEFAULT_STEPS
        } else {
            parsed.step_limit
        };
        let mut serial_sent = 0;
        let mut succeeded = true;
        while machine.step_count() < max_steps {
            if let Some(pending) = client.poll_command(0).expect("host command") {
                // This experiment does not export legacy host-device snapshots:
                // the real devices now live in Machine inside this monitor.
                assert_eq!(
                    pending.command.kind,
                    HostCommandKindV1::Stop as u32,
                    "unsupported machine monitor command"
                );
                break;
            }
            if machine.step_count() % (4096 * 256) == 0 {
                debug.publish(MonitorDebugSnapshot {
                    phase: DebugPhaseV1::X86Step,
                    steps: machine.step_count(),
                    cpu: Some(MonitorDebugCpuSnapshot::from_cpu(machine.cpu())),
                    bus: bus_stats(&machine),
                    shadow: machine.hardmmu_stats(),
                    transport: client.stats(),
                });
            }
            let budget = (max_steps - machine.step_count()).min(4096);
            let result = machine.run_steps(budget);
            flush(
                &mut client,
                &mut doorbell,
                &machine.serial_output()[serial_sent..],
            );
            serial_sent = machine.serial_output().len();
            match result {
                Ok(report) if report.stop.is_none() => {}
                Ok(report) if matches!(report.stop, Some(GuestStepStatus::Reset)) => {
                    assert_ne!(
                        parsed.feature_bits & r86::monitor::abi::FEATURE_MACHINE_BOOT_RELOAD,
                        0,
                        "VMM must retain the original boot inputs"
                    );
                    client
                        .reload_boot(&mut doorbell)
                        .expect("boot reload transport");
                    machine.reboot_preloaded(cpu_from_state_v1(initial));
                }
                Ok(_) => break,
                Err(error) => {
                    flush(&mut client, &mut doorbell, error.as_bytes());
                    succeeded = false;
                    break;
                }
            }
        }
        #[cfg(feature = "jit")]
        flush(&mut client, &mut doorbell, machine.jit_report().as_bytes());
        if parsed.feature_bits & r86::monitor::abi::FEATURE_MACHINE_STATE_V1 != 0 {
            let bytes = unsafe {
                core::slice::from_raw_parts_mut(
                    parsed.trace_ipa as *mut u8,
                    (boot.layout.trace_size - boot.trace_offset) as usize,
                )
            };
            struct StateWriter<'a> {
                bytes: &'a mut [u8],
                len: usize,
            }
            impl core::fmt::Write for StateWriter<'_> {
                fn write_str(&mut self, s: &str) -> core::fmt::Result {
                    let end = self.len.checked_add(s.len()).ok_or(core::fmt::Error)?;
                    self.bytes
                        .get_mut(self.len..end)
                        .ok_or(core::fmt::Error)?
                        .copy_from_slice(s.as_bytes());
                    self.len = end;
                    Ok(())
                }
            }
            let mut writer = StateWriter {
                bytes: &mut bytes[16..],
                len: 0,
            };
            writeln!(writer, "{:?}", machine.device_state_for_comparison())
                .expect("machine state fits trace slot");
            let len = writer.len as u64;
            bytes[8..16].copy_from_slice(&len.to_le_bytes());
            bytes[..8].copy_from_slice(&r86::monitor::abi::MACHINE_STATE_MAGIC.to_le_bytes());
        }
        let cpu = machine.cpu();
        let report = X86MonitorStepReport {
            steps: machine.step_count(),
            halted: cpu.halted,
            eip: cpu.eip,
            tsc: cpu.tsc(),
            bus: bus_stats(&machine),
        };
        debug.publish(MonitorDebugSnapshot {
            phase: DebugPhaseV1::Stopping,
            steps: report.steps,
            cpu: Some(MonitorDebugCpuSnapshot::from_cpu(cpu)),
            bus: report.bus,
            shadow: machine.hardmmu_stats(),
            transport: client.stats(),
        });
        unsafe {
            (parsed.cpu_state_ipa as *mut CpuStateV1)
                .write_volatile(cpu_state_v1_from_cpu(cpu, report.steps))
        };
        core::sync::atomic::fence(Ordering::Release);
        client
            .stop_ack(
                &mut doorbell,
                MonitorStopAckPacket::from_cpu_and_report(cpu, Some(&report)),
            )
            .expect("stop ack");
        for part in 0..HARDMMU_STATS_PARTS {
            if let Some(packet) = MonitorHardMmuStatsPacket::from_cpu_report_and_shadow_part(
                cpu,
                Some(&report),
                machine.hardmmu_stats(),
                part,
            ) {
                client
                    .hardmmu_stats(&mut doorbell, packet)
                    .expect("stats transport");
            }
        }
        flush(
            &mut client,
            &mut doorbell,
            if succeeded {
                NULL_HARDMMU_STEP_MARKER
            } else {
                NULL_HARDMMU_ERROR_MARKER
            },
        );
        client
            .halt(&mut doorbell, cpu.tsc())
            .expect("halt transport");
    }

    #[panic_handler]
    fn panic(info: &PanicInfo<'_>) -> ! {
        let (line, column, file) = info.location().map_or((0, 0, 0), |location| {
            let file = location.file().bytes().fold(0x811c_9dc5u32, |hash, byte| {
                (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
            });
            (location.line(), location.column(), file)
        });
        monitor_panic_halt(
            0xffff_0000,
            u64::from(line),
            u64::from(column),
            u64::from(file),
        )
    }
}
