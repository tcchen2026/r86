//! Real guests compare clocks during CPU work, HLT, host pauses and reset. Run explicitly:
//! cargo test --release --test clock -- --include-ignored --nocapture
#![cfg(feature = "host")]
use r86::FirmwareDisplay;
use r86::machine::{GuestStepStatus, Machine};
use std::time::{Duration, Instant};

#[test]
#[ignore = "wall-clock pause/reset integration; requires about 2.5 seconds"]
fn realtime_pause_and_reset_preserve_device_time_and_reset_cpu_time() {
    let mut machine = Machine::new_firmware(2 << 20, FirmwareDisplay::Serial).unwrap();
    machine
        .load_bootsector(include_bytes!("bootsectors/clock_reset.bin"))
        .unwrap();
    machine.enter_bootsector();
    machine.use_realtime_clock().unwrap();
    let until_marker = |machine: &mut Machine, marker| {
        let report = machine.run_until_serial_output(10_000).unwrap();
        assert!(report.stop.is_none(), "{report:?}");
        assert_eq!(machine.serial_output().last(), Some(&marker));
    };
    until_marker(&mut machine, b'S');
    let paused_at = machine.step_count();
    let pause = Instant::now();
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(machine.step_count(), paused_at);
    until_marker(&mut machine, b'P');
    let elapsed = pause.elapsed().as_secs_f64();
    until_marker(&mut machine, b'A');
    assert!(matches!(
        machine.run_steps(100).unwrap().stop,
        Some(GuestStepStatus::Halted)
    ));
    let paused_end = machine.step_count();
    machine.reboot().unwrap();
    assert_eq!(machine.step_count(), paused_end);
    assert!(machine.uses_realtime_clock());
    until_marker(&mut machine, b'R');
    assert_eq!(machine.serial_output(), b"SPAR");

    let ram = machine.ram();
    let pm = |p: usize| u32::from_le_bytes(ram[p..p + 4].try_into().unwrap());
    let tsc = |p: usize| u64::from_le_bytes(ram[p + 4..p + 12].try_into().unwrap());
    let seconds = |a: usize, b: usize| (ram[b + 12] + 60 - ram[a + 12]) % 60;
    let pm_elapsed = f64::from(pm(0x540).wrapping_sub(pm(0x520)) & 0xffffff) / 3_579_546.0;
    let tsc_elapsed = (tsc(0x540) - tsc(0x520)) as f64 / 1_000_000_000.0;
    println!(
        "pause wall={elapsed:.4} PM={pm_elapsed:.4} TSC={tsc_elapsed:.4} RTC={}s",
        seconds(0x520, 0x540)
    );
    assert!((pm_elapsed - elapsed).abs() < 0.1);
    assert!((tsc_elapsed - elapsed).abs() < 0.1);
    assert!((f64::from(seconds(0x520, 0x540)) - elapsed).abs() < 1.1);
    assert_eq!(ram[0x520 + 12], 20);
    assert_eq!(ram[0x540 + 13] & 0x90, 0x90); // update-ended event during pause
    assert_eq!(ram[0x540 + 14] & 1, 1); // PM MSB transition during pause
    assert_eq!(ram[0x560 + 13] & 0x90, 0); // RTC acknowledgement consumed it
    assert_eq!(ram[0x560 + 14] & 1, 0); // PM W1C consumed it
    assert_eq!(ram[0x580 + 13] & 0x80, 0); // reset clears RTC IRQ enables
    assert_eq!(ram[0x580 + 14] & 1, 0); // reset does not replay old PM events
    assert!(pm(0x580).wrapping_sub(pm(0x560)) & 0xffffff < 357_955);
    assert!(
        seconds(0x560, 0x580) <= 1,
        "reset must retain the guest RTC"
    );
    assert!((10_000_000_000..10_100_000_000).contains(&tsc(0x520)));
    assert!(
        tsc(0x580) < 100_000_000,
        "CPU reset must clear the written TSC offset"
    );
}

#[test]
#[ignore = "wall-clock guest integration; requires about 2.4 seconds"]
fn realtime_clocks_do_not_follow_cpu_throughput() {
    let mut machine = Machine::new_firmware(2 << 20, FirmwareDisplay::Serial).unwrap();
    machine
        .load_bootsector(include_bytes!("bootsectors/clock.bin"))
        .unwrap();
    machine.enter_bootsector();
    machine.use_realtime_clock().unwrap();
    #[cfg(all(feature = "jit", target_arch = "aarch64"))]
    machine.set_jit_enabled(true).unwrap();
    let start = Instant::now();
    let mut observations = Vec::new();
    loop {
        let report = machine.run_until_serial_output(50_000).unwrap();
        if machine.serial_output().len() > observations.len() {
            observations.push((start.elapsed().as_secs_f64(), machine.step_count()));
        }
        match report.stop {
            Some(GuestStepStatus::PoweredOff) => break,
            Some(GuestStepStatus::Halted) => std::thread::sleep(machine.halted_wait_duration()),
            None => {}
            Some(other) => panic!("unexpected stop: {other:?}"),
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "guest clock stopped"
        );
    }
    #[cfg(all(feature = "jit", target_arch = "aarch64"))]
    {
        let report = machine.jit_report();
        println!("{report}");
        let native: u64 = report
            .split("native_steps: ")
            .nth(1)
            .unwrap()
            .split(|ch: char| !ch.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            native > 100_000,
            "clock test must actually execute native blocks"
        );
    }
    assert_eq!(machine.serial_output(), b"SBI");
    let read32 = |addr| u32::from_le_bytes(machine.ram()[addr..addr + 4].try_into().unwrap());
    for phase in 0..2 {
        let a = 0x520 + phase * 32;
        let b = a + 32;
        let wall = observations[phase + 1].0 - observations[phase].0;
        let instructions = observations[phase + 1].1 - observations[phase].1;
        let pm = f64::from(read32(b).wrapping_sub(read32(a)) & 0xffffff) / 3_579_546.0;
        let tsc = |p| u64::from(read32(p + 4)) | (u64::from(read32(p + 8)) << 32);
        let tsc_seconds = tsc(b).wrapping_sub(tsc(a)) as f64 / 1_000_000_000.0;
        let seconds = (machine.ram()[b + 12] + 60 - machine.ram()[a + 12]) % 60;
        let pit_irqs = read32(b + 16) - read32(a + 16);
        let rtc_irqs = read32(b + 20) - read32(a + 20);
        println!(
            "phase={phase} wall={wall:.4} PM={pm:.4} TSC={tsc_seconds:.4} RTC={seconds}s PIT={pit_irqs} RTC_IRQ={rtc_irqs} instructions={instructions}"
        );
        assert!((wall - pm).abs() < 0.1);
        assert!((tsc_seconds - pm).abs() < 0.02);
        assert!((1..=2).contains(&seconds));
        assert!((100..=125).contains(&pit_irqs));
        assert!((64..=80).contains(&rtc_irqs));
    }
    let busy = observations[1].1 - observations[0].1;
    let idle = observations[2].1 - observations[1].1;
    assert!(
        busy > idle * 100,
        "HLT must yield instead of spending a CPU budget"
    );
    assert_eq!(read32(0x540 + 24), read32(0x560 + 24));
}
