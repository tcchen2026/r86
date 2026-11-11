use super::abi::*;
#[cfg(not(feature = "r86chip"))]
use core::{
    alloc::{GlobalAlloc, Layout},
    cell::UnsafeCell,
};
use core::{
    arch::{asm, global_asm},
    fmt::Write,
    ptr::{read_volatile, write_volatile},
};
use r86::{machine::Machine, x86::Cpu};

#[cfg(feature = "r86chip")]
#[path = "machine_heap.rs"]
mod reusable_heap;

global_asm!(include_str!("machine_entry.S"));

// Single hart, no asynchronous allocator callers. One bounded VM lifetime;
// buffers are allocated from RAM, not proposed on-chip hardware storage.
#[cfg(not(feature = "r86chip"))]
struct Bump(UnsafeCell<usize>);
#[cfg(not(feature = "r86chip"))]
unsafe impl Sync for Bump {}
#[cfg(not(feature = "r86chip"))]
unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let used = unsafe { *self.0.get() };
        let Some(start) = (HEAP + used)
            .checked_add(layout.align() - 1)
            .map(|x| x & !(layout.align() - 1))
        else {
            return core::ptr::null_mut();
        };
        let Some(end) = start
            .checked_add(layout.size())
            .filter(|x| *x <= HEAP + HEAP_BYTES)
        else {
            return core::ptr::null_mut();
        };
        unsafe { *self.0.get() = end - HEAP };
        start as *mut u8
    }
    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {}
}
#[cfg(not(feature = "r86chip"))]
#[global_allocator]
static ALLOCATOR: Bump = Bump(UnsafeCell::new(0));

fn get(address: usize) -> u32 {
    unsafe { read_volatile(address as *const u32) }
}
fn put(address: usize, value: u32) {
    unsafe { write_volatile(address as *mut u32, value) }
}
fn put64(address: usize, value: u64) {
    put(address, value as u32);
    put(address + 4, (value >> 32) as u32);
}
struct Console;
impl Write for Console {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        emit(text.as_bytes());
        Ok(())
    }
}
fn emit(bytes: &[u8]) {
    let used = get(OUTPUT_LEN) as usize;
    let Some(end) = used.checked_add(bytes.len()).filter(|n| *n <= OUTPUT_BYTES) else {
        finish(7)
    };
    for (n, byte) in bytes.iter().enumerate() {
        unsafe { write_volatile((OUTPUT + used + n) as *mut u8, *byte) };
    }
    put(OUTPUT_LEN, end as u32);
}
fn histogram(machine: &Machine) {
    if get(WORKLOAD_FLAGS) & WORKLOAD_JIT_HISTOGRAM == 0 {
        return;
    }
    assert!(get(WORKLOAD_FLAGS) & WORKLOAD_BOUNDARIES != 0);
    put(BOUNDARIES, JIT_HIST_MAGIC);
    put(BOUNDARIES + 4, 2);
    put(BOUNDARIES + 8, JIT_HIST_BYTES as u32);
    #[cfg(feature = "r86chip")]
    if let Some(stats) = machine.chip_jit_stats() {
        for (i, value) in stats
            .leaves
            .iter()
            .chain(&stats.services)
            .chain(&stats.ends)
            .chain(&stats.unsupported)
            .chain(&stats.unsupported_f7)
            .chain(&stats.unsupported_0f)
            .enumerate()
        {
            put64(BOUNDARIES + 16 + i * 8, *value);
        }
    }
    put(BOUNDARY_LEN, JIT_HIST_BYTES as u32);
}

fn finish(status: u32) -> ! {
    put(STATUS, status);
    unsafe { asm!("ebreak", options(noreturn)) }
}
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let _ = writeln!(Console, "\nRV32 firmware panic: {info}");
    finish(5)
}

#[unsafe(no_mangle)]
extern "C" fn machine_main() -> ! {
    put(OUTPUT_LEN, 0);
    put(BOUNDARY_LEN, 0);
    put(STATUS, 1);
    emit(b"R86 RV32IM interpreter starting\n");
    assert_eq!(get(CONFIG), MAGIC);
    assert_eq!(get(CONFIG + 4), 1);
    assert_eq!(get(CONFIG + 16), GUEST_BYTES as u32);
    let limit = u64::from(get(CONFIG + 20)) | (u64::from(get(CONFIG + 24)) << 32);
    let marker = if get(WORKLOAD_FLAGS) & WORKLOAD_LUA != 0 {
        LUA_MARKER
    } else {
        MARKER
    };
    let mut cpu = Cpu::new();
    cpu.enter_protected_flat(get(CONFIG + 8), get(CONFIG + 12));
    // Simulation loader owns this disjoint region until execution stops. It is
    // guest data; the RV firmware, heap, stack and output live outside it.
    let ram = unsafe { core::slice::from_raw_parts_mut(GUEST as *mut u8, GUEST_BYTES) };
    let mut machine = Machine::from_preloaded(cpu, ram);
    #[cfg(feature = "r86chip")]
    machine
        .set_chip_jit_enabled(get(WORKLOAD_FLAGS) & WORKLOAD_JIT != 0)
        .unwrap();
    #[cfg(not(feature = "r86chip"))]
    assert_eq!(
        get(WORKLOAD_FLAGS) & WORKLOAD_JIT,
        0,
        "JIT not compiled into this firmware"
    );
    assert_eq!(
        get(WORKLOAD_FLAGS)
            & !(WORKLOAD_LUA | WORKLOAD_JIT | WORKLOAD_BOUNDARIES | WORKLOAD_JIT_HISTOGRAM),
        0,
        "unknown workload flags: rebuild inputs for the current firmware ABI"
    );
    put(CHIP_EXTENSIONS, u32::from(cfg!(feature = "r86chip-native")));
    #[cfg(feature = "r86chip-native")]
    unsafe {
        r86::chipinterp::native::configure_ram(GUEST as u32, (GUEST + GUEST_BYTES) as u32);
    }
    emit(b"R86 guest machine ready\n");
    put(STATUS, 2);
    let mut sent = 0;
    let mut observed = 0;
    let trace_boundaries =
        get(WORKLOAD_FLAGS) & (WORKLOAD_BOUNDARIES | WORKLOAD_JIT_HISTOGRAM) == WORKLOAD_BOUNDARIES;
    let mut boundary_bytes = 0;
    while machine.step_count() < limit {
        let result = machine.run_until_serial_output((limit - machine.step_count()).min(4096));
        put(STEPS_LO, machine.step_count() as u32);
        put(STEPS_HI, (machine.step_count() >> 32) as u32);
        put(EIP, machine.cpu().eip);
        for (i, value) in machine.cpu().regs.iter().enumerate() {
            put(CPU_REGS + 4 * i, *value);
        }
        put(CPU_FLAGS, machine.cpu().effective_eflags());
        put64(CPU_TSC, machine.cpu().tsc());
        #[cfg(feature = "r86chip")]
        {
            let counts = &machine.cpu().chip_counts;
            put64(X86_COMPLETED, counts.completed);
            put64(X86_FAULT_ATTEMPTS, counts.fault_attempts);
            put64(X86_REPEAT_CHUNKS, counts.repeat_chunks);
            put64(X86_HALTED_POLLS, counts.halted_polls);
        }
        if trace_boundaries {
            assert!(
                boundary_bytes + BOUNDARY_WORDS * 4 <= BOUNDARY_BYTES,
                "boundary trace full"
            );
            for word in boundary(machine.step_count(), machine.cpu()) {
                put(BOUNDARIES + boundary_bytes, word);
                boundary_bytes += 4;
            }
            put(BOUNDARY_LEN, boundary_bytes as u32);
        }
        #[cfg(feature = "r86chip")]
        if let Some(stats) = machine.chip_jit_stats() {
            put64(JIT_NATIVE, stats.native_steps);
            put64(JIT_INVOKES, stats.invocations);
            put64(JIT_BLOCKS, stats.compiled_blocks);
            put64(JIT_USER, stats.native_user_steps);
            put64(JIT_INVALIDATIONS, stats.invalidations);
        }
        let serial = machine.serial_output();
        emit(&serial[sent..]);
        sent = serial.len();
        if serial[observed..]
            .windows(marker.len())
            .any(|x| x == marker)
        {
            histogram(&machine);
            finish(3)
        }
        observed = sent.saturating_sub(marker.len() - 1);
        match result {
            Ok(report) if report.stop.is_none() => (),
            other => {
                let _ = writeln!(Console, "\nRV32 guest stop: {other:?}");
                finish(6)
            }
        }
    }
    histogram(&machine);
    finish(4)
}
