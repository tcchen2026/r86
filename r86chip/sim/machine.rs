//! Large-RAM RV32 execution of the real no_std PC interpreter.
//!
//! This is the host execution harness, not an alternate implementation of the
//! chip ISA. `rv32::Core` executes every firmware and generated-code word. The
//! firmware owns x86 semantics, JIT compilation, device emulation and guest time;
//! the harness only loads bytes, runs native instructions and observes RAM.
//! `machine_abi.rs` is the shared source of memory regions and observation fields.
//!
//! ELF/config/guest images are loaded before the first fetch. At instruction
//! chunk boundaries we copy firmware-published output to files/stdout, without
//! advancing guest time. A firmware marker alone does not stop execution here:
//! final state is sampled after a native trap or a harness instruction/wall limit.
//! The comparison script decides whether the terminal trap, status and required
//! TinyCore/Lua assertions constitute a complete pass. A limit is not success.
use super::{memory::Memory, rv32};
#[path = "../firmware/machine_abi.rs"]
mod abi;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufWriter, Write},
    path::Path,
    time::Instant,
};

/// Host-side attribution, outside the simulated firmware and ISA. Count every
/// retired word at its native PC, including startup/helpers/devices. The dense
/// firmware range avoids hashing on the common path; other PCs remain counted.
struct Profile {
    firmware: Vec<u64>,
    other: BTreeMap<u32, u64>,
}

impl Profile {
    fn new() -> Self {
        Self {
            firmware: vec![0; abi::CONFIG / 4],
            other: BTreeMap::new(),
        }
    }

    fn run(
        &mut self,
        core: &mut rv32::Core,
        memory: &mut Memory,
        steps: u64,
    ) -> Result<rv32::Trap, String> {
        for _ in 0..steps {
            let pc = core.pc;
            if let Some(trap) = core.step(memory)? {
                return Ok(trap);
            }
            if let Some(count) = self.firmware.get_mut(pc as usize / 4) {
                *count += 1;
            } else {
                *self.other.entry(pc).or_default() += 1;
            }
        }
        Ok(rv32::Trap {
            pc: core.pc,
            cause: rv32::Cause::StepLimit,
            value: 0,
        })
    }

    fn write(&self, path: &Path, retired: u64) -> std::io::Result<()> {
        let mut out = BufWriter::new(fs::File::create(path)?);
        writeln!(out, "pc,retired")?;
        let mut total = 0;
        for (pc, count) in self
            .firmware
            .iter()
            .enumerate()
            .filter(|(_, n)| **n != 0)
            .map(|(i, n)| ((i * 4) as u32, *n))
            .chain(self.other.iter().map(|(p, n)| (*p, *n)))
        {
            writeln!(out, "0x{pc:08x},{count}")?;
            total += count;
        }
        assert_eq!(total, retired, "profile lost native retirements");
        out.flush()
    }
}

pub fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() < 5
        || args[5..]
            .iter()
            .any(|a| a != "--profile" && a != "--dump-guest" && a != "--tlb-stats")
    {
        return Err(
            "machine FIRMWARE.elf IMAGE_DIRECTORY RV_INSTRUCTION_LIMIT WALL_SECONDS [--profile] [--dump-guest] [--tlb-stats]"
                .into(),
        );
    }
    let limit = args[3].parse::<u64>()?;
    let seconds = args[4].parse::<f64>()?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err("invalid wall limit".into());
    }
    let dir = Path::new(&args[2]);
    let mut memory = Memory::new(abi::NATIVE_BYTES);
    let entry = rv32::load_elf(&fs::read(&args[1])?, &mut memory)?;
    let guest = fs::read(dir.join("guest.ram"))?;
    if guest.len() != abi::GUEST_BYTES {
        return Err("wrong guest RAM size".into());
    }
    memory.write(abi::GUEST as u32, &guest)?;
    memory.write(abi::CONFIG as u32, &fs::read(dir.join("config.bin"))?)?;
    // Start from the ELF entry. The firmware's native startup code initializes
    // its stack and runtime; no x86 execution or translation is delegated to host.
    let mut core = rv32::Core::new(entry);
    if args[5..].iter().any(|a| a == "--tlb-stats") {
        core.tlb_stats = Some(Box::default());
    }
    let mut serial = fs::File::create(dir.join("iss-serial.log"))?;
    let mut sent = 0;
    let mut progress = Instant::now();
    let start = Instant::now();
    let mut stop = None;
    let mut profile = args[5..]
        .iter()
        .any(|a| a == "--profile")
        .then(Profile::new);
    while core.host_steps < limit && start.elapsed().as_secs_f64() < seconds {
        // Chunking bounds host observation latency only. StepLimit resumes the
        // same core state; no reset, interrupt or guest instruction is injected.
        let chunk = (limit - core.host_steps).min(1_000_000);
        let trap = if let Some(profile) = &mut profile {
            profile.run(&mut core, &mut memory, chunk)?
        } else {
            core.run(&mut memory, chunk)?
        };
        // The producer publishes its byte count after writing output. Accept
        // only monotonically growing, in-range data; a broken ABI is an error,
        // never silently repaired by clipping or restarting the firmware.
        let end = memory.read(abi::OUTPUT_LEN as u32, 32) as usize;
        if end < sent || end > abi::OUTPUT_BYTES {
            return Err("invalid firmware output length".into());
        }
        let bytes = &memory.ram[abi::OUTPUT + sent..abi::OUTPUT + end];
        std::io::stdout().write_all(bytes)?;
        std::io::stdout().flush()?;
        serial.write_all(bytes)?;
        sent = end;
        if trap.cause != rv32::Cause::StepLimit {
            stop = Some(trap);
            break;
        }
        if progress.elapsed().as_secs() >= 5 {
            eprintln!(
                "ISS {:.1}s rv={} x86={} eip={:#x} pc={:#x}",
                start.elapsed().as_secs_f64(),
                core.host_steps,
                memory.read(abi::STEPS_LO as u32, 32),
                memory.read(abi::EIP as u32, 32),
                core.pc
            );
            progress = Instant::now();
        }
    }
    serial.flush()?;
    let execution_seconds = start.elapsed().as_secs_f64();
    if let Some(stats) = &core.tlb_stats {
        fs::write(dir.join("iss-tlb-stats.json"), stats.json())?;
    }
    if args[5..].iter().any(|a| a == "--dump-guest") {
        // Host-only mismatch diagnosis; no new simulated instructions or bus
        // reads. Keep it opt-in because the output is a full 128 MiB image.
        fs::write(
            dir.join("iss-guest-final.bin"),
            &memory.ram[abi::GUEST..abi::GUEST + abi::GUEST_BYTES],
        )?;
    }
    if let Some(profile) = profile {
        profile.write(&dir.join("iss-profile.csv"), core.retired_steps)?;
    }
    if memory.read(abi::WORKLOAD_FLAGS as u32, 32) & abi::WORKLOAD_BOUNDARIES != 0 {
        let len = memory.read(abi::BOUNDARY_LEN as u32, 32) as usize;
        if len > abi::BOUNDARY_BYTES || len % (abi::BOUNDARY_WORDS * 4) != 0 {
            return Err("invalid boundary trace length".into());
        }
        fs::write(
            dir.join("iss-boundaries.bin"),
            &memory.ram[abi::BOUNDARIES..abi::BOUNDARIES + len],
        )?;
    }
    // Compare the complete native address space in bounded ISS/RTL regressions,
    // including guest RAM, stack and allocator state. Outside the execution loop.
    let ram_hash = memory.ram.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    let guest_hash = memory.ram[abi::GUEST..abi::GUEST + abi::GUEST_BYTES]
        .iter()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
        });
    let read64 = |addr: usize| {
        u64::from(memory.read(addr as u32, 32))
            | (u64::from(memory.read(addr as u32 + 4, 32)) << 32)
    };
    // These are firmware-exported x86 registers/flags/time. They are distinct
    // from core.x (the currently active native RF) and core.host_steps. Older
    // saved firmware without these exports cannot supply a final-CPU comparison.
    let regs: [u32; 8] = core::array::from_fn(|n| memory.read((abi::CPU_REGS + n * 4) as u32, 32));
    let status = memory.read(abi::STATUS as u32, 32);
    let steps = u64::from(memory.read(abi::STEPS_LO as u32, 32))
        | (u64::from(memory.read(abi::STEPS_HI as u32, 32)) << 32);
    let output = &memory.ram[abi::OUTPUT..abi::OUTPUT + sent];
    let boot_marker = output.windows(abi::MARKER.len()).any(|s| s == abi::MARKER);
    let lua_marker = output
        .windows(abi::LUA_MARKER.len())
        .any(|s| s == abi::LUA_MARKER);
    let marker = boot_marker
        && (memory.read(abi::WORKLOAD_FLAGS as u32, 32) & abi::WORKLOAD_LUA == 0 || lua_marker);
    let summary = format!(
        "{{\"engine\":\"rust-rv32-functional\",\"rv_instructions\":{},\"x86_steps\":{},\"eip\":{},\"pc\":{},\"status\":{},\"marker\":{},\"boot_marker\":{},\"lua_marker\":{},\"seconds\":{:.3},\"trap\":\"{:?}\",\"ram_fnv64\":\"{:016x}\",\"guest_ram_fnv64\":\"{:016x}\",\"bank_enters\":{},\"jit_native_steps\":{},\"jit_invocations\":{},\"jit_compiled_blocks\":{},\"jit_user_steps\":{},\"jit_invalidations\":{},\"x86_regs\":{:?},\"eflags\":{},\"tsc\":{},\"retired_native\":{},\"chip_extensions\":{},\"xalu_ops\":{},\"tlb_lookups\":{},\"tlb_hits\":{},\"tlb_fills\":{},\"xld_ops\":{},\"xld_hits\":{},\"xea_ops\":{},\"xea_hits\":{},\"x86_completed\":{},\"x86_fault_attempts\":{},\"x86_repeat_chunks\":{},\"x86_halted_polls\":{}}}\n",
        core.host_steps,
        steps,
        memory.read(abi::EIP as u32, 32),
        core.pc,
        status,
        marker,
        boot_marker,
        lua_marker,
        execution_seconds,
        stop,
        ram_hash,
        guest_hash,
        core.bank_enters,
        read64(abi::JIT_NATIVE),
        read64(abi::JIT_INVOKES),
        read64(abi::JIT_BLOCKS),
        read64(abi::JIT_USER),
        read64(abi::JIT_INVALIDATIONS),
        regs,
        memory.read(abi::CPU_FLAGS as u32, 32),
        read64(abi::CPU_TSC),
        core.retired_steps,
        memory.read(abi::CHIP_EXTENSIONS as u32, 32),
        core.xalu_ops,
        core.tlb_lookups,
        core.tlb_hits,
        core.tlb_fills,
        core.xld_ops,
        core.xld_hits,
        core.xea_ops,
        core.xea_hits,
        read64(abi::X86_COMPLETED),
        read64(abi::X86_FAULT_ATTEMPTS),
        read64(abi::X86_REPEAT_CHUNKS),
        read64(abi::X86_HALTED_POLLS)
    );
    fs::write(dir.join("iss-run.json"), &summary)?;
    eprint!("{summary}");
    if stop.is_some_and(|t| t.cause != rv32::Cause::Breakpoint) || matches!(status, 5..=7) {
        return Err("firmware or RV32 execution failed; see saved report".into());
    }
    Ok(())
}
