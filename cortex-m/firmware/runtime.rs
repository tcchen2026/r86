//! Semihosting, the platform hooks of the Thumb-2 JIT, and the run loop.
//! One core in privileged thread mode and Secure state. SysTick is the only
//! exception in use.
use crate::abi::*;
use core::{
    arch::{asm, global_asm},
    fmt::Write,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed},
};
use r86::{machine::Machine, x86::Cpu};

global_asm!(include_str!("entry.S"));

// Arm semihosting (QEMU: -semihosting-config enable=on,target=native).
const SYS_OPEN: usize = 0x01;
const SYS_CLOSE: usize = 0x02;
const SYS_WRITE: usize = 0x05;
const SYS_READ: usize = 0x06;
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
    let left = semihost(
        SYS_READ,
        &[handle, into.as_mut_ptr() as usize, into.len()],
    );
    assert_eq!(left, 0, "short read of an input file");
}

#[unsafe(no_mangle)]
unsafe extern "C" fn r86_t32_code_region(bytes: *mut usize) -> *mut u8 {
    // SAFETY: the caller passes a valid out pointer; abi.rs reserves the region.
    unsafe { *bytes = CODE_BYTES };
    CODE as *mut u8
}

#[unsafe(no_mangle)]
unsafe extern "C" fn r86_t32_code_publish(start: *const u8, bytes: usize) {
    // Armv8-M cache maintenance by address: clean the data cache line, then
    // invalidate the instruction cache line. A core without caches (QEMU's
    // model) ignores the writes.
    const DCCMVAC: *mut u32 = 0xe000_ef68 as *mut u32;
    const ICIMVAU: *mut u32 = 0xe000_ef58 as *mut u32;
    let end = start as usize + bytes;
    let mut line = start as usize & !31;
    while line < end {
        // SAFETY: system control registers of this core.
        unsafe {
            DCCMVAC.write_volatile(line as u32);
            ICIMVAU.write_volatile(line as u32);
        }
        line += 32;
    }
    // SAFETY: barriers have no operands.
    unsafe { asm!("dsb", "isb") };
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
    let word = |index: usize| {
        u32::from_le_bytes(config[4 * index..4 * index + 4].try_into().unwrap())
    };
    assert_eq!(word(0), MAGIC);
    assert_eq!(word(1), 1);
    assert_eq!(word(4), GUEST_BYTES as u32);
    let limit = u64::from(word(5)) | u64::from(word(6)) << 32;
    let jit = word(7) & WORKLOAD_JIT != 0;
    assert_eq!(word(7) & !WORKLOAD_JIT, 0, "only the plain boot is supported");
    // SAFETY: abi.rs reserves this region for guest RAM alone.
    let ram = unsafe { core::slice::from_raw_parts_mut(GUEST as *mut u8, GUEST_BYTES) };
    load(b"guest.ram\0", ram);
    let mut cpu = Cpu::new();
    cpu.enter_protected_flat(word(2), word(3));
    let mut machine = Machine::from_preloaded(cpu, ram);
    machine.set_jit_enabled(jit).unwrap();
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
    while machine.step_count() < limit {
        let result = machine.run_until_serial_output((limit - machine.step_count()).min(4096));
        let serial = machine.serial_output();
        emit(&serial[sent..]);
        sent = serial.len();
        if serial[observed..]
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
    let samples = SAMPLES.load(Relaxed) as usize;
    let file = semihost(SYS_OPEN, &[b"profile.bin\0".as_ptr() as usize, 5, 11]);
    semihost(SYS_WRITE, &[file, PROFILE, 4 * samples]);
    semihost(SYS_CLOSE, &[file]);
    let steps = machine.step_count();
    let instructions = spent * 2_000_000 / calibration.max(1);
    let hash = machine.ram().iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
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
