//! Linux/KVM host for the standard EL1 firmware observer. Every invocation
//! creates fresh backing and an empty JIT arena; compilation is timed.
//! Guest clocks are deterministic; these are engine comparisons, not normal
//! realtime boot measurements. The host counter schedules observations only.
//! Usage: ISO BIOS VGA_ROM MONITOR_ELF OUTPUT.ppm [SECONDS] [FRAME_MS]
use r86::kvm::{
    boot::{DirectBootConfig, prepare_firmware_boot},
    monitor::run_prepared_null_monitor_smoke,
    probe::ProbeConfig,
};
use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !(5..=7).contains(&args.len()) {
        return Err("expected ISO BIOS VGA_ROM MONITOR_ELF OUTPUT.ppm [SECONDS] [FRAME_MS]".into());
    }
    let seconds = args.get(5).map_or(Ok(180), |s| s.parse::<u64>())?;
    let frame_ms = args.get(6).map_or(Ok(100), |s| s.parse::<u64>())?;
    if seconds == 0 || frame_ms == 0 {
        return Err("timeout and frame interval must be positive".into());
    }
    // Explicit fixed-step diagnostic export; absent in all measured runs.
    let checkpoint = std::env::var("R86_BENCH_STEPS")
        .ok()
        .map(|s| s.parse::<u64>())
        .transpose()?;
    let state = std::env::var_os("R86_BENCH_STATE").map(PathBuf::from);
    if checkpoint.is_some() != state.is_some() || checkpoint == Some(0) {
        return Err("checkpoint requires positive R86_BENCH_STEPS and R86_BENCH_STATE".into());
    }
    eprintln!("benchmark.clock=deterministic");
    let probe = ProbeConfig {
        ram_size: 128 * 1024 * 1024,
        monitor_size: 384 * 1024 * 1024,
        shared_size: 16 * 1024 * 1024,
        trace_size: 448 * 1024 * 1024,
        machine_monitor: true,
        create_vm: true,
        register_memslots: true,
        ..Default::default()
    };
    let boot = DirectBootConfig {
        iso_path: PathBuf::from(&args[0]),
        step_limit: checkpoint.unwrap_or(20_000_000_000),
        serial_success_marker: Some(
            if checkpoint.is_some() {
                "R86_REACTOS_CHECKPOINT"
            } else {
                "R86_REACTOS_DONE"
            }
            .into(),
        ),
        serial_stdout: true,
        frame_out: Some(PathBuf::from(&args[4])),
        machine_state_out: state,
        timeout_secs: Some(seconds),
        stall_timeout_secs: Some(30),
        ..Default::default()
    };
    let prepared = prepare_firmware_boot(
        &probe,
        &boot,
        Path::new(&args[1]),
        Path::new(&args[2]),
        frame_ms,
    )?;
    let report =
        run_prepared_null_monitor_smoke(&probe, prepared, Path::new(&args[3]), &boot, None)?;
    println!(
        "hardmmu steps={:?} stats={:?}",
        report.stop_ack, report.hardmmu_stats
    );
    Ok(())
}
