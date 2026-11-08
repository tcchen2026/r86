//! Full shared x86 interpreter on native RV32IM; host build prepares boot RAM.
#![cfg_attr(target_arch = "riscv32", no_std)]
#![cfg_attr(target_arch = "riscv32", no_main)]

#[path = "machine_abi.rs"]
mod abi;

#[cfg(target_arch = "riscv32")]
mod machine_runtime;

#[cfg(all(not(target_arch = "riscv32"), not(feature = "host")))]
fn main() {
    eprintln!(
        "Host image preparation requires --features host; use r86chip/experiments/tinycore.py"
    );
    std::process::exit(1);
}

#[cfg(all(not(target_arch = "riscv32"), feature = "host"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use r86::{
        initrd_overlay::InitrdFile, iso9660::Iso9660, linux_boot::BootConfig, machine::Machine,
    };
    use std::{fs, io::Write, path::Path};
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 4 {
        return Err(
            "usage: r86chip-machine TINYCORE.iso OUTPUT_DIRECTORY X86_STEP_LIMIT [--reference] [--lua-tree DIR] [--jit] [--boundaries]"
                .into(),
        );
    }
    let mut reference = false;
    let mut jit = false;
    let mut boundaries = false;
    let mut lua_tree = None;
    let mut options = args[4..].iter();
    while let Some(option) = options.next() {
        match option.as_str() {
            "--reference" => reference = true,
            "--jit" => jit = true,
            "--boundaries" => boundaries = true,
            "--lua-tree" => lua_tree = Some(options.next().ok_or("--lua-tree requires a path")?),
            _ => return Err(format!("unknown option: {option}").into()),
        }
    }
    let limit = args[3].parse::<u64>()?;
    let out = Path::new(&args[2]);
    fs::create_dir_all(out)?;
    let image = fs::read(&args[1])?;
    let iso = Iso9660::open(&image)?;
    let mut overlay = vec![
        InitrdFile::new(
            "opt/bootlocal.sh",
            0o755,
            b"#!/bin/sh\n{ uname -a; echo R86_CHIP_TINYCORE_OK; } >/dev/ttyS0 2>&1\n".to_vec(),
        )?,
        InitrdFile::new(
            "opt/bootsync.sh",
            0o755,
            b"#!/bin/sh\n/usr/bin/sethostname box\n/opt/bootlocal.sh\n".to_vec(),
        )?,
    ];
    let marker = if let Some(tree) = lua_tree {
        overlay.extend(r86::initrd_overlay::entries_from_host_tree(tree)?);
        overlay[0] = InitrdFile::new(
            "opt/bootlocal.sh",
            0o755,
            include_bytes!("../experiments/workloads/bootlocal-lua.sh").to_vec(),
        )?;
        overlay.push(InitrdFile::new(
            "opt/r86chip-workload.lua",
            0o644,
            include_bytes!("../experiments/workloads/verify.lua").to_vec(),
        )?);
        abi::LUA_MARKER
    } else {
        abi::MARKER
    };
    // Explicit direct-kernel boot: host prepares/decompresses immutable inputs;
    // every x86 instruction and device operation afterwards runs on RV32.
    let mut machine = Machine::new(abi::GUEST_BYTES);
    let cmdline =
        "console=ttyS0,115200n8 earlyprintk=serial,ttyS0,115200 loglevel=7 nozswap lpj=1000";
    let boot = machine.load_linux(
        &iso.read_file("boot/vmlinuz")?,
        &iso.read_file("boot/core.gz")?,
        &BootConfig {
            mem_size: abi::GUEST_BYTES as u32,
            cmdline,
            host_decompress_kernel: true,
            host_decompress_initrd: true,
            overlay_files: &overlay,
            dump_initrd_path: None,
        },
    )?;
    fs::write(out.join("guest.ram"), machine.ram())?;
    let words = [
        abi::MAGIC,
        1,
        boot.entry,
        boot.boot_params,
        abi::GUEST_BYTES as u32,
        limit as u32,
        (limit >> 32) as u32,
        (if lua_tree.is_some() {
            abi::WORKLOAD_LUA
        } else {
            0
        }) | if jit { abi::WORKLOAD_JIT } else { 0 }
            | if boundaries {
                abi::WORKLOAD_BOUNDARIES
            } else {
                0
            },
    ];
    let config: Vec<_> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    fs::write(out.join("config.bin"), config)?;
    println!(
        "guest_ram={} entry={:#x} boot_params={:#x} kernel={} initrd={} cmdline={cmdline}",
        machine.ram().len(),
        boot.entry,
        boot.boot_params,
        boot.kernel_payload_len,
        boot.initrd_len
    );
    if reference {
        // An independent host run checks the exact boot inputs and expected
        // endpoint. It is never used to execute instructions for either RV run.
        let start = std::time::Instant::now();
        let mut observed = 0;
        let mut passed = false;
        let mut boundary_log = if boundaries {
            Some(std::io::BufWriter::new(fs::File::create(
                out.join("host-boundaries.bin"),
            )?))
        } else {
            None
        };
        while machine.step_count() < limit {
            let report =
                machine.run_until_serial_output((limit - machine.step_count()).min(4096))?;
            if let Some(log) = &mut boundary_log {
                for word in abi::boundary(machine.step_count(), machine.cpu()) {
                    log.write_all(&word.to_le_bytes())?;
                }
            }
            let serial = machine.serial_output();
            if serial[observed..]
                .windows(marker.len())
                .any(|s| s == marker)
            {
                passed = true;
                break;
            }
            observed = serial.len().saturating_sub(marker.len() - 1);
            if report.stop.is_some() {
                return Err(format!("host reference stopped: {report:?}").into());
            }
        }
        if let Some(log) = &mut boundary_log {
            log.flush()?;
        }
        fs::write(out.join("host-serial.log"), machine.serial_output())?;
        let seconds = start.elapsed().as_secs_f64();
        let guest_hash = machine.ram().iter().fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        });
        let result = format!(
            "{{\"engine\":\"host-x86-reference\",\"x86_steps\":{},\"eip\":{},\"marker\":{},\"seconds\":{:.3},\"guest_ram_fnv64\":\"{:016x}\",\"x86_regs\":{:?},\"eflags\":{},\"tsc\":{}}}\n",
            machine.step_count(),
            machine.cpu().eip,
            passed,
            seconds,
            guest_hash,
            machine.cpu().regs,
            machine.cpu().effective_eflags(),
            machine.cpu().tsc()
        );
        fs::write(out.join("host-run.json"), &result)?;
        print!("{result}");
        if !passed {
            return Err("host reference did not reach boot marker".into());
        }
    }
    Ok(())
}
