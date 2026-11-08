use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use r86::initrd_overlay::{InitrdFile, entries_from_host_tree};
use r86::kvm::boot::{DirectBootConfig, prepare_tinycore_direct_boot};
use r86::kvm::doorbell::{run_doorbell_smoke, run_doorbell_state_probe};
use r86::kvm::error::{KvmError, Result};
use r86::kvm::memslot::{default_ipa_layout, round_up_4k};
use r86::kvm::monitor::{
    run_tinycore_hello_smoke, run_tinycore_null_monitor_smoke, run_tinycore_pio_smoke,
    run_tinycore_rust_monitor_smoke, run_tinycore_x86_monitor_smoke,
};
use r86::kvm::probe::{ProbeConfig, run_probe};
use r86::monitor::abi::TraceModeV1;

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("r86-kvm: {err}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<()> {
    let (mode, config, boot_config, monitor_elf) = parse_args(env::args().skip(1))?;
    match mode {
        Mode::Help => {
            print_usage();
            Ok(())
        }
        Mode::Plan => {
            let planned = config.validate_for_layout_only()?;
            print!("{planned}");
            Ok(())
        }
        Mode::Probe => {
            let report = run_probe(&config)?;
            print!("{report}");
            Ok(())
        }
        Mode::DoorbellSmoke => {
            let report = run_doorbell_smoke(&config)?;
            print!("{report}");
            Ok(())
        }
        Mode::DoorbellState => {
            let report = run_doorbell_state_probe(&config)?;
            print!("{report}");
            Ok(())
        }
        Mode::TinyCorePlan => {
            let report = prepare_tinycore_direct_boot(&config, &boot_config)?;
            print!("{report}");
            Ok(())
        }
        Mode::TinyCoreHelloSmoke => {
            let report = run_tinycore_hello_smoke(&config, &boot_config)?;
            print!("{report}");
            Ok(())
        }
        Mode::TinyCorePioSmoke => {
            let report = run_tinycore_pio_smoke(&config, &boot_config)?;
            print!("{report}");
            Ok(())
        }
        Mode::TinyCoreRustMonitorSmoke => {
            let monitor_elf = monitor_elf.ok_or_else(|| {
                KvmError::BadArgument(
                    "--tinycore-rust-monitor-smoke requires --monitor-elf PATH".to_string(),
                )
            })?;
            let report = run_tinycore_rust_monitor_smoke(&config, &boot_config, &monitor_elf)?;
            print!("{report}");
            Ok(())
        }
        Mode::TinyCoreX86MonitorSmoke => {
            let monitor_elf = monitor_elf.ok_or_else(|| {
                KvmError::BadArgument(
                    "--tinycore-x86-monitor-smoke requires --monitor-elf PATH".to_string(),
                )
            })?;
            let report = run_tinycore_x86_monitor_smoke(&config, &boot_config, &monitor_elf)?;
            print!("{report}");
            Ok(())
        }
        Mode::TinyCoreNullMonitorSmoke => {
            let monitor_elf = monitor_elf.ok_or_else(|| {
                KvmError::BadArgument(
                    "--tinycore-null-monitor-smoke requires --monitor-elf PATH".to_string(),
                )
            })?;
            let report = run_tinycore_null_monitor_smoke(&config, &boot_config, &monitor_elf)?;
            print!("{report}");
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Help,
    Plan,
    Probe,
    DoorbellSmoke,
    DoorbellState,
    TinyCorePlan,
    TinyCoreHelloSmoke,
    TinyCorePioSmoke,
    TinyCoreRustMonitorSmoke,
    TinyCoreX86MonitorSmoke,
    TinyCoreNullMonitorSmoke,
}

fn parse_args<I>(args: I) -> Result<(Mode, ProbeConfig, DirectBootConfig, Option<PathBuf>)>
where
    I: IntoIterator<Item = String>,
{
    let mut mode = Mode::Probe;
    let mut config = ProbeConfig::default();
    let mut boot_config = DirectBootConfig::default();
    let mut monitor_elf = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => mode = Mode::Help,
            "--plan" => mode = Mode::Plan,
            "--probe" => mode = Mode::Probe,
            "--doorbell-smoke" => mode = Mode::DoorbellSmoke,
            "--doorbell-state" => mode = Mode::DoorbellState,
            "--tinycore-plan" => mode = Mode::TinyCorePlan,
            "--tinycore-hello-smoke" => mode = Mode::TinyCoreHelloSmoke,
            "--tinycore-pio-smoke" => mode = Mode::TinyCorePioSmoke,
            "--tinycore-rust-monitor-smoke" => mode = Mode::TinyCoreRustMonitorSmoke,
            "--tinycore-x86-monitor-smoke" => mode = Mode::TinyCoreX86MonitorSmoke,
            "--tinycore-null-monitor-smoke" => mode = Mode::TinyCoreNullMonitorSmoke,
            "--machine-monitor" => {
                mode = Mode::TinyCoreNullMonitorSmoke;
                config.machine_monitor = true;
            }
            "--create-vm" => config.create_vm = true,
            "--register-memslots" => {
                config.create_vm = true;
                config.register_memslots = true;
            }
            "--ram-mib" => config.ram_size = parse_mib("--ram-mib", next_arg(&mut iter, &arg)?)?,
            "--monitor-mib" => {
                config.monitor_size = parse_mib("--monitor-mib", next_arg(&mut iter, &arg)?)?
            }
            "--shared-mib" => {
                config.shared_size = parse_mib("--shared-mib", next_arg(&mut iter, &arg)?)?
            }
            "--trace-mib" => {
                config.trace_size = parse_mib("--trace-mib", next_arg(&mut iter, &arg)?)?
            }
            "--ipa-bits" => {
                let value = next_arg(&mut iter, &arg)?;
                config.requested_ipa_bits =
                    Some(value.parse::<u8>().map_err(|_| KvmError::ParseArgument {
                        name: "--ipa-bits",
                        value,
                    })?);
            }
            "--iso" => boot_config.iso_path = next_arg(&mut iter, &arg)?.into(),
            "--snapshot-in" => boot_config.snapshot_in = Some(next_arg(&mut iter, &arg)?.into()),
            "--snapshot-out" => boot_config.snapshot_out = Some(next_arg(&mut iter, &arg)?.into()),
            "--machine-state-out" => {
                boot_config.machine_state_out = Some(next_arg(&mut iter, &arg)?.into())
            }
            "--monitor-elf" => monitor_elf = Some(next_arg(&mut iter, &arg)?.into()),
            "--cmdline" => boot_config.cmdline = next_arg(&mut iter, &arg)?,
            "--initrd-file" => {
                let spec = next_arg(&mut iter, &arg)?;
                boot_config
                    .initrd_files
                    .push(InitrdFile::from_host_spec(&spec).map_err(KvmError::BadArgument)?);
            }
            "--initrd-tree" => {
                let spec = next_arg(&mut iter, &arg)?;
                boot_config
                    .initrd_files
                    .extend(entries_from_host_tree(&spec).map_err(KvmError::BadArgument)?);
            }
            "--bootlocal-command" => {
                boot_config.bootlocal_command = Some(next_arg(&mut iter, &arg)?)
            }
            "--serial-success-marker" => {
                boot_config.serial_success_marker = Some(next_arg(&mut iter, &arg)?)
            }
            "--stop-on-serial-marker" => {
                boot_config.stop_on_serial_marker = Some(next_arg(&mut iter, &arg)?)
            }
            "--stop-at-step" => {
                boot_config.stop_at_step =
                    Some(parse_u64("--stop-at-step", next_arg(&mut iter, &arg)?)?)
            }
            "--host-decompress-kernel" => boot_config.host_decompress_kernel = true,
            "--guest-decompress-initrd" => boot_config.host_decompress_initrd = false,
            "--host-decompress-initrd" => boot_config.host_decompress_initrd = true,
            "--step-limit" => {
                boot_config.step_limit = parse_u64("--step-limit", next_arg(&mut iter, &arg)?)?
            }
            "--timer-poll-interval" => {
                boot_config.timer_poll_interval =
                    parse_u64("--timer-poll-interval", next_arg(&mut iter, &arg)?)?
            }
            "--trace-mode" => {
                boot_config.trace_mode = parse_trace_mode(next_arg(&mut iter, &arg)?)?
            }
            "--progress" => boot_config.progress = true,
            "--timeout-secs" => {
                boot_config.timeout_secs =
                    Some(parse_u64("--timeout-secs", next_arg(&mut iter, &arg)?)?)
            }
            "--stall-timeout-secs" => {
                boot_config.stall_timeout_secs = Some(parse_u64(
                    "--stall-timeout-secs",
                    next_arg(&mut iter, &arg)?,
                )?)
            }
            "--json" => boot_config.json_progress = true,
            _ => {
                return Err(KvmError::BadArgument(format!(
                    "unknown argument {arg:?}; use --help"
                )));
            }
        }
    }
    let rounded = default_ipa_layout(round_up_4k(config.ram_size));
    config.ram_size = rounded.ram_size;
    config.monitor_size = round_up_4k(config.monitor_size);
    config.shared_size = round_up_4k(config.shared_size);
    config.trace_size = round_up_4k(config.trace_size);
    Ok((mode, config, boot_config, monitor_elf))
}

fn next_arg<I>(iter: &mut I, name: &str) -> Result<String>
where
    I: Iterator<Item = String>,
{
    iter.next()
        .ok_or_else(|| KvmError::BadArgument(format!("{name} requires a value")))
}

fn parse_mib(name: &'static str, value: String) -> Result<u64> {
    let mib = value
        .parse::<u64>()
        .map_err(|_| KvmError::ParseArgument { name, value })?;
    mib.checked_mul(1024 * 1024)
        .ok_or_else(|| KvmError::BadArgument(format!("{name} value is too large")))
}

fn parse_u64(name: &'static str, value: String) -> Result<u64> {
    value
        .parse::<u64>()
        .map_err(|_| KvmError::ParseArgument { name, value })
}

fn parse_trace_mode(value: String) -> Result<TraceModeV1> {
    match value.as_str() {
        "off" => Ok(TraceModeV1::Off),
        "arch" | "architectural" => Ok(TraceModeV1::Architectural),
        "full" | "implementation" | "architectural-and-implementation" => {
            Ok(TraceModeV1::ArchitecturalAndImplementation)
        }
        _ => Err(KvmError::ParseArgument {
            name: "--trace-mode",
            value,
        }),
    }
}

fn print_usage() {
    println!(
        "usage: r86-kvm [--plan|--probe|--doorbell-smoke|--doorbell-state|--tinycore-plan|--tinycore-hello-smoke|--tinycore-pio-smoke|--tinycore-rust-monitor-smoke|--tinycore-x86-monitor-smoke|--tinycore-null-monitor-smoke|--machine-monitor] [--create-vm] [--register-memslots] [--ram-mib N] [--monitor-mib N] [--shared-mib N] [--trace-mib N] [--ipa-bits N] [--iso PATH] [--snapshot-in PATH] [--snapshot-out PATH] [--machine-state-out STEM] [--monitor-elf PATH] [--cmdline TEXT] [--initrd-file HOST:GUEST[:MODE]] [--initrd-tree HOST[:GUEST]] [--bootlocal-command TEXT] [--serial-success-marker TEXT] [--stop-on-serial-marker TEXT] [--stop-at-step N] [--host-decompress-kernel] [--guest-decompress-initrd] [--step-limit N] [--timer-poll-interval N] [--trace-mode off|arch|full] [--progress] [--timeout-secs N] [--stall-timeout-secs N] [--json]"
    );
}
