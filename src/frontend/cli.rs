use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use r86::audio_output::AudioOutput;
use r86::devices::Ps2ButtonState;

#[path = "config.rs"]
mod config;
#[path = "disk_files.rs"]
mod disk_files;
#[path = "keys.rs"]
mod keys;
#[path = "media.rs"]
mod media;
#[path = "protocol.rs"]
mod protocol;
use protocol::*;
#[cfg(feature = "sdl2-frontend")]
#[path = "sdl2_frontend.rs"]
mod sdl2_frontend;
#[cfg(unix)]
#[path = "terminal.rs"]
mod terminal;
#[cfg(not(feature = "sdl2-frontend"))]
mod sdl2_frontend {
    use super::{Machine, Sdl2Options};

    pub fn run(
        _machine: &mut Machine,
        _options: Sdl2Options,
        _network: &mut Option<r86::network::UserNetwork>,
        _audio: &mut r86::audio_output::AudioOutput,
    ) -> Result<(), String> {
        Err("SDL2 frontend was not compiled in; rebuild with --features sdl2-frontend".to_string())
    }
}
#[path = "ws.rs"]
mod ws;
#[path = "ws_frontend.rs"]
mod ws_frontend;
use ws_frontend::serve_ws;

use r86::FirmwareDisplay;
use r86::disk::Disk;
use r86::initrd_overlay::{InitrdFile, append_newc_overlay, entries_from_host_tree};
use r86::iso9660::{Iso9660, replace_file, replace_file_in_place};
use r86::linux_boot::BootConfig;
use r86::machine::{BootOrder, DEFAULT_JIT, GuestStepStatus, Machine, PerfMode, TraceMode};
use r86::network::{PortForward, UserNetwork};
use r86::x86::cpu::SegmentReg;
use r86::x86::{Cpu, Reg32};

const DEFAULT_MEM_MIB: usize = 128;
const DEFAULT_STEPS: u64 = 0;
const DEFAULT_BIOS: &str = "bios.bin";
const DEFAULT_VGA_BIOS: &str = "vgabios-stdvga.bin";
const DEFAULT_WS_SLICE_STEPS: u64 = 250_000;
const DEFAULT_SDL2_SLICE_STEPS: u64 = 250_000;
const DEFAULT_SDL2_SCALE: u32 = 2;
fn main() -> ExitCode {
    exit_result(run(env::args().skip(1)))
}

fn exit_result(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("r86-run: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let args = Args::parse(arguments)?;
    if let Some(path) = &args.create_vhd {
        let size = args.size_mib.unwrap() * 1024 * 1024;
        Disk::create_vhd(path, size)
            .map_err(|error| format!("failed to create VHD {path}: {error}"))?;
        println!("created dynamic VHD: {path} ({} MiB)", size / (1024 * 1024));
        return Ok(());
    }
    let mut iso_bytes = match &args.iso {
        Some(path) => fs::read(path).map_err(|err| format!("failed to read {path}: {err}"))?,
        None => Vec::new(),
    };
    let mut machine = if matches!(args.boot, BootMode::Firmware) {
        let display = args.effective_firmware_display();
        if args.iso.is_some() {
            prepare_firmware_iso(
                &mut iso_bytes,
                &args.cmdline,
                &args.initrd_files,
                args.dump_initrd.as_deref(),
                display,
            )?;
        }
        if let Some(path) = args.dump_prepared_iso.as_deref() {
            fs::write(path, &iso_bytes)
                .map_err(|err| format!("failed to dump prepared firmware ISO to {path}: {err}"))?;
        }
        let mut machine = Machine::new_firmware(args.mem_mib * 1024 * 1024, display)?;
        machine.set_boot_order(args.boot_order.unwrap_or(BootOrder::DiskFirst));
        let bios = fs::read(&args.bios)
            .map_err(|err| format!("failed to read BIOS {}: {err}", args.bios.display()))?;
        machine.load_bios_rom(&bios)?;
        if let Some(vga_bios) = &args.vga_bios {
            let rom = fs::read(vga_bios)
                .map_err(|err| format!("failed to read VGA BIOS {}: {err}", vga_bios.display()))?;
            machine.load_option_rom(0x000c_0000, &rom)?;
        }
        machine
    } else if let BootMode::Sector(path) = &args.boot {
        let mut machine =
            Machine::new_firmware(args.mem_mib * 1024 * 1024, FirmwareDisplay::Serial)?;
        let bootsector = fs::read(path)
            .map_err(|err| format!("failed to read bootsector {}: {err}", path.display()))?;
        machine.load_bootsector(&bootsector)?;
        machine
    } else if args.term {
        // The terminal needs the VGA aperture even when Linux is loaded
        // directly. The small serial-only machine treats that range as RAM.
        Machine::new_firmware(args.mem_mib * 1024 * 1024, FirmwareDisplay::Vga)?
    } else {
        Machine::new(args.mem_mib * 1024 * 1024)
    };
    if !args.no_cdrom {
        // An empty image represents an attached ATAPI drive with no medium.
        if args.hdd.is_some() {
            machine.attach_secondary_cdrom(iso_bytes);
        } else {
            machine.attach_primary_cdrom(iso_bytes);
        }
    }
    if let Some(path) = &args.hdd {
        let disk = Disk::open(path, args.hdd_writable)
            .map_err(|error| format!("failed to open HDD {path}: {error}"))?;
        machine.attach_disk(disk);
    }
    let result = run_machine(args, &mut machine);
    let flushed = machine.flush_disks();
    match (result, flushed) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(format!("failed to flush HDD: {error}")),
        (Err(error), Err(flush)) => Err(format!("{error}; failed to flush HDD: {flush}")),
    }
}

fn run_machine(args: Args, machine: &mut Machine) -> Result<(), String> {
    let mut audio = AudioOutput::new(args.audio_wav.as_deref(), args.sdl2)?;
    audio.set_volume(machine, 100);
    let result = run_machine_output(args, machine, &mut audio);
    let finished = audio.finish(machine);
    result.and(finished)
}

fn run_machine_output(
    args: Args,
    machine: &mut Machine,
    audio: &mut AudioOutput,
) -> Result<(), String> {
    let mut delayed_serial_input =
        if args.serial_input_after.is_some() && !args.serial_input.is_empty() {
            Some(args.serial_input.clone())
        } else {
            if !args.serial_input.is_empty() {
                machine.queue_serial_input(&args.serial_input);
            }
            None
        };

    if matches!(args.boot, BootMode::Firmware) {
        machine.reset_bios();
        println!(
            "loaded firmware PC: bios={} vga_bios={} cdrom={} hdd={} mem={} MiB reset={:#06x}:{:#06x}",
            args.bios.display(),
            args.vga_bios
                .as_deref()
                .map_or_else(|| "<none>".into(), |path| path.display().to_string()),
            args.iso
                .as_deref()
                .unwrap_or(if args.no_cdrom { "<none>" } else { "<empty>" }),
            args.hdd.as_deref().unwrap_or("<none>"),
            args.mem_mib,
            machine.cpu().seg(SegmentReg::Cs).selector,
            machine.cpu().eip
        );
    } else if let BootMode::Sector(path) = &args.boot {
        machine.enter_bootsector();
        println!(
            "loaded bootsector: image={} mem={} MiB entry={:#06x}:{:#06x}",
            path.display(),
            args.mem_mib,
            machine.cpu().seg(SegmentReg::Cs).selector,
            machine.cpu().eip
        );
    } else if let BootMode::Linux { kernel, initrd } = &args.boot {
        let kernel = fs::read(kernel)
            .map_err(|err| format!("failed to read Linux kernel {}: {err}", kernel.display()))?;
        let initrd = fs::read(initrd)
            .map_err(|err| format!("failed to read Linux initrd {}: {err}", initrd.display()))?;
        let mem_size = machine.ram().len() as u32;
        let boot = machine.load_linux(
            &kernel,
            &initrd,
            &BootConfig {
                mem_size,
                cmdline: &args.cmdline,
                host_decompress_kernel: args.host_decompress_kernel,
                host_decompress_initrd: args.host_decompress_initrd,
                overlay_files: &args.initrd_files,
                dump_initrd_path: args.dump_initrd.as_deref(),
            },
        )?;

        println!(
            "loaded Linux: kernel_payload={} bytes initrd={} bytes initrd_addr={:#010x} entry={:#010x} boot_params={:#010x}",
            boot.kernel_payload_len,
            boot.initrd_len,
            boot.initrd_load_addr,
            boot.entry,
            boot.boot_params
        );
    }

    let trace_mode = if args.debug_ring {
        TraceMode::Full
    } else if args.minimal_ring {
        TraceMode::Minimal
    } else {
        TraceMode::Off
    };
    let perf_mode = if args.perf_counters {
        PerfMode::Enabled
    } else {
        PerfMode::Disabled
    };
    machine.set_diagnostics(trace_mode, perf_mode);
    machine.set_jit_enabled(args.jit)?;
    if args.realtime_clock {
        machine.use_realtime_clock()?;
    }
    if args.usb_tablet {
        machine.attach_usb_tablet();
    }
    let mut network = if args.net {
        let net = UserNetwork::new(&args.net_forward).map_err(|e| format!("network: {e}"))?;
        machine.attach_network();
        Some(net)
    } else {
        None
    };
    if let Some(addr) = args.ws_listen.as_deref() {
        return serve_ws(
            addr,
            args.ws_slice_steps,
            args.ws_record.as_deref(),
            machine,
            &mut network,
            audio,
        );
    }
    if args.sdl2 {
        return sdl2_frontend::run(
            machine,
            Sdl2Options {
                slice_steps: args.sdl2_slice_steps,
                scale: args.sdl2_scale,
                grab_mouse: args.sdl2_grab_mouse,
                do_not_exit_when_poweroff: args.do_not_exit_when_poweroff,
                step_limit: if args.steps == 0 {
                    None
                } else {
                    Some(args.steps)
                },
            },
            &mut network,
            audio,
        );
    }
    if args.term {
        #[cfg(unix)]
        return terminal::run(
            machine,
            args.steps,
            args.do_not_exit_when_poweroff,
            &mut network,
            audio,
        );
        #[cfg(not(unix))]
        return Err("--term requires a Unix terminal".to_string());
    }

    let mut replay = ReplaySchedule::load(args.replay.as_deref())?;
    let mut completed_budget = true;
    let mut matched_serial_expectation = args.expect_serial.is_none();
    let mut serial_observed_len = 0usize;
    let mut input_after_matcher = args.serial_input_after.as_deref().map(ByteMatcher::new);
    let mut expect_matcher = args.expect_serial.as_deref().map(ByteMatcher::new);
    let can_bulk_run = !audio.enabled()
        && network.is_none()
        && !args.realtime_clock
        && replay.is_empty()
        && input_after_matcher.is_none()
        && expect_matcher.is_none();
    if args.steps != 0 && can_bulk_run {
        let report = machine.run_steps(args.steps)?;
        if let Some(status) = report.stop {
            print_machine_stop(machine, &status);
            completed_budget = false;
        }
    } else {
        let deadline = machine.step_count().saturating_add(args.steps);
        while machine.step_count() < deadline {
            replay.apply_due(machine)?;
            if let Some(net) = &mut network {
                net.poll(machine).map_err(|e| format!("network: {e}"))?;
            }
            let budget = if network.is_some() || audio.enabled() {
                (deadline - machine.step_count()).min(250_000)
            } else {
                deadline - machine.step_count()
            };
            let status = if replay.is_empty() {
                machine
                    .run_until_serial_output(budget)?
                    .stop
                    .unwrap_or(GuestStepStatus::Continue)
            } else {
                machine.step_once()?
            };
            audio.pump(machine)?;
            if !matches!(status, GuestStepStatus::Continue) {
                if matches!(status, GuestStepStatus::Halted) {
                    if machine.uses_realtime_clock() {
                        let wait = machine.halted_wait_duration();
                        std::thread::sleep(
                            network.as_ref().map_or(wait, |net| net.limit_wait(wait)),
                        );
                        continue;
                    }
                    // An external input can wake a halted CPU before the next
                    // device deadline. Never skip over its recorded timestamp.
                    let next_input = replay
                        .events
                        .get(replay.next)
                        .map_or(deadline, |event| event.step.min(deadline));
                    let remaining = next_input.saturating_sub(machine.step_count());
                    if machine.advance_halted(remaining) {
                        continue;
                    }
                }
                print_machine_stop(machine, &status);
                completed_budget = false;
                break;
            }
            let (input_after_matched, expect_matched) = {
                let new_serial = &machine.serial_output()[serial_observed_len..];
                serial_observed_len = machine.serial_output().len();
                let input_after_matched = input_after_matcher.as_mut().is_some_and(|matcher| {
                    delayed_serial_input.is_some() && matcher.observe(new_serial)
                });
                let expect_matched = expect_matcher
                    .as_mut()
                    .is_some_and(|matcher| matcher.observe(new_serial));
                (input_after_matched, expect_matched)
            };
            if input_after_matched {
                if let Some(bytes) = delayed_serial_input.take() {
                    machine.queue_serial_input(&bytes);
                }
            }
            if expect_matched {
                println!(
                    "serial expectation matched after {} steps",
                    machine.step_count()
                );
                matched_serial_expectation = true;
                completed_budget = false;
                break;
            }
        }
    }
    if completed_budget && args.steps != 0 {
        println!(
            "completed {} steps: {}",
            args.steps,
            cpu_summary(machine.cpu())
        );
        let eip = machine.cpu().eip;
        print_guest_bytes(machine, eip);
        print_guest_stack_if_requested(machine);
        print_vga_text(machine);
        print!("{}", machine.trace_report());
    }
    replay.ensure_finished()?;
    if args.print_frame_hash || args.expect_frame_hash.is_some() {
        let frame = capture_frame(machine)?;
        println!(
            "frame hash at step {}: {} {}x{}",
            machine.step_count(),
            frame.hash,
            frame.width,
            frame.height
        );
        if let Some(expected) = args.expect_frame_hash.as_deref() {
            if !frame.hash.eq_ignore_ascii_case(expected) {
                return Err(format!(
                    "frame hash mismatch: expected {expected}, got {}",
                    frame.hash
                ));
            }
        }
    }

    print!("{}", machine.jit_report());
    if !machine.serial_output().is_empty() {
        print!("{}", String::from_utf8_lossy(&machine.serial_output()));
    }
    if !matched_serial_expectation {
        let marker = String::from_utf8_lossy(args.expect_serial.as_deref().unwrap_or(&[]));
        return Err(format!("serial expectation was not observed: {marker}"));
    }
    if let Some(report) = machine.perf_report() {
        println!("{report}");
    }
    if machine.powered_off() && args.do_not_exit_when_poweroff {
        // S5 already flushed the disks. Without an interactive frontend there
        // is no reset command to service; remain idle until the host stops us.
        loop {
            std::thread::park();
        }
    }
    Ok(())
}

fn prepare_firmware_iso(
    iso_bytes: &mut Vec<u8>,
    cmdline: &str,
    overlay_files: &[InitrdFile],
    dump_initrd_path: Option<&str>,
    display: FirmwareDisplay,
) -> Result<(), String> {
    let iso = match Iso9660::open(iso_bytes) {
        Ok(iso) => iso,
        Err(err) => {
            if overlay_files.is_empty() && dump_initrd_path.is_none() {
                return Ok(());
            }
            return Err(format!(
                "firmware ISO preparation needs ISO9660 metadata: {err}"
            ));
        }
    };
    if iso.file_info("boot/isolinux/isolinux.cfg").is_err() {
        if overlay_files.is_empty() && dump_initrd_path.is_none() {
            return Ok(());
        }
        return Err(
            "firmware ISO overlays are only supported for TinyCore-style ISO images".to_string(),
        );
    }
    if !overlay_files.is_empty() || dump_initrd_path.is_some() {
        let initrd_gz = iso.read_file("boot/core.gz")?;
        let mut initrd = decompress_gzip_stream(&initrd_gz, "firmware initrd")?;
        append_newc_overlay(&mut initrd, overlay_files);
        if let Some(path) = dump_initrd_path {
            fs::write(path, &initrd).map_err(|err| {
                format!("failed to dump prepared firmware initrd to {path}: {err}")
            })?;
        }
        let patched = compress_gzip_stream(&initrd)?;
        replace_file(iso_bytes, "boot/core.gz", &patched)
            .map_err(|err| format!("failed to patch firmware ISO initrd: {err}"))?;
    }
    let cfg = firmware_isolinux_cfg_with_display(cmdline, display);
    replace_file_in_place(iso_bytes, "boot/isolinux/isolinux.cfg", cfg.as_bytes())
        .map_err(|err| format!("failed to patch firmware ISO boot config: {err}"))
}

#[cfg_attr(not(test), allow(dead_code))]
fn firmware_isolinux_cfg(cmdline: &str) -> String {
    firmware_isolinux_cfg_with_display(cmdline, FirmwareDisplay::Serial)
}

fn firmware_isolinux_cfg_with_display(cmdline: &str, display: FirmwareDisplay) -> String {
    let mut cfg = String::new();
    if display == FirmwareDisplay::Serial {
        cfg.push_str("SERIAL 0 115200\n");
    }
    cfg.push_str(
        "DEFAULT r86\n\
         PROMPT 0\n\
         TIMEOUT 1\n\
         LABEL r86\n\
         KERNEL /boot/vmlinuz\n\
         INITRD /boot/core.gz\n\
         APPEND",
    );
    if !cmdline.trim().is_empty() {
        cfg.push(' ');
        cfg.push_str(cmdline.trim());
    }
    cfg.push('\n');
    cfg
}

fn decompress_gzip_stream(bytes: &[u8], what: &str) -> Result<Vec<u8>, String> {
    let mut decoder = GzDecoder::new(bytes);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|err| format!("failed to decompress {what}: {err}"))?;
    Ok(out)
}

fn compress_gzip_stream(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder
        .write_all(bytes)
        .map_err(|err| format!("failed to compress firmware initrd: {err}"))?;
    encoder
        .finish()
        .map_err(|err| format!("failed to finish firmware initrd compression: {err}"))
}

fn cpu_summary(cpu: &Cpu) -> String {
    let cs = cpu.seg(SegmentReg::Cs);
    format!(
        "eip={:#010x} cs={:#06x} cs_base={:#010x} cs_limit={:#010x} cs_flags={:#06x} gdtr={:#010x}:{:#06x} eax={:#010x} ebx={:#010x} ecx={:#010x} edx={:#010x} esi={:#010x} edi={:#010x} ebp={:#010x} esp={:#010x} eflags={:#010x}",
        cpu.eip,
        cs.selector,
        cs.base,
        cs.limit,
        cs.architectural_flags(),
        cpu.gdtr.base,
        cpu.gdtr.limit,
        cpu.reg(Reg32::Eax),
        cpu.reg(Reg32::Ebx),
        cpu.reg(Reg32::Ecx),
        cpu.reg(Reg32::Edx),
        cpu.reg(Reg32::Esi),
        cpu.reg(Reg32::Edi),
        cpu.reg(Reg32::Ebp),
        cpu.reg(Reg32::Esp),
        cpu.effective_eflags()
    )
}

fn print_machine_stop(machine: &mut Machine, status: &GuestStepStatus) {
    match status {
        GuestStepStatus::Continue => {}
        GuestStepStatus::Halted => {
            println!(
                "halted after {} steps: {}",
                machine.step_count(),
                cpu_summary(machine.cpu())
            );
            print_guest_bytes(machine, machine.cpu().eip);
        }
        GuestStepStatus::Reset => {
            println!(
                "guest requested reset after {} steps: {}",
                machine.step_count(),
                cpu_summary(machine.cpu())
            );
            print_guest_bytes(machine, machine.cpu().eip);
        }
        GuestStepStatus::PoweredOff => {
            println!("guest powered off after {} steps", machine.step_count());
        }
        GuestStepStatus::Stopped { message, eip } => {
            println!(
                "stopped after {} steps: {} {}",
                machine.step_count(),
                message,
                cpu_summary(machine.cpu())
            );
            print_guest_bytes(machine, eip.unwrap_or(machine.cpu().eip));
        }
    }
    if !matches!(status, GuestStepStatus::Continue) {
        print_vga_text(&machine);
        print!("{}", machine.trace_report());
    }
}

fn print_guest_bytes(machine: &mut Machine, eip: u32) {
    println!("{}", guest_bytes_dump(machine, eip));
}

fn guest_bytes_dump(machine: &mut Machine, eip: u32) -> String {
    use std::fmt::Write as _;

    let start = eip.saturating_sub(24);
    let cs = machine.cpu().seg(SegmentReg::Cs);
    let linear = cs.base.wrapping_add(start);
    let mut dump = format!(
        "bytes near {eip:#010x} from {start:#010x} \
         (cs={:#06x} base={:#010x} linear={linear:#010x}):",
        cs.selector, cs.base
    );
    let mut bytes = [None; 64];
    machine.guest_code_bytes(linear, &mut bytes, false);
    for byte in bytes {
        match byte {
            Some(byte) => {
                let _ = write!(dump, " {byte:02x}");
            }
            None => dump.push_str(" ??"),
        }
    }
    dump
}

fn print_guest_stack_if_requested(machine: &mut Machine) {
    if std::env::var_os("R86_PRINT_STACK").is_none() {
        return;
    }
    let esp = machine.cpu().reg(Reg32::Esp);
    print!("stack at esp={esp:#010x}:");
    let mut words = [None; 8];
    machine.guest_stack_words(&mut words);
    for (index, word) in words.iter().enumerate() {
        let addr = esp.wrapping_add(index as u32 * 4);
        match word {
            Some(value) => print!(" [{addr:#010x}]={value:#010x}"),
            None => print!(" [{addr:#010x}]=????????"),
        }
    }
    println!();
    if let Some(return_eip) = words[0] {
        print_guest_bytes(machine, return_eip);
    }
}

fn print_vga_text(machine: &Machine) {
    let base = 0xb8000;
    let ram_vga = machine.ram().get(base..base + 80 * 25 * 2);
    if !machine.firmware_mode() && ram_vga.is_none() {
        return;
    }
    let mut any = false;
    println!("vga text:");
    for row in 0..25 {
        let mut line = String::new();
        for col in 0..80 {
            let offset = (row * 80 + col) * 2;
            let ch = if machine.firmware_mode() {
                machine.vga().text_byte(offset).unwrap_or(0)
            } else {
                ram_vga
                    .and_then(|vga| vga.get(offset).copied())
                    .unwrap_or(0)
            };
            let ch = if (0x20..=0x7e).contains(&ch) {
                ch as char
            } else {
                ' '
            };
            if ch != ' ' {
                any = true;
            }
            line.push(ch);
        }
        if !line.trim_end().is_empty() {
            println!("  {}", line.trim_end());
        }
    }
    if !any {
        println!("  <empty>");
    }
}

struct ReplaySchedule {
    events: Vec<ReplayEvent>,
    next: usize,
    mouse_buttons: Ps2ButtonState,
}

struct ReplayEvent {
    order: usize,
    step: u64,
    line: String,
}

impl ReplaySchedule {
    fn load(path: Option<&str>) -> Result<Self, String> {
        let Some(path) = path else {
            return Ok(Self {
                events: Vec::new(),
                next: 0,
                mouse_buttons: Ps2ButtonState::default(),
            });
        };
        let text = fs::read_to_string(path)
            .map_err(|err| format!("failed to read replay file {path}: {err}"))?;
        let mut events = Vec::new();
        for (order, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let step = json_u64(line, "step")?
                .ok_or_else(|| format!("replay line {} missing step", order + 1))?;
            events.push(ReplayEvent {
                order,
                step,
                line: line.to_string(),
            });
        }
        events.sort_by(|a, b| a.step.cmp(&b.step).then(a.order.cmp(&b.order)));
        Ok(Self {
            events,
            next: 0,
            mouse_buttons: Ps2ButtonState::default(),
        })
    }

    fn apply_due(&mut self, machine: &mut Machine) -> Result<(), String> {
        while self
            .events
            .get(self.next)
            .is_some_and(|event| event.step <= machine.step_count())
        {
            let line = self.events[self.next].line.clone();
            self.apply_event(machine, &line)?;
            self.next += 1;
        }
        Ok(())
    }

    fn ensure_finished(&self) -> Result<(), String> {
        if let Some(event) = self.events.get(self.next) {
            Err(format!(
                "replay ended before event at step {}: {}",
                event.step, event.line
            ))
        } else {
            Ok(())
        }
    }

    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    fn apply_event(&mut self, machine: &mut Machine, line: &str) -> Result<(), String> {
        let cmd =
            json_string(line, "cmd")?.ok_or_else(|| "replay event missing cmd".to_string())?;
        match cmd.as_str() {
            "power-button" => machine.press_power_button(),
            "reset" => machine.reboot()?,
            "media-eject" => machine.change_cdrom(Vec::new())?,
            "power-off" => machine.power_off()?,
            "power-on" => machine.reboot()?,
            "type" => {
                let text = json_string(line, "text")?
                    .ok_or_else(|| "type event missing text".to_string())?;
                machine.inject_ascii(text.as_bytes());
            }
            "keydown" | "keyup" => {
                let key_name =
                    json_string(line, "key")?.ok_or_else(|| format!("{cmd} event missing key"))?;
                keys::inject_named_key(machine, &key_name, cmd == "keydown")?;
            }
            "send-scancodes" => {
                let hex = json_string(line, "hex")?
                    .ok_or_else(|| "send-scancodes event missing hex".to_string())?;
                let bytes = parse_hex_bytes(&hex)?;
                machine.inject_scancode_set2(&bytes);
            }
            "usb-tablet" => {
                let (x, y, buttons, wheel) = parse_usb_tablet(line)?;
                machine.inject_usb_tablet(x, y, buttons, wheel)?;
            }
            "mouse-move" => {
                let dx = clamp_i64_to_i16(json_i64(line, "dx")?.unwrap_or(0));
                let dy = clamp_i64_to_i16(json_i64(line, "dy")?.unwrap_or(0));
                machine.inject_mouse(dx, dy, self.mouse_buttons);
            }
            "mouse-button" => {
                let button = json_string(line, "button")?
                    .ok_or_else(|| "mouse-button event missing button".to_string())?;
                let down = json_bool(line, "down")?.unwrap_or(false);
                set_mouse_button(&mut self.mouse_buttons, &button, down)?;
                machine.inject_mouse(0, 0, self.mouse_buttons);
            }
            other => return Err(format!("unsupported replay event command {other}")),
        }
        Ok(())
    }
}

enum BootMode {
    Firmware,
    Sector(PathBuf),
    Linux { kernel: PathBuf, initrd: PathBuf },
}

struct Args {
    jit: bool,
    realtime_clock: bool,
    iso: Option<String>,
    no_cdrom: bool,
    boot_order: Option<BootOrder>,
    boot: BootMode,
    bios: PathBuf,
    vga_bios: Option<PathBuf>,
    hdd: Option<String>,
    hdd_writable: bool,
    net: bool,
    usb_tablet: bool,
    net_forward: Vec<PortForward>,
    audio_wav: Option<String>,
    do_not_exit_when_poweroff: bool,
    create_vhd: Option<String>,
    size_mib: Option<u64>,
    firmware_display: Option<FirmwareDisplay>,
    mem_mib: usize,
    steps: u64,
    cmdline: String,
    host_decompress_kernel: bool,
    host_decompress_initrd: bool,
    serial_input: Vec<u8>,
    serial_input_after: Option<Vec<u8>>,
    expect_serial: Option<Vec<u8>>,
    initrd_files: Vec<InitrdFile>,
    dump_initrd: Option<String>,
    dump_prepared_iso: Option<String>,
    ws_listen: Option<String>,
    ws_slice_steps: u64,
    ws_record: Option<String>,
    sdl2: bool,
    term: bool,
    sdl2_slice_steps: u64,
    sdl2_scale: u32,
    sdl2_grab_mouse: bool,
    replay: Option<String>,
    print_frame_hash: bool,
    expect_frame_hash: Option<String>,
    minimal_ring: bool,
    debug_ring: bool,
    perf_counters: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Sdl2Options {
    slice_steps: u64,
    scale: u32,
    grab_mouse: bool,
    do_not_exit_when_poweroff: bool,
    step_limit: Option<u64>,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let args = config::arguments(args)?;
        let executable = env::current_exe()
            .map_err(|err| format!("failed to locate executable for BIOS paths: {err}"))?;
        let mut iso = None;
        let mut no_cdrom = false;
        let mut boot_order = None;
        let mut jit = DEFAULT_JIT;
        let mut realtime_clock = true;
        let mut bootsector = None;
        let mut boot_linux_kernel = None;
        let mut boot_linux_initrd = None;
        let mut bios = executable.with_file_name(DEFAULT_BIOS);
        let mut vga_bios = Some(executable.with_file_name(DEFAULT_VGA_BIOS));
        let mut hdd = None;
        let mut hdd_writable = false;
        let mut net = false;
        let mut usb_tablet = false;
        let mut net_forward = Vec::new();
        let mut audio_wav = None;
        let mut do_not_exit_when_poweroff = false;
        let mut create_vhd = None;
        let mut size_mib = None;
        let mut firmware_display = None;
        let mut mem_mib = DEFAULT_MEM_MIB;
        let mut steps = DEFAULT_STEPS;
        let mut cmdline = String::from(
            "console=ttyS0,115200n8 earlyprintk=serial,ttyS0,115200 loglevel=7 nozswap",
        );
        let mut cmdline_explicit = false;
        let mut host_decompress_kernel = false;
        let mut host_decompress_initrd = true;
        let mut serial_input = Vec::new();
        let mut serial_input_after = None;
        let mut expect_serial = None;
        let mut initrd_files = Vec::new();
        let mut dump_initrd = None;
        let mut dump_prepared_iso = None;
        let mut bootlocal_command = String::new();
        let mut ws_listen = None;
        let mut ws_slice_steps = DEFAULT_WS_SLICE_STEPS;
        let mut ws_record = None;
        let mut sdl2 = false;
        let mut term = false;
        let mut sdl2_slice_steps = DEFAULT_SDL2_SLICE_STEPS;
        let mut sdl2_scale = DEFAULT_SDL2_SCALE;
        let mut sdl2_grab_mouse = false;
        let mut replay = None;
        let mut print_frame_hash = false;
        let mut expect_frame_hash = None;
        let mut minimal_ring = false;
        let mut debug_ring = false;
        let mut perf_counters = false;

        let mut args = args.into_iter().peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--iso" => iso = Some(take_value(&mut args, "--iso")?),
                "--no-cdrom" => no_cdrom = true,
                "--boot-order" => {
                    boot_order = Some(match take_value(&mut args, "--boot-order")?.as_str() {
                        "disk" => BootOrder::DiskFirst,
                        "cdrom" => BootOrder::CdromFirst,
                        _ => return Err("--boot-order expects disk or cdrom".into()),
                    });
                }
                "--clock" => {
                    realtime_clock = match take_value(&mut args, "--clock")?.as_str() {
                        "realtime" => true,
                        "deterministic" => false,
                        _ => return Err("--clock expects realtime or deterministic".into()),
                    };
                }
                "--cpu-engine" => {
                    jit = match take_value(&mut args, "--cpu-engine")?.as_str() {
                        "interp" => false,
                        "jit" => true,
                        _ => return Err("--cpu-engine expects interp or jit".into()),
                    };
                }
                "--bootsector" => bootsector = Some(take_value(&mut args, "--bootsector")?.into()),
                "--boot-linux-kernel" => {
                    boot_linux_kernel = Some(take_value(&mut args, "--boot-linux-kernel")?.into());
                }
                "--boot-linux-initrd" => {
                    boot_linux_initrd = Some(take_value(&mut args, "--boot-linux-initrd")?.into());
                }
                "--bios" => bios = take_value(&mut args, "--bios")?.into(),
                "--vga-bios" => vga_bios = Some(take_value(&mut args, "--vga-bios")?.into()),
                "--no-vga-bios" => vga_bios = None,
                "--hdd" => hdd = Some(take_value(&mut args, "--hdd")?),
                "--hdd-writable" => hdd_writable = true,
                "--usb-tablet" => usb_tablet = true,
                "--audio-wav" => audio_wav = Some(take_value(&mut args, "--audio-wav")?),
                "--net" => {
                    net = match take_value(&mut args, "--net")?.as_str() {
                        "user" => true,
                        "none" => false,
                        _ => return Err("--net expects user or none".into()),
                    }
                }
                "--net-forward" => {
                    net_forward.push(take_value(&mut args, "--net-forward")?.parse()?)
                }
                "--do-not-exit-when-poweroff" => do_not_exit_when_poweroff = true,
                "--create-vhd" => create_vhd = Some(take_value(&mut args, "--create-vhd")?),
                "--size-mib" => {
                    size_mib = Some(
                        take_value(&mut args, "--size-mib")?
                            .parse::<u64>()
                            .map_err(|_| "--size-mib expects an integer".to_string())?,
                    );
                }
                "--firmware-display" => {
                    let value = take_value(&mut args, "--firmware-display")?;
                    firmware_display = Some(match value.as_str() {
                        "serial" => FirmwareDisplay::Serial,
                        "vga" => FirmwareDisplay::Vga,
                        _ => {
                            return Err(
                                "--firmware-display expects either 'serial' or 'vga'".to_string()
                            );
                        }
                    });
                }
                "--mem-mib" => {
                    mem_mib = take_value(&mut args, "--mem-mib")?
                        .parse()
                        .map_err(|_| "--mem-mib expects an integer".to_string())?;
                }
                "--steps" => {
                    steps = take_value(&mut args, "--steps")?
                        .parse()
                        .map_err(|_| "--steps expects an integer".to_string())?;
                }
                "--cmdline" => {
                    cmdline = take_value(&mut args, "--cmdline")?;
                    cmdline_explicit = true;
                }
                "--host-decompress-kernel" => host_decompress_kernel = true,
                "--guest-decompress-initrd" => host_decompress_initrd = false,
                "--serial-input" => {
                    let bytes = take_nonempty_bytes(&mut args, "--serial-input")?;
                    serial_input.extend_from_slice(&bytes);
                }
                "--serial-input-file" => {
                    let path = take_value(&mut args, "--serial-input-file")?;
                    let bytes = fs::read(&path)
                        .map_err(|err| format!("failed to read serial input file {path}: {err}"))?;
                    serial_input.extend_from_slice(&bytes);
                }
                "--serial-input-after" => {
                    serial_input_after =
                        Some(take_nonempty_bytes(&mut args, "--serial-input-after")?);
                }
                "--serial-command" => {
                    let command = take_value(&mut args, "--serial-command")?;
                    serial_input.extend_from_slice(command.as_bytes());
                    if !command.ends_with('\n') {
                        serial_input.push(b'\n');
                    }
                    if serial_input_after.is_none() {
                        serial_input_after = Some(b"tc@box:~$".to_vec());
                    }
                }
                "--expect-serial" => {
                    expect_serial = Some(take_nonempty_bytes(&mut args, "--expect-serial")?);
                }
                "--bootlocal-command" => {
                    let command = take_value(&mut args, "--bootlocal-command")?;
                    bootlocal_command.push_str(&command);
                    if !bootlocal_command.ends_with('\n') {
                        bootlocal_command.push('\n');
                    }
                }
                "--initrd-file" => {
                    let spec = take_value(&mut args, "--initrd-file")?;
                    initrd_files.push(InitrdFile::from_host_spec(&spec)?);
                }
                "--initrd-tree" => {
                    let spec = take_value(&mut args, "--initrd-tree")?;
                    initrd_files.extend(entries_from_host_tree(&spec)?);
                }
                "--dump-initrd" => dump_initrd = Some(take_value(&mut args, "--dump-initrd")?),
                "--dump-prepared-iso" => {
                    dump_prepared_iso = Some(take_value(&mut args, "--dump-prepared-iso")?)
                }
                "--ws-listen" => ws_listen = Some(take_value(&mut args, "--ws-listen")?),
                "--ws-slice-steps" => {
                    ws_slice_steps = take_value(&mut args, "--ws-slice-steps")?
                        .parse()
                        .map_err(|_| "--ws-slice-steps expects an integer".to_string())?;
                    if ws_slice_steps == 0 {
                        return Err("--ws-slice-steps must be non-zero".to_string());
                    }
                }
                "--ws-record" => ws_record = Some(take_value(&mut args, "--ws-record")?),
                "--sdl2" => sdl2 = true,
                "--term" => term = true,
                "--sdl2-slice-steps" => {
                    sdl2_slice_steps = take_value(&mut args, "--sdl2-slice-steps")?
                        .parse()
                        .map_err(|_| "--sdl2-slice-steps expects an integer".to_string())?;
                    if sdl2_slice_steps == 0 {
                        return Err("--sdl2-slice-steps must be non-zero".to_string());
                    }
                }
                "--sdl2-scale" => {
                    sdl2_scale = take_value(&mut args, "--sdl2-scale")?
                        .parse()
                        .map_err(|_| "--sdl2-scale expects an integer".to_string())?;
                    if sdl2_scale == 0 {
                        return Err("--sdl2-scale must be non-zero".to_string());
                    }
                }
                "--sdl2-grab-mouse" => sdl2_grab_mouse = true,
                "--replay" => replay = Some(take_value(&mut args, "--replay")?),
                "--minimal-ring" => minimal_ring = true,
                "--debug-ring" => debug_ring = true,
                "--perf-counters" => perf_counters = true,
                "--print-frame-hash" => print_frame_hash = true,
                "--expect-frame-hash" => {
                    expect_frame_hash = Some(take_value(&mut args, "--expect-frame-hash")?)
                }
                "-h" | "--help" => return Err(usage()),
                other => return Err(format!("unknown argument {other}\n{}", usage())),
            }
        }
        let boot = match (boot_linux_kernel, boot_linux_initrd, bootsector) {
            (None, None, None) => BootMode::Firmware,
            (None, None, Some(path)) => BootMode::Sector(path),
            (Some(kernel), Some(initrd), None) => BootMode::Linux { kernel, initrd },
            (_, _, Some(_)) => {
                return Err("--bootsector cannot be combined with Linux boot images".into());
            }
            _ => {
                return Err(
                    "--boot-linux-kernel and --boot-linux-initrd must be specified together".into(),
                );
            }
        };
        let firmware_boot = matches!(boot, BootMode::Firmware);
        if create_vhd.is_some() != size_mib.is_some() {
            return Err("--create-vhd and --size-mib must be specified together".to_string());
        }
        if size_mib.is_some_and(|size| size == 0 || size > 131072) {
            return Err("--size-mib must be between 1 and 131072 (LBA28)".to_string());
        }
        if create_vhd.is_some()
            && (hdd.is_some()
                || iso.is_some()
                || !firmware_boot
                || term
                || sdl2
                || ws_listen.is_some())
        {
            return Err(
                "--create-vhd creates a disk and exits; run the guest separately".to_string(),
            );
        }
        if mem_mib == 0 || mem_mib.checked_mul(1024 * 1024).is_none() {
            return Err("--mem-mib must be positive and fit in host address space".to_string());
        }
        if dump_prepared_iso.is_some() && !firmware_boot {
            return Err("--dump-prepared-iso requires BIOS boot".to_string());
        }
        if boot_order.is_some() && !firmware_boot {
            return Err("--boot-order requires BIOS boot".into());
        }
        if dump_prepared_iso.is_some() && iso.is_none() {
            return Err("--dump-prepared-iso requires --iso".into());
        }
        if !matches!(boot, BootMode::Linux { .. })
            && (!firmware_boot || iso.is_none())
            && (dump_initrd.is_some() || !initrd_files.is_empty() || !bootlocal_command.is_empty())
        {
            return Err(
                "initrd overlays and dumps require Linux boot images or a BIOS boot ISO".into(),
            );
        }
        if replay.is_some() && realtime_clock {
            return Err("--replay requires --clock deterministic".into());
        }
        if ws_record.is_some() && ws_listen.is_none() {
            return Err("--ws-record requires --ws-listen".to_string());
        }
        if sdl2 && ws_listen.is_some() {
            return Err("--sdl2 cannot be combined with --ws-listen".to_string());
        }
        if term && (sdl2 || ws_listen.is_some()) {
            return Err("--term cannot be combined with --sdl2 or --ws-listen".to_string());
        }
        if (sdl2 || term || ws_listen.is_some()) && !cmdline_explicit {
            cmdline = String::from("loglevel=3 nozswap");
        }
        if !bootlocal_command.is_empty() {
            initrd_files.push(InitrdFile::new(
                "opt/bootlocal.sh",
                0o755,
                bootlocal_script(&bootlocal_command),
            )?);
            initrd_files.push(InitrdFile::new(
                "opt/bootsync.sh",
                0o755,
                bootsync_script(),
            )?);
        }
        if !net && !net_forward.is_empty() {
            return Err("--net-forward requires --net user".into());
        }
        if net && replay.is_some() {
            return Err("live networking cannot be combined with input replay".into());
        }
        Ok(Self {
            net,
            usb_tablet,
            net_forward,
            audio_wav,
            iso,
            no_cdrom,
            boot_order,
            jit,
            realtime_clock,
            boot,
            bios,
            vga_bios,
            hdd,
            hdd_writable,
            do_not_exit_when_poweroff,
            create_vhd,
            size_mib,
            firmware_display,
            mem_mib,
            steps,
            cmdline,
            host_decompress_kernel,
            host_decompress_initrd,
            serial_input,
            serial_input_after,
            expect_serial,
            initrd_files,
            dump_initrd,
            dump_prepared_iso,
            ws_listen,
            ws_slice_steps,
            ws_record,
            sdl2,
            term,
            sdl2_slice_steps,
            sdl2_scale,
            sdl2_grab_mouse,
            replay,
            print_frame_hash,
            expect_frame_hash,
            minimal_ring,
            debug_ring,
            perf_counters,
        })
    }

    fn effective_firmware_display(&self) -> FirmwareDisplay {
        self.firmware_display
            .unwrap_or(if self.sdl2 || self.term || self.ws_listen.is_some() {
                FirmwareDisplay::Vga
            } else {
                FirmwareDisplay::Serial
            })
    }
}

fn take_value(
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    name: &str,
) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{name} requires a value\n{}", usage()))
}

fn take_nonempty_bytes(
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    name: &str,
) -> Result<Vec<u8>, String> {
    let value = take_value(args, name)?;
    if value.is_empty() {
        Err(format!("{name} requires a non-empty value\n{}", usage()))
    } else {
        Ok(value.into_bytes())
    }
}

fn bootlocal_script(command: &str) -> Vec<u8> {
    let mut script = String::from("#!/bin/sh\n{\n");
    script.push_str(command);
    if !script.ends_with('\n') {
        script.push('\n');
    }
    script.push_str("} >/dev/ttyS0 2>&1\n");
    script.into_bytes()
}

fn bootsync_script() -> Vec<u8> {
    b"#!/bin/sh\n/usr/bin/sethostname box\n/opt/bootlocal.sh\n".to_vec()
}

fn usage() -> String {
    "usage: cargo run --bin r86-run -- [--config FILE.toml] [--cpu-engine interp|jit] [--clock realtime|deterministic] [--iso FILE|--no-cdrom] [--boot-linux-kernel FILE --boot-linux-initrd FILE] [--boot-order disk|cdrom] [--bootsector FILE] [--bios FILE] [--vga-bios FILE|--no-vga-bios] [--hdd IMAGE] [--hdd-writable] [--audio-wav PATH] [--usb-tablet] [--net none|user] [--net-forward tcp:HOSTPORT:GUESTPORT] [--do-not-exit-when-poweroff] [--firmware-display serial|vga] [--mem-mib 128] [--steps N] [--cmdline TEXT] [--host-decompress-kernel] [--guest-decompress-initrd] [--initrd-file HOST:GUEST[:MODE]] [--initrd-tree HOST[:GUEST]] [--bootlocal-command TEXT] [--serial-input TEXT] [--serial-input-file PATH] [--serial-input-after TEXT] [--serial-command TEXT] [--expect-serial TEXT] [--dump-initrd PATH] [--dump-prepared-iso PATH] [--ws-listen ADDR] [--ws-slice-steps N] [--ws-record PATH] [--term|--sdl2] [--sdl2-slice-steps N] [--sdl2-scale N] [--sdl2-grab-mouse] [--replay PATH] [--minimal-ring] [--debug-ring] [--perf-counters] [--print-frame-hash] [--expect-frame-hash HEX]\n       r86-run --create-vhd disk.vhd --size-mib 8192\n\nBoot: BIOS by default, with an empty CD-ROM drive and no hard disk.\nBIOS defaults: bios.bin and vgabios-stdvga.bin beside the executable.\nLinux boot: supply both --boot-linux-kernel and --boot-linux-initrd as external files.\n--iso inserts a CD-ROM image; --no-cdrom removes the drive.\nCPU: JIT by default on supported ARM64 Linux builds; interpreter on other hosts.\nClock: realtime by default; deterministic uses instruction ticks for tests/replay.\nHDD images: dynamic VHD or raw; --hdd-writable persists guest writes.\nUSB tablet: off by default. --usb-tablet adds PIIX3 UHCI and an absolute HID pointer.\nNetwork: off by default. --net user supplies IPv4 TCP, DHCP and DNS A; forwards bind 127.0.0.1.\nPower-off: flush disks and exit by default; --do-not-exit-when-poweroff keeps the process alive. --ws-listen always stays up.\nPrecedence: built-in defaults < TOML < CLI (regardless of --config position).\nValue options accept --name=value; flags accept --flag=true or --flag=false.\nTOML keys match long option names without --; relative paths use the working directory.\nRepeated CLI inputs replace the corresponding TOML list. See design/CONFIGURATION.md.".to_string()
}

#[cfg(test)]
mod tests {
    use super::keys::parse_ps2_key;
    use super::*;
    use r86::devices::Ps2Key;

    #[test]
    fn segmented_code_dump_and_ws_read_cs_base() {
        let mut machine = Machine::new_firmware(2 << 20, FirmwareDisplay::Serial).unwrap();
        let mut boot = [0xcc; 512];
        boot[..10].copy_from_slice(&[
            0xc6, 0x06, 0x20, 0x00, 0xaa, // mov byte [20h],aah: misleading raw-offset byte
            0xea, 0x38, 0x00, 0xc0, 0x07, // jmp 07c0:0038
        ]);
        let expected: Vec<u8> = (0x40..0x80).collect();
        boot[0x20..0x60].copy_from_slice(&expected);
        boot[510..].copy_from_slice(&[0x55, 0xaa]);
        machine.load_bootsector(&boot).unwrap();
        machine.enter_bootsector();
        machine.run_steps(2).unwrap();
        assert_eq!(machine.cpu().eip, 0x38);
        assert_eq!(machine.cpu().seg(SegmentReg::Cs).base, 0x7c00);
        assert_eq!(machine.ram()[0x20], 0xaa);

        let expected_bytes: String = expected.iter().map(|byte| format!(" {byte:02x}")).collect();
        assert_eq!(
            guest_bytes_dump(&mut machine, 0x38),
            format!(
                "bytes near 0x00000038 from 0x00000020 \
                 (cs=0x07c0 base=0x00007c00 linear=0x00007c20):{expected_bytes}"
            )
        );
        let json = guest_bytes_json(&mut machine, true);
        assert_eq!(json_u64(&json, "eip").unwrap(), Some(0x38));
        assert_eq!(json_u64(&json, "start").unwrap(), Some(0x20));
        assert_eq!(
            json_string(&json, "hex").unwrap(),
            Some(hex_encode(&expected))
        );
    }

    #[test]
    fn replayed_power_button_wakes_an_idle_guest_at_its_recorded_step() {
        let record = TestConfig::new("{\"cmd\":\"power-button\",\"step\":2000}\n");
        let args = Args::parse(
            [
                "--bootsector",
                "tests/bootsectors/acpi_button.bin",
                "--steps",
                "10000",
                "--clock",
                "deterministic",
                "--replay",
                record.0.to_str().unwrap(),
            ]
            .into_iter()
            .map(str::to_string),
        )
        .unwrap();
        let mut machine = Machine::new_firmware(2 << 20, FirmwareDisplay::Serial).unwrap();
        machine
            .load_bootsector(include_bytes!("../../tests/bootsectors/acpi_button.bin"))
            .unwrap();
        run_machine(args, &mut machine).unwrap();
        assert!(machine.powered_off());
        assert!(machine.step_count() > 2000 && machine.step_count() < 2100);
        assert_eq!(machine.serial_output(), b"P");
    }

    #[test]
    fn usb_tablet_cli_toml_and_replay_boundary() {
        let config = TestConfig::new("usb-tablet = true\n");
        assert!(config.parse(&[]).unwrap().usb_tablet);
        assert!(!config.parse(&["--usb-tablet=false"]).unwrap().usb_tablet);
        assert!(config.parse(&["--usb-tablet=true"]).unwrap().usb_tablet);
        assert_eq!(
            parse_usb_tablet(r#"{"x":32767,"y":0,"buttons":7,"wheel":-127}"#).unwrap(),
            (32767, 0, 7, -127)
        );
        for input in [
            r#"{"x":32768,"y":0}"#,
            r#"{"x":-1,"y":0}"#,
            r#"{"x":0}"#,
            r#"{"x":0,"y":0,"buttons":8}"#,
            r#"{"x":0,"y":0,"wheel":-128}"#,
            r#"{"x":0.5,"y":0}"#,
        ] {
            assert!(parse_usb_tablet(input).is_err(), "{input}");
        }
        let mut m = Machine::new(0x10000);
        let mut replay = ReplaySchedule::load(None).unwrap();
        let input = r#"{"cmd":"usb-tablet","x":123,"y":456,"buttons":1}"#;
        assert!(replay.apply_event(&mut m, input).is_err());
        m.attach_usb_tablet();
        replay.apply_event(&mut m, input).unwrap();
    }

    #[test]
    fn network_cli_and_toml_options() {
        let config =
            TestConfig::new("net = 'user'\nnet-forward = ['tcp:8080:80', 'tcp:2222:22']\n");
        let args = config.parse(&[]).unwrap();
        assert!(args.net);
        assert_eq!(
            args.net_forward,
            vec![
                PortForward {
                    host_port: 8080,
                    guest_port: 80
                },
                PortForward {
                    host_port: 2222,
                    guest_port: 22
                }
            ]
        );
        assert_eq!(
            config
                .parse(&["--net-forward=tcp:9000:80"])
                .unwrap()
                .net_forward,
            vec![PortForward {
                host_port: 9000,
                guest_port: 80
            }]
        );
        assert!(config.parse(&["--net=none"]).is_err());
        for value in ["udp:8080:80", "tcp:0:80", "tcp:65536:80", "tcp:1:2:3"] {
            assert!(config.parse(&[&format!("--net-forward={value}")]).is_err());
        }
        assert!(config.parse(&["--replay=unused"]).is_err());
    }

    #[test]
    fn disk_boot_cli_overrides_configured_cdrom_before_loading_media() {
        let config = TestConfig::new("iso = '/nonexistent/xp.iso'\nboot-order = 'cdrom'\n");
        let args = config.parse(&["--no-cdrom", "--boot-order=disk"]).unwrap();
        assert!(args.iso.is_none());
        assert!(args.no_cdrom);
        assert_eq!(args.boot_order, Some(BootOrder::DiskFirst));
        let config = TestConfig::new("no-cdrom = true\n");
        assert_eq!(
            config.parse(&["--iso=setup.iso"]).unwrap().iso.as_deref(),
            Some("setup.iso")
        );
        assert!(!config.parse(&["--iso=setup.iso"]).unwrap().no_cdrom);
        let empty = config.parse(&["--no-cdrom=false"]).unwrap();
        assert!(!empty.no_cdrom);
        assert!(empty.iso.is_none());
        let config = TestConfig::new("iso = 'setup.iso'\nno-cdrom = true\n");
        assert!(config.parse(&[]).is_err());
    }

    #[test]
    fn jit_engine_obeys_toml_and_cli_precedence() {
        let config = TestConfig::new("cpu-engine = 'jit'\n");
        let configured = config.parse(&[]).unwrap();
        assert!(configured.jit);
        let explicit = Args::parse(
            [
                "--cpu-engine=interp".to_string(),
                "--config".to_string(),
                config.0.to_str().unwrap().to_string(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert!(!explicit.jit);
        assert!(Args::parse(["--cpu-engine=invalid".to_string()].into_iter()).is_err());
    }

    struct TestConfig(std::path::PathBuf);

    impl TestConfig {
        fn new(text: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "r86-toml-test-{}-{time}-{id}.toml",
                std::process::id()
            ));
            let mut file = fs::File::create_new(&path).unwrap();
            file.write_all(text.as_bytes()).unwrap();
            Self(path)
        }

        fn parse(&self, cli: &[&str]) -> Result<Args, String> {
            Args::parse(
                ["--config".to_string(), self.0.to_str().unwrap().to_string()]
                    .into_iter()
                    .chain(cli.iter().map(|arg| (*arg).to_string())),
            )
        }
    }

    impl Drop for TestConfig {
        fn drop(&mut self) {
            fs::remove_file(&self.0).unwrap();
        }
    }

    #[test]
    fn toml_supplies_defaults_and_cli_wins_on_either_side_of_config() {
        let config = TestConfig::new(
            "iso = 'configured.iso'\nmem-mib = 64\nsteps = 9_000\nsdl2 = true\ndo-not-exit-when-poweroff = true\n",
        );
        let configured = config.parse(&[]).unwrap();
        assert_eq!(configured.iso.as_deref(), Some("configured.iso"));
        assert_eq!(configured.mem_mib, 64);
        assert_eq!(configured.steps, 9000);
        assert!(configured.sdl2);
        assert!(configured.do_not_exit_when_poweroff);
        assert_eq!(configured.cmdline, "loglevel=3 nozswap");
        for config_first in [false, true] {
            let mut cli = vec![
                "--iso=cli.iso",
                "--mem-mib=32",
                "--mem-mib",
                "256",
                "--steps=0",
                "--sdl2=false",
                "--do-not-exit-when-poweroff=false",
            ];
            let at = if config_first { 0 } else { cli.len() };
            cli.splice(at..at, ["--config", config.0.to_str().unwrap()]);
            let args = Args::parse(cli.into_iter().map(str::to_string)).unwrap();
            assert_eq!(args.iso.as_deref(), Some("cli.iso"));
            assert_eq!(args.mem_mib, 256);
            assert_eq!(args.steps, 0);
            assert!(!args.sdl2);
            assert!(!args.do_not_exit_when_poweroff);
            assert!(args.cmdline.starts_with("console=ttyS0"));
        }
    }

    #[test]
    fn toml_strings_keep_unicode_literals_escapes_and_multiline_commands() {
        let config = TestConfig::new(
            r#"iso = '客机镜像/Tiny Core.iso'
bios = 'C:\images\bios.bin'
cmdline = 'console=ttyS0 # literal comment text'
serial-input = ["first\n", 'second\n']
bootlocal-command = """
echo "hello"
echo world
"""
"#,
        );
        let args = config.parse(&[]).unwrap();
        assert_eq!(args.iso.as_deref(), Some("客机镜像/Tiny Core.iso"));
        assert_eq!(args.bios, PathBuf::from(r"C:\images\bios.bin"));
        assert_eq!(args.cmdline, "console=ttyS0 # literal comment text");
        assert_eq!(args.serial_input, b"first\nsecond\\n");
        assert!(
            String::from_utf8_lossy(&args.initrd_files[0].data)
                .contains("echo \"hello\"\necho world\n")
        );
    }

    #[test]
    fn cli_replaces_configured_repeated_inputs_before_reading_their_files() {
        let config = TestConfig::new(
            "iso = 'test.iso'\nserial-input-file = ['missing-serial-input']\n\
             serial-command = ['configured command']\n\
             initrd-tree = ['missing-initrd-tree']\n\
             bootlocal-command = ['configured boot command']\n",
        );
        let file_spec = format!("{}:tmp/config:644", config.0.display());
        let args = config
            .parse(&[
                "--serial-input",
                "one ",
                "--serial-command",
                "two",
                "--serial-input",
                "three",
                "--initrd-file",
                &file_spec,
                "--bootlocal-command",
                "echo first",
                "--bootlocal-command",
                "echo second",
            ])
            .unwrap();
        assert_eq!(args.serial_input, b"one two\nthree");
        assert_eq!(args.initrd_files.len(), 3);
        assert_eq!(args.initrd_files[0].guest_path, "tmp/config");
        let bootlocal = String::from_utf8_lossy(&args.initrd_files[1].data);
        assert!(bootlocal.contains("echo first\necho second\n"));
        assert!(!bootlocal.contains("configured"));
    }

    #[test]
    fn explicit_cli_booleans_can_disable_config_and_previous_cli_flags() {
        let config = TestConfig::new(
            "hdd-writable = true\nguest-decompress-initrd = true\n\
             host-decompress-kernel = true\nno-vga-bios = true\n",
        );
        let args = config
            .parse(&[
                "--hdd-writable=false",
                "--guest-decompress-initrd=false",
                "--host-decompress-kernel=false",
                "--sdl2",
                "--sdl2=false",
                "--vga-bios",
                "cli-vga.bin",
            ])
            .unwrap();
        assert!(!args.hdd_writable);
        assert!(args.host_decompress_initrd);
        assert!(!args.host_decompress_kernel);
        assert!(!args.sdl2);
        assert_eq!(args.vga_bios, Some(PathBuf::from("cli-vga.bin")));
        let with_bios = TestConfig::new("vga-bios = 'configured-vga.bin'\n");
        assert!(
            with_bios
                .parse(&["--no-vga-bios"])
                .unwrap()
                .vga_bios
                .is_none()
        );
        let restored = with_bios.parse(&["--no-vga-bios=false"]).unwrap();
        assert_eq!(
            restored.vga_bios,
            Some(env::current_exe().unwrap().with_file_name(DEFAULT_VGA_BIOS))
        );
    }

    #[test]
    fn toml_cmdline_stays_explicit_when_frontend_changes() {
        let config = TestConfig::new("cmdline = 'explicit console'\nsdl2 = true\n");
        assert_eq!(
            config.parse(&["--sdl2=false"]).unwrap().cmdline,
            "explicit console"
        );
        assert_eq!(
            config.parse(&["--cmdline", "cli console"]).unwrap().cmdline,
            "cli console"
        );
    }

    #[test]
    fn config_option_is_not_recognized_inside_an_option_value() {
        let args = Args::parse(
            ["--cmdline", "--config", "--serial-input=--help"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();
        assert_eq!(args.cmdline, "--config");
        assert_eq!(args.serial_input, b"--help");
        let err = Args::parse(["--config"].into_iter().map(str::to_string))
            .err()
            .unwrap();
        assert_eq!(err, "--config requires a value");
    }

    #[test]
    fn invalid_toml_options_report_the_file_and_key() {
        for (text, expected) in [
            ("unknown-option = true", "unknown-option"),
            ("[cpu]\ngen = 6", "cpu"),
            ("mem-mib = '128'", "mem-mib expects a non-negative integer"),
            ("steps = -1", "steps expects a non-negative integer"),
            ("sdl2 = 1", "sdl2 expects a boolean"),
            ("iso = false", "iso expects a string"),
            (
                "serial-input = ['valid', 7]",
                "serial-input expects a string or an array of strings",
            ),
            ("iso = 'a'\niso = 'b'", "invalid TOML config"),
            ("iso = \"unterminated", "invalid TOML config"),
            (
                "vga-bios = 'vga.bin'\nno-vga-bios = true",
                "specify either vga-bios or no-vga-bios",
            ),
        ] {
            let config = TestConfig::new(text);
            let error = config.parse(&[]).err().unwrap();
            assert!(error.contains(config.0.to_str().unwrap()), "{error}");
            assert!(error.contains(expected), "{error}");
        }
    }

    #[test]
    fn merged_config_uses_existing_frontend_and_numeric_validation() {
        let config = TestConfig::new("sdl2 = true\nws-listen = '127.0.0.1:8765'\n");
        assert!(
            config
                .parse(&[])
                .err()
                .unwrap()
                .contains("--sdl2 cannot be combined")
        );
        assert!(!config.parse(&["--sdl2=false"]).unwrap().sdl2);
        for text in ["ws-slice-steps = 0", "sdl2-scale = 0", "mem-mib = 0"] {
            assert!(TestConfig::new(text).parse(&[]).is_err());
        }
        let config = TestConfig::new("steps = 9\n");
        assert!(
            config
                .parse(&["--mem-mib", "18446744073709551615"])
                .is_err()
        );
        assert_eq!(
            config
                .parse(&["--steps", "18446744073709551615"])
                .unwrap()
                .steps,
            u64::MAX
        );
    }

    #[test]
    fn config_path_accepts_equals_and_rejects_multiple_files() {
        let config = TestConfig::new("steps = 123\n");
        let arg = format!("--config={}", config.0.display());
        assert_eq!(Args::parse([arg.clone()].into_iter()).unwrap().steps, 123);
        assert!(
            Args::parse([arg.clone(), arg].into_iter())
                .err()
                .unwrap()
                .contains("only be specified once")
        );
    }

    #[test]
    fn serial_command_defaults_to_tinycore_prompt_marker() {
        let args = Args::parse(
            ["--serial-command", "echo ok"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();

        assert_eq!(args.serial_input, b"echo ok\n");
        assert_eq!(args.serial_input_after.as_deref(), Some(&b"tc@box:~$"[..]));
    }

    #[test]
    fn firmware_isolinux_cfg_uses_serial_console_and_default_label() {
        let cfg = firmware_isolinux_cfg("console=ttyS0,115200n8 loglevel=7");

        assert!(cfg.contains("SERIAL 0 115200\n"));
        assert!(cfg.contains("DEFAULT r86\n"));
        assert!(cfg.contains("TIMEOUT 1\n"));
        assert!(cfg.contains("APPEND console=ttyS0,115200n8 loglevel=7\n"));
        assert!(cfg.len() < 1445);
    }

    #[test]
    fn firmware_isolinux_cfg_vga_display_omits_serial_directive() {
        let cfg = firmware_isolinux_cfg_with_display("loglevel=3 nozswap", FirmwareDisplay::Vga);

        assert!(!cfg.contains("SERIAL 0 115200\n"));
        assert!(cfg.contains("DEFAULT r86\n"));
        assert!(cfg.contains("APPEND loglevel=3 nozswap\n"));
        assert!(cfg.len() < 1445);
    }

    #[test]
    fn firmware_iso_prepare_leaves_non_tinycore_iso_unmodified_without_overlays() {
        let mut iso = b"not a tinycore iso".to_vec();
        let original = iso.clone();

        prepare_firmware_iso(&mut iso, "ignored", &[], None, FirmwareDisplay::Vga).unwrap();

        assert_eq!(iso, original);
    }

    #[test]
    fn firmware_iso_prepare_rejects_non_tinycore_overlay() {
        let mut iso = b"not a tinycore iso".to_vec();
        let overlay = [InitrdFile::new("opt/test", 0o644, b"x".to_vec()).unwrap()];

        let err =
            prepare_firmware_iso(&mut iso, "", &overlay, None, FirmwareDisplay::Vga).unwrap_err();

        assert!(err.contains("firmware ISO preparation needs ISO9660 metadata"));
    }

    #[test]
    fn bootlocal_command_adds_executable_overlay() {
        let args = Args::parse(
            ["--iso", "test.iso", "--bootlocal-command", "echo booted"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();

        assert_eq!(args.initrd_files.len(), 2);
        assert_eq!(args.initrd_files[0].guest_path, "opt/bootlocal.sh");
        assert_eq!(args.initrd_files[0].mode, 0o755);
        assert!(args.initrd_files[0].data.starts_with(b"#!/bin/sh\n"));
        assert!(args.initrd_files[0].data.ends_with(b"} >/dev/ttyS0 2>&1\n"));
        assert_eq!(args.initrd_files[1].guest_path, "opt/bootsync.sh");
        assert_eq!(args.initrd_files[1].mode, 0o755);
        assert!(args.initrd_files[1].data.ends_with(b"/opt/bootlocal.sh\n"));
    }

    #[test]
    fn prepared_iso_dump_requires_an_iso() {
        let err = Args::parse(
            ["--dump-prepared-iso", "/tmp/prepared.iso"]
                .into_iter()
                .map(str::to_string),
        )
        .err()
        .unwrap();

        assert_eq!(err, "--dump-prepared-iso requires --iso");
    }

    #[test]
    fn prepared_iso_dump_path_is_preserved() {
        let args = Args::parse(
            [
                "--iso",
                "test.iso",
                "--dump-prepared-iso",
                "/tmp/prepared.iso",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .unwrap();

        assert_eq!(args.dump_prepared_iso.as_deref(), Some("/tmp/prepared.iso"));
    }

    #[test]
    fn websocket_json_helpers_parse_escaped_command_fields() {
        let json =
            r#"{"cmd":"wait-text","text":"tc@box:~$\n","timeout_steps":123,"down":true,"dx":-7}"#;

        assert_eq!(
            json_string(json, "cmd").unwrap().as_deref(),
            Some("wait-text")
        );
        assert_eq!(
            json_string(json, "text").unwrap().as_deref(),
            Some("tc@box:~$\n")
        );
        assert_eq!(json_u64(json, "timeout_steps").unwrap(), Some(123));
        assert_eq!(json_bool(json, "down").unwrap(), Some(true));
        assert_eq!(json_i64(json, "dx").unwrap(), Some(-7));
    }

    #[test]
    fn websocket_key_and_hex_parsers_cover_replay_inputs() {
        assert_eq!(parse_ps2_key("Enter").unwrap(), Ps2Key::Enter);
        assert_eq!(parse_ps2_key("a").unwrap(), Ps2Key::A);
        assert_eq!(parse_hex_bytes("1c f0 1c").unwrap(), vec![0x1c, 0xf0, 0x1c]);
        assert!(parse_hex_bytes("abc").is_err());
    }

    #[test]
    fn terminal_config_cli_override_and_display_defaults() {
        let config = TestConfig::new("term = true\n");
        let args = config.parse(&[]).unwrap();
        assert!(args.term);
        assert_eq!(args.effective_firmware_display(), FirmwareDisplay::Vga);
        assert_eq!(args.cmdline, "loglevel=3 nozswap");
        let args = config.parse(&["--term=false", "--sdl2"]).unwrap();
        assert!(!args.term);
        assert!(config.parse(&["--ws-listen", "127.0.0.1:8765"]).is_err());
        let args = config
            .parse(&["--cmdline", "explicit", "--firmware-display", "serial"])
            .unwrap();
        assert_eq!(args.cmdline, "explicit");
        assert_eq!(args.effective_firmware_display(), FirmwareDisplay::Serial);
    }

    #[test]
    fn sdl2_args_parse_frontend_options() {
        let args = Args::parse(
            [
                "--sdl2",
                "--sdl2-slice-steps",
                "10000",
                "--sdl2-scale",
                "1",
                "--sdl2-grab-mouse",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .unwrap();

        assert!(args.sdl2);
        assert_eq!(args.effective_firmware_display(), FirmwareDisplay::Vga);
        assert_eq!(args.sdl2_slice_steps, 10000);
        assert_eq!(args.sdl2_scale, 1);
        assert!(args.sdl2_grab_mouse);
        assert_eq!(args.cmdline, "loglevel=3 nozswap");
    }

    #[test]
    fn firmware_display_arg_overrides_frontend_default() {
        let args = Args::parse(
            ["--sdl2", "--firmware-display", "serial"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();

        assert_eq!(args.effective_firmware_display(), FirmwareDisplay::Serial);
    }

    #[test]
    fn sdl2_args_preserve_explicit_cmdline() {
        let args = Args::parse(
            ["--sdl2", "--cmdline", "console=ttyS0 custom"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();

        assert_eq!(args.cmdline, "console=ttyS0 custom");
    }

    #[test]
    fn bootsector_mode_rejects_linux_boot_combo() {
        let parsed = Args::parse(
            [
                "--boot-linux-kernel",
                "vmlinuz",
                "--bootsector",
                "tests/bootsectors/ws_ps2_vga.bin",
            ]
            .into_iter()
            .map(str::to_string),
        );
        let err = match parsed {
            Ok(_) => panic!("bootsector plus Linux boot unexpectedly parsed"),
            Err(err) => err,
        };

        assert!(err.contains("--bootsector cannot be combined"));
    }

    #[test]
    fn linux_boot_requires_external_image_pair_and_accepts_no_iso() {
        for argument in ["--boot-linux-kernel=kernel", "--boot-linux-initrd=initrd"] {
            assert!(
                Args::parse([argument.into()].into_iter())
                    .err()
                    .unwrap()
                    .contains("must be specified together")
            );
        }
        let config = TestConfig::new(
            "boot-linux-kernel = 'configured-kernel'\nboot-linux-initrd = 'configured-initrd'\n",
        );
        let args = config
            .parse(&[
                "--boot-linux-kernel=external-kernel",
                "--bootlocal-command=echo ready",
            ])
            .unwrap();
        assert!(args.iso.is_none());
        let BootMode::Linux { kernel, initrd } = args.boot else {
            panic!("expected Linux boot");
        };
        assert_eq!(kernel, PathBuf::from("external-kernel"));
        assert_eq!(initrd, PathBuf::from("configured-initrd"));
        assert!(config.parse(&["--boot-order=cdrom"]).is_err());
    }
}
