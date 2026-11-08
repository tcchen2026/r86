//! Firmware observer for the standard ReactOS language-dialog endpoint.
//! ARM host counter schedules observations only; guest time stays deterministic.
//! State-export runs stop at a fixed instruction count and are never timings.
#![cfg_attr(all(target_arch = "aarch64", target_os = "none"), no_std)]
#![cfg_attr(all(target_arch = "aarch64", target_os = "none"), no_main)]

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn main() {}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod aarch64_monitor {
    extern crate alloc;
    use core::fmt::Write as _;
    use core::panic::PanicInfo;
    use core::sync::atomic::Ordering;

    use alloc::vec::Vec;
    use r86::FirmwareDisplay;
    use r86::machine::{GuestStepStatus, Machine};
    use r86::monitor::abi::{
        CpuStateV1, DOORBELL_IPA, HostCommandKindV1, MIN_ARM_IPA_BITS, MailboxV1,
        debug::{DebugPhaseV1, DebugStateBlockV1, PanicBlockV1},
    };
    use r86::monitor::firmware::{FRAME_MAGIC, FRAME_OFFSET, INPUT_MAGIC, INPUT_OFFSET, Inputs};
    use r86::monitor::{
        DefaultMonitorShadowRuntime, HARDMMU_STATS_PARTS, MONITOR_SHADOW_L3_TABLES, MailboxClient,
        MmioDoorbell, MonitorBoot, MonitorDebugCpuSnapshot, MonitorDebugPublisher,
        MonitorDebugSnapshot, MonitorHardMmuStatsPacket, MonitorShadowStage1Tables,
        MonitorStage1Tables, MonitorStopAckPacket, NULL_HARDMMU_DEFAULT_STEPS,
        NULL_HARDMMU_ERROR_MARKER, NULL_HARDMMU_STEP_MARKER, SERIAL_FLUSH_PACKET_CAPACITY,
        SerialFlushPacket, X86MonitorBusStats, X86MonitorStepReport, cpu_state_v1_from_cpu,
        enable_identity_stage1_with_shadow_from_boot, install_exception_panic_transport_with_debug,
        monitor_panic_halt,
    };
    const BATCH_STEPS: u64 = 100_000;
    const DONE: &str = "R86_REACTOS_DONE";

    fn counter() -> u64 {
        let value: u64;
        unsafe {
            core::arch::asm!("isb", "mrs {value}, cntvct_el0", value = out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
    }
    fn frequency() -> u64 {
        let value: u64;
        unsafe {
            core::arch::asm!("mrs {value}, cntfrq_el0", value = out(reg) value, options(nomem, nostack, preserves_flags));
        }
        value
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
        let start = counter();
        let frequency = frequency();
        let ram = unsafe {
            core::slice::from_raw_parts_mut(parsed.ram_base as *mut u8, parsed.ram_size as usize)
        };
        assert_eq!(boot.trace_offset, 0);
        let trace_len = boot.layout.trace_size as usize;
        assert!(trace_len >= INPUT_OFFSET + core::mem::size_of::<Inputs>());
        let inputs =
            unsafe { ((parsed.trace_ipa as usize + INPUT_OFFSET) as *const Inputs).read() };
        assert_eq!(inputs.magic, INPUT_MAGIC);
        let frame_ticks = frequency
            .checked_mul(inputs.frame_ms)
            .expect("frame interval")
            / 1000;
        assert!(frame_ticks > 0);
        let asset = |offset: u64, len: u64| -> &'static [u8] {
            assert!(offset as usize >= INPUT_OFFSET + core::mem::size_of::<Inputs>());
            let end = offset.checked_add(len).expect("input end");
            assert!(end <= trace_len as u64);
            // Fixed VMM backing is immutable throughout monitor execution;
            // diagnostic output is restricted below INPUT_OFFSET.
            unsafe {
                core::slice::from_raw_parts((parsed.trace_ipa + offset) as *const u8, len as usize)
            }
        };
        let mut machine =
            Machine::from_firmware_ram(ram, FirmwareDisplay::Vga).expect("firmware PC");
        machine.attach_primary_cdrom_borrowed(asset(inputs.iso_offset, inputs.iso_len));
        machine
            .load_bios_rom(asset(inputs.bios_offset, inputs.bios_len))
            .expect("BIOS");
        machine
            .load_option_rom(0xc0000, asset(inputs.vga_offset, inputs.vga_len))
            .expect("VGA ROM");
        machine.reset_bios();
        let points: Vec<(usize, usize, [u8; 3])> = include_str!("langsel.rgb")
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let fields: Vec<usize> = line
                    .split_whitespace()
                    .map(|field| field.parse().expect("endpoint number"))
                    .collect();
                assert_eq!(fields.len(), 5);
                (
                    fields[0],
                    fields[1],
                    [fields[2] as u8, fields[3] as u8, fields[4] as u8],
                )
            })
            .collect();
        let mut pixels = Vec::new();
        let mut size = (0, 0);
        let mut polls = 0;
        let mut last_frame = start;
        let checkpoint = parsed.feature_bits & r86::monitor::abi::FEATURE_MACHINE_STATE_V1 != 0;
        let mut matched = false;
        let shadow = unsafe { &mut *core::ptr::addr_of_mut!(SHADOW_RUNTIME) };
        shadow.bind_stage1(core::ptr::addr_of_mut!(SHADOW_STAGE1));
        machine.enable_hardmmu(shadow);
        machine.set_jit_enabled(true).expect("EL1 JIT");
        let max_steps = if parsed.step_limit == 0 {
            NULL_HARDMMU_DEFAULT_STEPS
        } else {
            parsed.step_limit
        };
        let mut serial_sent = 0;
        let mut next_debug = 0;
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
            if machine.step_count() >= next_debug {
                next_debug = machine.step_count() + 1_000_000;
                debug.publish(MonitorDebugSnapshot {
                    phase: DebugPhaseV1::X86Step,
                    steps: machine.step_count(),
                    cpu: Some(MonitorDebugCpuSnapshot::from_cpu(machine.cpu())),
                    bus: bus_stats(&machine),
                    shadow: machine.hardmmu_stats(),
                    transport: client.stats(),
                });
            }
            let budget = (max_steps - machine.step_count()).min(BATCH_STEPS);
            let result = machine.run_steps(budget);
            flush(
                &mut client,
                &mut doorbell,
                &machine.serial_output()[serial_sent..],
            );
            serial_sent = machine.serial_output().len();
            match result {
                Ok(report) if report.stop.is_none() => {}
                Ok(report) if matches!(report.stop, Some(GuestStepStatus::Halted)) => {}
                Ok(_) => break,
                Err(error) => {
                    flush(&mut client, &mut doorbell, error.as_bytes());
                    succeeded = false;
                    break;
                }
            }
            if counter().wrapping_sub(last_frame) >= frame_ticks
                || machine.step_count() == max_steps
            {
                last_frame = counter();
                let mode = machine.vga().mode_info();
                let (width, height) = (usize::from(mode.width), usize::from(mode.height));
                size = (width, height);
                pixels.resize(width * height * 4, 0);
                machine
                    .vga()
                    .render_rgba(&mut pixels)
                    .expect("complete frame");
                polls += 1;
                matched = size == (800, 600)
                    && points.iter().all(|&(x, y, rgb)| {
                        x < width && y < height && pixels[(y * width + x) * 4..][..3] == rgb
                    });
                if matched && !checkpoint {
                    // Forwarded immediately by the VMM, before frame/state export.
                    let message = alloc::format!(
                        "REACTOS_LANGUAGE_DIALOG elapsed={:.6} steps={} polls={} {}\n",
                        counter().wrapping_sub(start) as f64 / frequency as f64,
                        machine.step_count(),
                        polls,
                        DONE
                    );
                    flush(&mut client, &mut doorbell, message.as_bytes());
                    break;
                }
            }
        }
        assert!(
            succeeded && (matched || checkpoint),
            "language dialog not reached"
        );
        flush(&mut client, &mut doorbell, machine.jit_report().as_bytes());
        if checkpoint {
            flush(&mut client, &mut doorbell, b"R86_REACTOS_CHECKPOINT\n");
        }
        let frame = unsafe {
            core::slice::from_raw_parts_mut(
                (parsed.trace_ipa as usize + FRAME_OFFSET) as *mut u8,
                INPUT_OFFSET - FRAME_OFFSET,
            )
        };
        assert!(pixels.len() + 32 <= frame.len());
        for (i, value) in [
            FRAME_MAGIC,
            size.0 as u64,
            size.1 as u64,
            pixels.len() as u64,
        ]
        .iter()
        .enumerate()
        {
            frame[i * 8..(i + 1) * 8].copy_from_slice(&value.to_le_bytes());
        }
        frame[32..32 + pixels.len()].copy_from_slice(&pixels);
        if parsed.feature_bits & r86::monitor::abi::FEATURE_MACHINE_STATE_V1 != 0 {
            let bytes = unsafe {
                core::slice::from_raw_parts_mut(parsed.trace_ipa as *mut u8, FRAME_OFFSET)
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
