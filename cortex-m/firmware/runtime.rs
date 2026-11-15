//! Semihosting, the platform hooks of the Thumb-2 JIT, and the run loop.
//! One core in privileged thread mode and Secure state. SysTick is the only
//! exception in use.
extern crate alloc;

use crate::abi::*;
use core::{
    arch::{asm, global_asm},
    fmt::Write,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed},
};
#[cfg(feature = "mcu")]
use r86::{
    FirmwareDisplay,
    disk::{Disk, SECTOR_SIZE},
};
use r86::{
    machine::Machine,
    x86::{
        Cpu, SegmentReg,
        cpu::{CR0_PE, SEGMENT_FLAG_DEFAULT_BIG},
    },
};

global_asm!(include_str!("entry.S"));

// Arm semihosting (QEMU: -semihosting-config enable=on,target=native).
const SYS_OPEN: usize = 0x01;
const SYS_CLOSE: usize = 0x02;
const SYS_WRITE: usize = 0x05;
const SYS_READ: usize = 0x06;
#[cfg(feature = "mcu")]
const SYS_SEEK: usize = 0x0a;
const SYS_FLEN: usize = 0x0c;
const SYS_EXIT_EXTENDED: usize = 0x20;
const APPLICATION_EXIT: usize = 0x2_0026;

fn semihost(operation: usize, block: &[usize]) -> usize {
    let mut result = operation;
    // SAFETY: the host reads `block` and writes only the buffers it names.
    unsafe { asm!("bkpt 0xab", inout("r0") result, in("r1") block.as_ptr()) };
    result
}

static CONSOLE: AtomicUsize = AtomicUsize::new(0);
static CODE_CAPACITY: AtomicUsize = AtomicUsize::new(CODE_BYTES);
#[cfg(feature = "mcu")]
static DISK: AtomicUsize = AtomicUsize::new(usize::MAX);

fn emit(bytes: &[u8]) {
    semihost(
        SYS_WRITE,
        &[CONSOLE.load(Relaxed), bytes.as_ptr() as usize, bytes.len()],
    );
}

struct Console;
impl Write for Console {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        emit(text.as_bytes());
        Ok(())
    }
}

fn exit(code: usize) -> ! {
    semihost(SYS_EXIT_EXTENDED, &[APPLICATION_EXIT, code]);
    loop {}
}

/// Reads the whole file `name` (NUL-terminated) from the host's directory.
fn load(name: &[u8], into: &mut [u8]) {
    let handle = semihost(SYS_OPEN, &[name.as_ptr() as usize, 1, name.len() - 1]);
    assert!(handle != usize::MAX, "cannot open an input file");
    assert_eq!(semihost(SYS_FLEN, &[handle]), into.len(), "input size");
    let left = semihost(SYS_READ, &[handle, into.as_mut_ptr() as usize, into.len()]);
    assert_eq!(left, 0, "short read of an input file");
    semihost(SYS_CLOSE, &[handle]);
}

struct KeyEvent {
    step: u64,
    bytes: [u8; 16],
    len: usize, // Zero for a host screenshot marker.
}

/// Read before timing. Only replay data is retained during guest execution.
fn load_keys(limit: u64) -> alloc::vec::Vec<KeyEvent> {
    let handle = semihost(SYS_OPEN, &[b"keys.txt\0".as_ptr() as usize, 1, 8]);
    assert_ne!(handle, usize::MAX, "cannot open keys.txt");
    let size = semihost(SYS_FLEN, &[handle]);
    assert!(size <= PC_KEYS_MAX_BYTES, "key script exceeds 64 KiB");
    let mut text = alloc::vec![0; size];
    assert_eq!(
        semihost(SYS_READ, &[handle, text.as_mut_ptr() as usize, size]),
        0,
        "short key script read"
    );
    assert_eq!(semihost(SYS_CLOSE, &[handle]), 0, "key script close");
    assert!(text.is_ascii(), "key script must be ASCII");
    let mut events = alloc::vec::Vec::new();
    let mut previous = 0;
    let mut last_key = None;
    for line in core::str::from_utf8(&text).unwrap().lines() {
        let mut fields = line.split('#').next().unwrap().split_ascii_whitespace();
        let Some(step) = fields.next() else { continue };
        let step = step.parse::<u64>().expect("key script step");
        assert!(step >= previous && step <= limit, "key script step/order");
        previous = step;
        let first = fields.next().expect("key script event");
        let mut event = KeyEvent {
            step,
            bytes: [0; 16],
            len: 0,
        };
        if first == "!shot" {
            let name = fields.next().expect("shot name");
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "shot name"
            );
            assert!(fields.next().is_none(), "extra shot fields");
        } else {
            assert!(
                last_key.is_none_or(|last| step - last >= 2_000_000),
                "key event spacing"
            );
            last_key = Some(step);
            for byte in core::iter::once(first).chain(fields) {
                assert!(
                    byte.len() == 2 && event.len < event.bytes.len(),
                    "key bytes"
                );
                event.bytes[event.len] = u8::from_str_radix(byte, 16).expect("set-2 byte");
                event.len += 1;
            }
        }
        assert!(events.len() < PC_KEYS_MAX_EVENTS, "too many key events");
        events.push(event);
    }
    events
}

// The seed is opened read-only. Disk::frozen owns any guest writes in RAM;
// the hook never writes the user's host file or loads it wholesale.
#[cfg(feature = "mcu")]
#[unsafe(no_mangle)]
unsafe extern "C" fn r86_disk_io(
    drive: u32,
    operation: u32,
    sector: u32,
    bytes: *mut u8,
    len: usize,
) -> i32 {
    if drive != 0 {
        return -1;
    }
    match operation {
        0 => {
            let Some(offset) = (sector as usize).checked_mul(SECTOR_SIZE) else {
                return -1;
            };
            let handle = DISK.load(Relaxed);
            if handle == usize::MAX || semihost(SYS_SEEK, &[handle, offset]) != 0 {
                return -1;
            }
            if semihost(SYS_READ, &[handle, bytes as usize, len]) == 0 {
                0
            } else {
                -1
            }
        }
        2 => 0, // No persistent writes on this read-only backend.
        _ => -1,
    }
}

#[cfg(feature = "mcu")]
fn pc(ram: &'static mut [u8]) -> Machine {
    ram.fill(0);
    let mut machine = Machine::new_firmware_in(ram, FirmwareDisplay::Vga).unwrap();
    machine
        .load_bios_rom(include_bytes!("../../firmware/seabios/bios.bin"))
        .unwrap();
    machine
        .load_option_rom(
            0xc0000,
            include_bytes!("../../firmware/seabios/vgabios-stdvga.bin"),
        )
        .unwrap();
    let handle = semihost(SYS_OPEN, &[b"disk.img\0".as_ptr() as usize, 1, 8]);
    assert_ne!(handle, usize::MAX, "cannot open disk.img");
    let bytes = semihost(SYS_FLEN, &[handle]);
    assert!(bytes != usize::MAX && bytes % SECTOR_SIZE == 0, "disk size");
    DISK.store(handle, Relaxed);
    machine.attach_disk(
        Disk::platform(0, (bytes / SECTOR_SIZE) as u64, false)
            .unwrap()
            .frozen(),
    );
    machine
}

/// frame.bin: little-endian u32 width, u32 height, then tightly packed RGBA.
/// Rendering is after the measured run; its allocation still counts in the heap.
#[cfg(feature = "mcu")]
fn write_frame(machine: &Machine) {
    let info = machine.vga().mode_info();
    let width = u32::from(info.width);
    let height = u32::from(info.height);
    let mut pixels = alloc::vec![0; width as usize * height as usize * 4];
    machine.vga().render_rgba(&mut pixels).expect("VGA render");
    let mut header = [0u8; 8];
    header[..4].copy_from_slice(&width.to_le_bytes());
    header[4..].copy_from_slice(&height.to_le_bytes());
    let file = semihost(SYS_OPEN, &[b"frame.bin\0".as_ptr() as usize, 5, 9]);
    assert_ne!(file, usize::MAX, "cannot create frame.bin");
    for bytes in [&header[..], &pixels[..]] {
        assert_eq!(
            semihost(SYS_WRITE, &[file, bytes.as_ptr() as usize, bytes.len()]),
            0,
            "frame write"
        );
    }
    assert_eq!(semihost(SYS_CLOSE, &[file]), 0, "frame close");
}

#[unsafe(no_mangle)]
unsafe extern "C" fn r86_t32_code_region(bytes: *mut usize) -> *mut u8 {
    // SAFETY: the caller passes a valid out pointer; abi.rs reserves the region.
    unsafe { *bytes = CODE_CAPACITY.load(Relaxed) };
    CODE as *mut u8
}

#[unsafe(no_mangle)]
unsafe extern "C" fn r86_t32_code_publish(start: *const u8, bytes: usize) {
    // Armv8-M cache maintenance by address, in the order CMSIS uses: clean the
    // data cache for the whole range and let that complete, then invalidate
    // the instruction cache for it. A core without caches (QEMU's model)
    // ignores the writes; on the SF32LB52 the lines are 32 bytes.
    const DCCMVAC: *mut u32 = 0xe000_ef68 as *mut u32;
    const ICIMVAU: *mut u32 = 0xe000_ef58 as *mut u32;
    let first = start as usize & !31;
    let end = start as usize + bytes;
    for register in [DCCMVAC, ICIMVAU] {
        let mut line = first;
        while line < end {
            // SAFETY: a system control register of this core.
            unsafe { register.write_volatile(line as u32) };
            line += 32;
        }
        // SAFETY: a barrier has no operands.
        unsafe { asm!("dsb") };
    }
    // SAFETY: a barrier has no operands.
    unsafe { asm!("isb") };
}

// SysTick counts down on the processor clock. Under QEMU's -icount its ticks
// are proportional to executed instructions; `calibrate` finds the ratio.
// Each wrap also samples the interrupted program counter, which makes a flat
// profile with one sample per `RELOAD + 1` ticks (about 100,000 instructions).
const SYST_CSR: *mut u32 = 0xe000_e010 as *mut u32;
const SYST_RVR: *mut u32 = 0xe000_e014 as *mut u32;
const SYST_CVR: *mut u32 = 0xe000_e018 as *mut u32;
const RELOAD: u32 = 3202;
static WRAPS: AtomicU32 = AtomicU32::new(0);
static SAMPLES: AtomicU32 = AtomicU32::new(0);

#[unsafe(no_mangle)]
extern "C" fn systick_sample(frame: *const u32) {
    WRAPS.fetch_add(1, Relaxed);
    let count = SAMPLES.load(Relaxed) as usize;
    if count < PROFILE_BYTES / 4 {
        // SAFETY: word 6 of the exception frame is the interrupted PC, and
        // abi.rs reserves the buffer.
        unsafe { ((PROFILE + 4 * count) as *mut u32).write(frame.add(6).read()) };
        SAMPLES.store(count as u32 + 1, Relaxed);
    }
}

fn ticks() -> u64 {
    loop {
        let wraps = WRAPS.load(Relaxed);
        // SAFETY: a system control register of this core.
        let current = unsafe { SYST_CVR.read_volatile() };
        if WRAPS.load(Relaxed) == wraps {
            return u64::from(wraps) * u64::from(RELOAD + 1) + u64::from(RELOAD - current);
        }
    }
}

/// Ticks taken by two million instructions.
fn calibrate() -> u64 {
    let start = ticks();
    let mut count = 1_000_000u32;
    // SAFETY: a counted loop on one register.
    unsafe { asm!("2: subs {0}, #1", "bne 2b", inout(reg) count) };
    let _ = count;
    ticks() - start
}

#[unsafe(no_mangle)]
extern "C" fn fault_report(frame: *const u32, exc_return: u32) -> ! {
    // SAFETY: system control block registers and the stacked exception frame.
    let scb = |offset: usize| unsafe { ((0xe000_ed00 + offset) as *const u32).read_volatile() };
    let stacked = |index: usize| unsafe { frame.add(index).read() };
    let _ = writeln!(
        Console,
        "\nfault: pc={:#010x} lr={:#010x} xpsr={:#010x} exc_return={exc_return:#010x} cfsr={:#010x} hfsr={:#010x} sfsr={:#010x} mmfar={:#010x} bfar={:#010x}",
        stacked(6),
        stacked(5),
        stacked(7),
        scb(0x28),
        scb(0x2c),
        scb(0xe4),
        scb(0x34),
        scb(0x38)
    );
    exit(8)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let _ = writeln!(Console, "\nfirmware panic: {info}");
    exit(5)
}

#[unsafe(no_mangle)]
extern "C" fn machine_main() -> ! {
    CONSOLE.store(
        semihost(SYS_OPEN, &[b":tt\0".as_ptr() as usize, 4, 3]),
        Relaxed,
    );
    // SAFETY: system control registers of this core.
    unsafe {
        SYST_RVR.write_volatile(RELOAD);
        SYST_CVR.write_volatile(0);
        SYST_CSR.write_volatile(7);
    }
    let mut config = [0u8; 32];
    load(b"config.bin\0", &mut config);
    let word =
        |index: usize| u32::from_le_bytes(config[4 * index..4 * index + 4].try_into().unwrap());
    assert_eq!(word(0), MAGIC);
    assert_eq!(word(1), 1);
    let limit = u64::from(word(5)) | u64::from(word(6)) << 32;
    let jit = word(7) & WORKLOAD_JIT != 0;
    let firmware_pc = word(7) & WORKLOAD_PC != 0;
    let replay_keys = word(7) & WORKLOAD_PC_KEYS != 0;
    assert!(!replay_keys || firmware_pc, "keys require PC mode");
    assert_eq!(
        word(7) & !(WORKLOAD_JIT | WORKLOAD_PC | WORKLOAD_PC_KEYS),
        0,
        "unsupported workload flags"
    );
    let guest_bytes = if firmware_pc {
        PC_GUEST_BYTES
    } else {
        GUEST_BYTES
    };
    assert_eq!(word(4), guest_bytes as u32);
    if firmware_pc {
        assert!(cfg!(feature = "mcu"), "PC mode requires --features mcu");
        crate::heap::limit_to(PC_HEAP_BYTES);
        CODE_CAPACITY.store(PC_CODE_BYTES, Relaxed);
        // Keep the rough model within a 1 MiB native stack; addresses remain
        // those of MPS3, not the SF32 board. The heap starts at the stack top.
        unsafe { asm!("msr msplim, {0}", in(reg) (HEAP - PC_STACK_BYTES)) };
    }
    // SAFETY: abi.rs reserves this region for guest RAM alone.
    let ram = unsafe { core::slice::from_raw_parts_mut(GUEST as *mut u8, guest_bytes) };
    let mut machine = if firmware_pc {
        #[cfg(feature = "mcu")]
        {
            pc(ram)
        }
        #[cfg(not(feature = "mcu"))]
        {
            unreachable!()
        }
    } else {
        load(b"guest.ram\0", ram);
        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(word(2), word(3));
        Machine::from_preloaded(cpu, ram)
    };
    machine.set_jit_enabled(jit).unwrap();
    let keys = if replay_keys {
        load_keys(limit)
    } else {
        alloc::vec::Vec::new()
    };
    let mut key_index = 0;
    let mut key_count = 0;
    let _ = writeln!(
        Console,
        "r86 on a Cortex-M33, {}",
        if jit { "Thumb-2 JIT" } else { "interpreter" }
    );
    let calibration = calibrate();
    SAMPLES.store(0, Relaxed);
    let start = ticks();
    let mut sent = 0;
    let mut observed = 0;
    let mut marker = false;
    let mut modes = [0u64; 3];
    let mut mode_windows = [[0u64; 3]; PC_MODE_WINDOWS];
    let mut window_ticks = [0u64; PC_MODE_WINDOWS];
    let mut previous_ticks = start;
    let mut next_window = 25_000_000u64;
    loop {
        while let Some(event) = keys
            .get(key_index)
            .filter(|event| event.step <= machine.step_count())
        {
            assert_eq!(event.step, machine.step_count(), "key event step mismatch");
            if event.len != 0 {
                // Same call as the WebSocket send-scancodes handler, including
                // draining the resulting PS/2 interrupt effects.
                machine.inject_scancode_set2(&event.bytes[..event.len]);
                key_count += 1;
            }
            key_index += 1;
        }
        if machine.step_count() == limit {
            break;
        }
        // Serial exits may split a slice. Sample only at the fixed 4096-step
        // boundaries (and the final partial slice), after guest execution.
        let mut boundary = if firmware_pc {
            (4096 - machine.step_count() % 4096).min(next_window - machine.step_count())
        } else {
            4096
        };
        if let Some(event) = keys.get(key_index) {
            boundary = boundary.min(event.step - machine.step_count());
        }
        let result = machine.run_until_serial_output((limit - machine.step_count()).min(boundary));
        if firmware_pc && (machine.step_count() % 4096 == 0 || machine.step_count() == limit) {
            let cpu = machine.cpu();
            let mode = if cpu.cr0 & CR0_PE == 0 || cpu.eflags & (1 << 17) != 0 {
                0 // Real mode or virtual 8086.
            } else if cpu.seg(SegmentReg::Cs).flags & SEGMENT_FLAG_DEFAULT_BIG == 0 {
                1
            } else {
                2
            };
            modes[mode] += 1;
            if let Some(window) =
                mode_windows.get_mut(((machine.step_count() - 1) / 25_000_000) as usize)
            {
                window[mode] += 1;
            }
        }
        if firmware_pc && machine.step_count() == next_window {
            let now = ticks();
            if let Some(window) = window_ticks.get_mut((next_window / 25_000_000 - 1) as usize) {
                *window = now - previous_ticks;
            }
            previous_ticks = now;
            next_window += 25_000_000;
            // Complete cumulative diagnostics at the boot/apps boundary.
            // Subtract this snapshot from the final report on the host. Plain
            // builds perform neither the allocation nor semihosting here.
            #[cfg(feature = "jit-profile")]
            if replay_keys && machine.step_count() == 250_000_000 {
                let _ = writeln!(
                    Console,
                    "jit_checkpoint: step=250000000\n{}end-jit_checkpoint",
                    machine.jit_report()
                );
            }
        }
        let serial = machine.serial_output();
        emit(&serial[sent..]);
        sent = serial.len();
        if !firmware_pc
            && serial[observed..]
                .windows(MARKER.len())
                .any(|window| window == MARKER)
        {
            marker = true;
            break;
        }
        observed = sent.saturating_sub(MARKER.len() - 1);
        match result {
            Ok(report) if report.stop.is_none() => (),
            other => {
                let _ = writeln!(Console, "\nguest stop: {other:?}");
                exit(6)
            }
        }
    }
    let spent = ticks() - start;
    // Freeze profiling before snapshot rendering, file I/O and reporting.
    unsafe { SYST_CSR.write_volatile(0) };
    let run_heap_high_water = crate::heap::high_water();
    if firmware_pc {
        assert_eq!(key_index, keys.len(), "unconsumed key events");
        if replay_keys {
            let _ = writeln!(
                Console,
                "keys: records={key_index} injected={key_count} exact_steps=true"
            );
        }
        let text = machine.text_snapshot();
        // PC pass rules (DOS text or a reference frame) belong to pc.py.
        marker = machine.step_count() == limit;
        let _ = writeln!(Console, "screen:\n{text}\nend-screen");
        #[cfg(feature = "mcu")]
        write_frame(&machine);
        let total = modes.iter().sum::<u64>();
        let _ = writeln!(
            Console,
            "batch_modes: total={total} real_v86={} protected16={} protected32={} shares_pct_x100={},{},{}",
            modes[0],
            modes[1],
            modes[2],
            modes[0] * 10000 / total.max(1),
            modes[1] * 10000 / total.max(1),
            modes[2] * 10000 / total.max(1)
        );
        let _ = writeln!(
            Console,
            "memory: heap_high_water_bytes={} run_heap_high_water_bytes={run_heap_high_water} heap_limit_bytes={} machine_bytes={} guest_bytes={guest_bytes} code_bytes={} stack_limit_bytes={PC_STACK_BYTES}",
            crate::heap::high_water(),
            PC_HEAP_BYTES,
            core::mem::size_of::<Machine>(),
            PC_CODE_BYTES
        );
        for (index, counts) in mode_windows.iter().enumerate() {
            if index as u64 * 25_000_000 >= limit {
                break;
            }
            let total = counts.iter().sum::<u64>();
            let _ = writeln!(
                Console,
                "batch_modes_window: end_step={} total={total} real_v86={} protected16={} protected32={} ticks={} arm_instructions={}",
                (index + 1) * 25_000_000,
                counts[0],
                counts[1],
                counts[2],
                window_ticks[index],
                window_ticks[index] * 2_000_000 / calibration.max(1)
            );
        }
    }
    // The sample buffer is a measurement aid of this model, outside the PC
    // mode's memory budget.
    let samples = SAMPLES.load(Relaxed) as usize;
    let _ = writeln!(
        Console,
        "profile: samples={samples} capacity_samples={} full={}",
        PROFILE_BYTES / 4,
        samples == PROFILE_BYTES / 4
    );
    let file = semihost(SYS_OPEN, &[b"profile.bin\0".as_ptr() as usize, 5, 11]);
    semihost(SYS_WRITE, &[file, PROFILE, 4 * samples]);
    semihost(SYS_CLOSE, &[file]);
    let steps = machine.step_count();
    let instructions = spent * 2_000_000 / calibration.max(1);
    let hash = machine
        .ram()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    let _ = writeln!(
        Console,
        "\nresult: marker={marker} x86_steps={steps} eip={} eflags={} tsc={} guest_ram_fnv64={hash:016x}",
        machine.cpu().eip,
        machine.cpu().effective_eflags(),
        machine.cpu().tsc()
    );
    // Meaningful only under -icount, where SysTick follows the instruction count.
    let _ = writeln!(
        Console,
        "cost: ticks={spent} calibration={calibration} arm_instructions={instructions} per_x86_step_x100={}",
        instructions * 100 / steps.max(1)
    );
    let _ = writeln!(Console, "{}", machine.jit_report());
    exit(if marker { 0 } else { 4 })
}
