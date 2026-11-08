#![cfg_attr(all(target_arch = "aarch64", target_os = "none"), no_std)]
#![cfg_attr(all(target_arch = "aarch64", target_os = "none"), no_main)]

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn main() {}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod aarch64_monitor {
    use core::panic::PanicInfo;

    use r86::monitor::abi::{
        CpuStateV1, DOORBELL_IPA, MIN_ARM_IPA_BITS, MailboxV1,
        debug::{DebugPhaseV1, DebugStateBlockV1, PanicBlockV1},
    };
    use r86::monitor::{
        DefaultMonitorShadowRuntime, HARDMMU_STATS_PARTS, MONITOR_SHADOW_L3_TABLES, MailboxClient,
        MmioDoorbell, MonitorBoot, MonitorDebugPublisher, MonitorHardMmuStatsPacket, MonitorRam,
        MonitorShadowStage1Tables, MonitorStage1Tables, MonitorStopAckPacket,
        NATIVE_HARDMMU_READ_MARKER, NULL_HARDMMU_DEFAULT_STEPS, NULL_HARDMMU_ERROR_MARKER,
        NULL_HARDMMU_STEP_MARKER, SerialFlushPacket, cpu_from_state_v1, cpu_state_v1_from_cpu,
        enable_identity_stage1_with_shadow_from_boot, install_exception_panic_transport_with_debug,
        monitor_panic_halt, run_x86_monitor_step_window_with_shadow_and_debug,
        send_x86_monitor_error_report,
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
        assert_eq!(
            parsed.feature_bits & r86::monitor::abi::FEATURE_MACHINE_DEVICES,
            0,
            "legacy monitor cannot use shared Machine device ownership"
        );
        let Ok(mut client) = MailboxClient::new(mailbox) else {
            return;
        };
        let mut doorbell = MmioDoorbell::new(DOORBELL_IPA as *mut u32);
        if client.hello(&mut doorbell, parsed.feature_bits, 0).is_err() {
            return;
        }
        debug.publish_phase(DebugPhaseV1::Hello);

        let initial = unsafe { &*(parsed.cpu_state_ipa as *const CpuStateV1) };
        let Ok(mut ram) = (unsafe { MonitorRam::from_raw_parts(parsed.ram_base, parsed.ram_size) })
        else {
            return;
        };
        let mut cpu = cpu_from_state_v1(initial);
        let shadow = unsafe { &mut *core::ptr::addr_of_mut!(SHADOW_RUNTIME) };
        shadow.bind_stage1(core::ptr::addr_of_mut!(SHADOW_STAGE1));
        shadow.reset(&cpu);
        let max_steps = if parsed.step_limit == 0 {
            NULL_HARDMMU_DEFAULT_STEPS
        } else {
            parsed.step_limit
        };
        let result = run_x86_monitor_step_window_with_shadow_and_debug(
            &mut client,
            &mut doorbell,
            &mut ram,
            parsed.regions,
            &mut cpu,
            shadow,
            max_steps,
            parsed.timer_poll_interval,
            &mut debug,
        );

        let final_steps = result.as_ref().map(|report| report.steps).unwrap_or(0);
        unsafe {
            (parsed.cpu_state_ipa as *mut CpuStateV1)
                .write_volatile(cpu_state_v1_from_cpu(&cpu, final_steps));
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        let _ = client.stop_ack(
            &mut doorbell,
            MonitorStopAckPacket::from_cpu_and_report(&cpu, result.as_ref().ok()),
        );
        for part in 0..HARDMMU_STATS_PARTS {
            if let Some(packet) = MonitorHardMmuStatsPacket::from_cpu_report_and_shadow_part(
                &cpu,
                result.as_ref().ok(),
                shadow.stats(),
                part,
            ) {
                let _ = client.hardmmu_stats(&mut doorbell, packet);
            }
        }
        if shadow.stats().native_read_hits != 0 {
            if let Ok(packet) = SerialFlushPacket::from_bytes(NATIVE_HARDMMU_READ_MARKER) {
                let _ = client.serial_flush(&mut doorbell, packet);
            }
        }
        if let Err(error) = result {
            send_x86_monitor_error_report(&mut client, &mut doorbell, error);
        }
        let marker = if result.is_ok() {
            NULL_HARDMMU_STEP_MARKER
        } else {
            NULL_HARDMMU_ERROR_MARKER
        };
        if let Ok(packet) = SerialFlushPacket::from_bytes(marker) {
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
    fn panic(_: &PanicInfo<'_>) -> ! {
        monitor_panic_halt(0xffff_0000, 0, 0, 0)
    }
}
