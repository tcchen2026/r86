#![cfg_attr(all(target_arch = "aarch64", target_os = "none"), no_std)]
#![cfg_attr(all(target_arch = "aarch64", target_os = "none"), no_main)]

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn main() {}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod aarch64_monitor {
    use core::panic::PanicInfo;

    use r86::monitor::abi::{DOORBELL_IPA, MIN_ARM_IPA_BITS, MailboxV1};
    use r86::monitor::{
        DefaultMonitorShadowRuntime, HARDMMU_PAGED_ERROR_MARKER, HARDMMU_PAGED_SMOKE_MARKER,
        HARDMMU_PAGED_SMOKE_MAX_STEPS, HARDMMU_STATS_PARTS, MONITOR_SHADOW_L3_TABLES,
        MailboxClient, MmioDoorbell, MonitorBoot, MonitorHardMmuStatsPacket, MonitorRam,
        MonitorShadowStage1Tables, MonitorStage1Tables, MonitorStopAckPacket,
        NATIVE_HARDMMU_READ_MARKER, NULL_HARDMMU_ERROR_MARKER, NULL_HARDMMU_STEP_MARKER,
        SerialFlushPacket, enable_identity_stage1_with_shadow_from_boot,
        hardmmu_paged_smoke_registers_match, install_exception_panic_transport, monitor_panic_halt,
        run_hardmmu_large_paged_smoke, run_hardmmu_paged_smoke,
    };

    static mut STAGE1_TABLES: MonitorStage1Tables = MonitorStage1Tables::new();
    static mut SHADOW_STAGE1: MonitorShadowStage1Tables<MONITOR_SHADOW_L3_TABLES> =
        MonitorShadowStage1Tables::new();
    static mut SHADOW_RUNTIME: DefaultMonitorShadowRuntime =
        DefaultMonitorShadowRuntime::new(core::ptr::null_mut());

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_monitor_main(boot_block_ipa: u64) -> ! {
        unsafe {
            run(boot_block_ipa);
        }
        idle_forever()
    }

    unsafe fn run(boot_block_ipa: u64) {
        let boot = unsafe { &*(boot_block_ipa as *const r86::monitor::abi::BootBlockV1) };
        let Ok(parsed) = MonitorBoot::from_boot_block(boot_block_ipa, boot, MIN_ARM_IPA_BITS)
        else {
            return;
        };
        if unsafe {
            enable_identity_stage1_with_shadow_from_boot(
                core::ptr::addr_of_mut!(STAGE1_TABLES),
                core::ptr::addr_of_mut!(SHADOW_STAGE1),
                boot,
                MIN_ARM_IPA_BITS,
            )
        }
        .is_err()
        {
            return;
        }
        let mailbox = unsafe {
            core::slice::from_raw_parts_mut(
                parsed.mailbox_ipa as *mut u8,
                core::mem::size_of::<MailboxV1>(),
            )
        };
        install_exception_panic_transport(mailbox.as_mut_ptr(), DOORBELL_IPA as *mut u32);
        let Ok(mut client) = MailboxClient::new(mailbox) else {
            return;
        };
        let mut doorbell = MmioDoorbell::new(DOORBELL_IPA as *mut u32);
        if client.hello(&mut doorbell, parsed.feature_bits, 0).is_err() {
            return;
        }

        let shadow = unsafe { &mut *core::ptr::addr_of_mut!(SHADOW_RUNTIME) };
        shadow.bind_stage1(core::ptr::addr_of_mut!(SHADOW_STAGE1));
        #[cfg(all(feature = "machine", feature = "jit-profile"))]
        let shadow = {
            // These temporary Machines own this part of the fixed RAM memslot
            // until they return. Create MonitorRam only after they are gone.
            assert!(parsed.ram_size >= 0x100000);
            let ram =
                unsafe { core::slice::from_raw_parts_mut(parsed.ram_base as *mut u8, 0x100000) };
            let shadow = r86::machine::jit_checks::run_read_checks(ram, shadow, |message| {
                for chunk in message
                    .as_bytes()
                    .chunks(r86::monitor::SERIAL_FLUSH_PACKET_CAPACITY)
                {
                    client
                        .serial_flush(&mut doorbell, SerialFlushPacket::from_bytes(chunk).unwrap())
                        .unwrap();
                }
            });
            client
                .serial_flush(
                    &mut doorbell,
                    SerialFlushPacket::from_bytes(b"R86_JIT_READS_OK\n").unwrap(),
                )
                .unwrap();
            shadow
        };
        let Ok(mut ram) = (unsafe { MonitorRam::from_raw_parts(parsed.ram_base, parsed.ram_size) })
        else {
            return;
        };
        let mut cpu = r86::x86::Cpu::new();
        run_hardmmu_large_paged_smoke(
            &mut client,
            &mut doorbell,
            &mut ram,
            parsed.regions,
            &mut cpu,
            shadow,
            HARDMMU_PAGED_SMOKE_MAX_STEPS,
        )
        .expect("large-page architectural regression");
        let packet = SerialFlushPacket::from_bytes(b"R86_HMMU_LARGE_OK\n").unwrap();
        client.serial_flush(&mut doorbell, packet).unwrap();
        let result = run_hardmmu_paged_smoke(
            &mut client,
            &mut doorbell,
            &mut ram,
            parsed.regions,
            &mut cpu,
            shadow,
            HARDMMU_PAGED_SMOKE_MAX_STEPS,
        );
        let report = result.as_ref().ok();
        let stats = shadow.stats();
        let native_hits = report
            .map(|report| report.bus.native_read_hits)
            .unwrap_or(0);
        let ok = match report {
            Some(report) => {
                report.halted
                    && hardmmu_paged_smoke_registers_match(&cpu)
                    && stats.context_hits >= 5
                    && stats.stage1_installs != 0
                    && native_hits >= 8
                    && stats.native_read_descriptor_aborts == 0
                    && stats.shadow_errors == 0
            }
            None => false,
        };

        let _ = client.stop_ack(
            &mut doorbell,
            MonitorStopAckPacket::from_cpu_and_report(&cpu, report),
        );
        for part in 0..HARDMMU_STATS_PARTS {
            if let Some(packet) = MonitorHardMmuStatsPacket::from_cpu_report_and_shadow_part(
                &cpu, report, stats, part,
            ) {
                let _ = client.hardmmu_stats(&mut doorbell, packet);
            }
        }
        if native_hits != 0 {
            if let Ok(packet) = SerialFlushPacket::from_bytes(NATIVE_HARDMMU_READ_MARKER) {
                let _ = client.serial_flush(&mut doorbell, packet);
            }
        }
        let marker = if ok {
            HARDMMU_PAGED_SMOKE_MARKER
        } else {
            HARDMMU_PAGED_ERROR_MARKER
        };
        if let Ok(packet) = SerialFlushPacket::from_bytes(marker) {
            let _ = client.serial_flush(&mut doorbell, packet);
        }
        let null_marker = if ok {
            NULL_HARDMMU_STEP_MARKER
        } else {
            NULL_HARDMMU_ERROR_MARKER
        };
        if let Ok(packet) = SerialFlushPacket::from_bytes(null_marker) {
            let _ = client.serial_flush(&mut doorbell, packet);
        }
        let _ = client.halt(&mut doorbell, cpu.tsc());
    }

    fn idle_forever() -> ! {
        loop {
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
            }
        }
    }

    #[panic_handler]
    fn panic(info: &PanicInfo<'_>) -> ! {
        let (line, column) = info
            .location()
            .map(|loc| (loc.line(), loc.column()))
            .unwrap_or_default();
        monitor_panic_halt(0xffff_0000, u64::from(line), u64::from(column), 0)
    }
}
