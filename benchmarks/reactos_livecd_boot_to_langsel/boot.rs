//! Headless firmware boot to the complete ReactOS language-dialog pixel gate.
//!
//! Usage: ISO BIOS VGA_ROM interp|jit OUTPUT.ppm [SECONDS] [FRAME_MS] [realtime|deterministic]
//! The production Machine executes unchanged guest input without keyboard events.
//! Normal boot timing uses the production realtime device clock. Deterministic
//! engine comparisons and fixed-step checkpoints are separate measurements.
//! Compare with QEMU headless full-frame observation at the same cadence;
//! include loading/compilation in external process timing and preserve the frame.
//! With the monitor feature, R86_BENCH_STEPS + R86_BENCH_STATE instead select
//! an untimed fixed-step CPU/RAM/device checkpoint for cross-backend comparison.
use r86::FirmwareDisplay;
use r86::machine::{GuestStepStatus, Machine};
use std::io::Write;
use std::time::{Duration, Instant};

fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !(5..=8).contains(&args.len()) || !matches!(args[3].as_str(), "interp" | "jit") {
        return Err("expected ISO BIOS VGA_ROM interp|jit OUTPUT.ppm [SECONDS] [FRAME_MS] [realtime|deterministic]".into());
    }
    let seconds = args
        .get(5)
        .map_or(Ok(600), |s| s.parse::<u64>())
        .map_err(|e| e.to_string())?;
    let frame_ms = args
        .get(6)
        .map_or(Ok(2000), |s| s.parse::<u64>())
        .map_err(|e| e.to_string())?;
    if seconds == 0 || frame_ms == 0 {
        return Err("timeout and frame interval must be positive".into());
    }
    let start = Instant::now();
    #[cfg(not(feature = "monitor"))]
    if std::env::var_os("R86_BENCH_STEPS").is_some()
        || std::env::var_os("R86_BENCH_STATE").is_some()
    {
        return Err("fixed-step export requires the monitor feature".into());
    }
    #[cfg(feature = "monitor")]
    let checkpoint = std::env::var("R86_BENCH_STEPS")
        .ok()
        .map(|s| s.parse::<u64>())
        .transpose()
        .map_err(|e| e.to_string())?;
    #[cfg(not(feature = "monitor"))]
    let checkpoint: Option<u64> = None;
    #[cfg(feature = "monitor")]
    let state = std::env::var_os("R86_BENCH_STATE").map(std::path::PathBuf::from);
    #[cfg(feature = "monitor")]
    if checkpoint.is_some() != state.is_some() || checkpoint == Some(0) {
        return Err("checkpoint requires positive R86_BENCH_STEPS and R86_BENCH_STATE".into());
    }
    let clock = args
        .get(7)
        .map(String::as_str)
        .unwrap_or(if checkpoint.is_some() {
            "deterministic"
        } else {
            "realtime"
        });
    if !matches!(clock, "realtime" | "deterministic") {
        return Err("clock must be realtime or deterministic".into());
    }
    if checkpoint.is_some() && clock != "deterministic" {
        return Err("fixed-step checkpoints require a deterministic clock".into());
    }
    eprintln!("benchmark.clock={clock}");
    let points: Vec<_> = include_str!("langsel.rgb")
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let fields: Vec<usize> = line
                .split_whitespace()
                .map(|field| field.parse().expect("embedded endpoint coordinate"))
                .collect();
            assert_eq!(fields.len(), 5, "embedded endpoint row");
            (
                fields[0],
                fields[1],
                [fields[2] as u8, fields[3] as u8, fields[4] as u8],
            )
        })
        .collect();
    let read = |p: &str| std::fs::read(p).map_err(|e| format!("{p}: {e}"));
    let mut machine = Machine::new_firmware(128 * 1024 * 1024, FirmwareDisplay::Vga)?;
    machine.attach_primary_cdrom(read(&args[0])?);
    machine.load_bios_rom(&read(&args[1])?)?;
    machine.load_option_rom(0xc0000, &read(&args[2])?)?;
    machine.reset_bios();
    machine.set_jit_enabled(args[3] == "jit")?;
    if clock == "realtime" {
        machine.use_realtime_clock()?;
    }
    let mut last_frame = start;
    let mut last_progress = start;
    let mut pixels = Vec::new();
    let mut polls = 0;
    let mut matched = false;
    let mut size = (0, 0);
    while start.elapsed() < Duration::from_secs(seconds) {
        let budget =
            checkpoint.map_or(100_000, |steps| (steps - machine.step_count()).min(100_000));
        let report = machine.run_steps(budget)?;
        if let Some(stop) = report.stop {
            if !matches!(stop, GuestStepStatus::Halted) {
                return Err(format!("guest stopped: {stop:?}"));
            }
            if clock == "realtime" {
                std::thread::sleep(machine.halted_wait_duration());
            }
        }
        let at_checkpoint = checkpoint == Some(machine.step_count());
        if last_frame.elapsed() < Duration::from_millis(frame_ms) && !at_checkpoint {
            continue;
        }
        last_frame = Instant::now();
        let mode = machine.vga().mode_info();
        let (width, height) = (usize::from(mode.width), usize::from(mode.height));
        size = (width, height);
        pixels.resize(width * height * 4, 0);
        machine
            .vga()
            .render_rgba(&mut pixels)
            .ok_or("VGA render failed")?;
        polls += 1;
        matched = (width, height) == (800, 600)
            && points.iter().all(|&(x, y, rgb)| {
                x < width && y < height && pixels[(y * width + x) * 4..][..3] == rgb
            });
        if at_checkpoint {
            break;
        }
        if matched && checkpoint.is_none() {
            // The observer timestamps this line before screenshot serialization.
            println!(
                "REACTOS_LANGUAGE_DIALOG elapsed={:.6} steps={} polls={polls}",
                start.elapsed().as_secs_f64(),
                machine.step_count()
            );
            std::io::stdout().flush().map_err(|e| e.to_string())?;
            break;
        }
        if last_progress.elapsed() >= Duration::from_secs(10) {
            eprintln!(
                "progress elapsed={:.3} steps={} eip={:#010x} size={}x{}",
                start.elapsed().as_secs_f64(),
                machine.step_count(),
                machine.cpu().eip,
                width,
                height
            );
            last_progress = Instant::now();
        }
    }
    let (width, height) = size;
    let mut file =
        std::io::BufWriter::new(std::fs::File::create(&args[4]).map_err(|e| e.to_string())?);
    write!(file, "P6\n{width} {height}\n255\n").map_err(|e| e.to_string())?;
    for rgba in pixels.chunks_exact(4) {
        file.write_all(&rgba[..3]).map_err(|e| e.to_string())?;
    }
    file.flush().map_err(|e| e.to_string())?;
    #[cfg(feature = "monitor")]
    if let Some(path) = state {
        if checkpoint != Some(machine.step_count()) {
            return Err("fixed-step checkpoint not reached".into());
        }
        let cpu = r86::monitor::cpu_state_v1_from_cpu(machine.cpu(), machine.step_count());
        std::fs::write(path.with_extension("cpu"), format!("{cpu:?}\n"))
            .map_err(|e| e.to_string())?;
        std::fs::write(path.with_extension("ram"), machine.ram()).map_err(|e| e.to_string())?;
        let mut devices = std::io::BufWriter::new(
            std::fs::File::create(path.with_extension("devices")).map_err(|e| e.to_string())?,
        );
        writeln!(devices, "{:?}", machine.device_state_for_comparison())
            .map_err(|e| e.to_string())?;
        devices.flush().map_err(|e| e.to_string())?;
        println!("R86_REACTOS_CHECKPOINT");
    }
    println!("{}", machine.jit_report());
    println!(
        "final steps={} eip={:#010x} resets={}",
        machine.step_count(),
        machine.cpu().eip,
        machine.reset_count()
    );
    if !matched && checkpoint.is_none() {
        return Err("language dialog not reached before timeout".into());
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("reactos_boot_benchmark: {error}");
        std::process::exit(1);
    }
}
