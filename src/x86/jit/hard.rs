//! EL1 generated-memory fault boundary. Exact registered translation faults
//! can refill already-A/D ordinary RAM mappings and retry the faulting access.
//! The shared probe performs no guest memory, device or architectural effects
//! while x86 registers/flags/time remain native. Missing proof and permission
//! faults use the existing precise exit; the interpreter performs A/D updates
//! or delivers faults after state commit. No MMIO read is replayed.
//!
//! The Rust refill call has a complete exception ABI: caller-clobbered integer
//! and all SIMD registers, FPCR/FPSR, ELR/SPSR are saved explicitly. AAPCS64
//! preserves X19..X29. Ordinary Rust accesses still use no recoverable boundary.
//! Required write intent comes from FaultSite (including an RMW load), never
//! from ESR.WnR alone. Watched code cannot acquire a writable native alias.
//! AF, external and unrecorded native faults remain emulator bugs.

use super::{code::monitor, frame::Frame, memory::NativeFault};
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicUsize, Ordering};

#[unsafe(no_mangle)]
static R86_JIT_ACTIVE_FRAME: AtomicUsize = AtomicUsize::new(0);

pub(super) struct Invocation;

impl Invocation {
    pub(super) fn enter(frame: &mut Frame) -> Self {
        assert_eq!(
            R86_JIT_ACTIVE_FRAME.swap(frame as *mut Frame as usize, Ordering::Release),
            0
        );
        Self
    }
}

impl Drop for Invocation {
    fn drop(&mut self) {
        R86_JIT_ACTIVE_FRAME.store(0, Ordering::Release);
    }
}

/// SAFETY: the vector saved the complete calling context. The invocation is
/// the sole owner of Frame/View/Cpu/HostBus; only paging controls may be read
/// from Cpu because GPRs/flags/time have not been committed from native state.
#[unsafe(no_mangle)]
unsafe extern "C" fn __r86_jit_refill(frame: *mut Frame, linear: u32, write: u32) -> u32 {
    let frame = unsafe { &mut *frame };
    let view = unsafe { &mut *frame.memory.cast::<super::soft::View>() };
    let cpu = unsafe { &*view.cpu };
    let bus = unsafe { &mut *view.bus.cast::<crate::machine::HostBus>() };
    let repaired = bus.jit_refill_hard(cpu, crate::x86::LinearAddr(linear), write != 0);
    frame.hard_refills += u64::from(repaired);
    u32::from(repaired)
}

const _: () = assert!(size_of::<NativeFault>() == 16);
const _: () = assert!(offset_of!(NativeFault, pc) == 0);
const _: () = assert!(offset_of!(NativeFault, recovery) == 8);
const _: () = assert!(monitor::EXECUTABLE_BASE == 0x30_0000_0000);
const _: () = assert!(monitor::CODE_BYTES == 1 << 27);

core::arch::global_asm!(
    r#"
    .section .text.r86_jit_fault, "ax"
    .global __r86_sync_current_el
__r86_sync_current_el:
    // Preserve state before classifying the PC: X8 and all guest/flag registers
    // may be live. Non-JIT faults retain the original leaf thunk's clobber ABI.
    stp x0, x1, [sp, #-16]!
    mrs x0, elr_el1
    mov x1, #0x3000000000
    sub x1, x0, x1
    lsr x1, x1, #27
    cbz x1, .Ljit_abort
    ldp x0, x1, [sp], #16
    b __r86_sync_hard_read

.Ljit_abort:
    sub sp, sp, #48
    stp x2, x3, [sp]
    stp x4, x5, [sp, #16]
    stp x6, x7, [sp, #32]
    adrp x1, R86_JIT_ACTIVE_FRAME
    ldr x1, [x1, :lo12:R86_JIT_ACTIVE_FRAME]
    cbz x1, __r86_unexpected_exception
    cmp x1, x28
    b.ne __r86_unexpected_exception

    // Current-EL data abort, no cache maintenance/external/invalid FAR.
    mrs x2, esr_el1
    mov w3, #0x0700
    movk w3, #0xfc00, lsl #16
    and w3, w2, w3
    mov w4, #0x94000000
    cmp w3, w4
    b.ne __r86_unexpected_exception
    ubfx x3, x2, #2, #4
    cmp x3, #1
    b.eq .Ljit_fault_kind_ok
    cmp x3, #3
    b.ne __r86_unexpected_exception
    // A write can encounter a read-only shadow before its x86 write walk.
    // A native read permission fault or access-flag fault is still a bug.
    tbz x2, #6, __r86_unexpected_exception
.Ljit_fault_kind_ok:
    ubfx x3, x2, #6, #1
    str w3, [x28, #{write}]
    mrs x2, far_el1
    ldr x3, [x28, #{read_base}]
    cbz x3, __r86_unexpected_exception
    lsr x4, x2, #32
    lsr x3, x3, #32
    cmp x4, x3
    b.ne __r86_unexpected_exception

    // Search only the immutable table belonging to this active arena. Old
    // bodies remain recorded until clear; code bytes are never reused earlier.
    ldr x1, [x28, #{faults}]
    ldr x3, [x28, #{count}]
    mov x2, #0
.Ljit_find:
    cmp x2, x3
    b.hs __r86_unexpected_exception
    add x4, x2, x3
    lsr x4, x4, #1
    add x7, x1, x4, lsl #4
    ldp x5, x6, [x7]
    cmp x0, x5
    b.eq .Ljit_found
    b.lo .Ljit_lower
    add x2, x4, #1
    b .Ljit_find
.Ljit_lower:
    mov x3, x4
    b .Ljit_find
.Ljit_found:
    // Permission faults keep the precise exit. An RMW may already have read
    // through this RO mapping: never mix that value with a newly remapped PA.
    mrs x2, esr_el1
    ubfx x3, x2, #2, #4
    cmp x3, #1
    b.ne .Ljit_cold
    sub sp, sp, #656
    stp x8, x9, [sp]
    stp x10, x11, [sp, #16]
    stp x12, x13, [sp, #32]
    stp x14, x15, [sp, #48]
    stp x16, x17, [sp, #64]
    stp x18, x30, [sp, #80]
    stp x0, x6, [sp, #96]
    mrs x3, spsr_el1
    mrs x4, fpcr
    mrs x5, fpsr
    stp x3, x4, [sp, #112]
    str x5, [sp, #128]
    stp q0, q1, [sp, #144]
    stp q2, q3, [sp, #176]
    stp q4, q5, [sp, #208]
    stp q6, q7, [sp, #240]
    stp q8, q9, [sp, #272]
    stp q10, q11, [sp, #304]
    stp q12, q13, [sp, #336]
    stp q14, q15, [sp, #368]
    stp q16, q17, [sp, #400]
    stp q18, q19, [sp, #432]
    stp q20, q21, [sp, #464]
    stp q22, q23, [sp, #496]
    stp q24, q25, [sp, #528]
    stp q26, q27, [sp, #560]
    stp q28, q29, [sp, #592]
    stp q30, q31, [sp, #624]
    and w2, w6, #1
    mov x0, x28
    mrs x1, far_el1
    bl __r86_jit_refill
    // Select the unchanged native PC on success, otherwise its precise exit.
    ldp x2, x3, [sp, #96]
    bic x3, x3, #1
    cmp w0, #0
    csel x2, x2, x3, ne
    msr elr_el1, x2
    cbnz w0, .Ljit_refilled
    mrs x2, far_el1
    str x2, [x28, #{address}]
.Ljit_refilled:
    ldp q0, q1, [sp, #144]
    ldp q2, q3, [sp, #176]
    ldp q4, q5, [sp, #208]
    ldp q6, q7, [sp, #240]
    ldp q8, q9, [sp, #272]
    ldp q10, q11, [sp, #304]
    ldp q12, q13, [sp, #336]
    ldp q14, q15, [sp, #368]
    ldp q16, q17, [sp, #400]
    ldp q18, q19, [sp, #432]
    ldp q20, q21, [sp, #464]
    ldp q22, q23, [sp, #496]
    ldp q24, q25, [sp, #528]
    ldp q26, q27, [sp, #560]
    ldp q28, q29, [sp, #592]
    ldp q30, q31, [sp, #624]
    ldp x3, x4, [sp, #112]
    ldr x5, [sp, #128]
    msr spsr_el1, x3
    msr fpcr, x4
    msr fpsr, x5
    ldp x8, x9, [sp]
    ldp x10, x11, [sp, #16]
    ldp x12, x13, [sp, #32]
    ldp x14, x15, [sp, #48]
    ldp x16, x17, [sp, #64]
    ldp x18, x30, [sp, #80]
    add sp, sp, #656
    b .Ljit_restore
.Ljit_cold:
    bic x6, x6, #1
    msr elr_el1, x6
    mrs x2, far_el1
    str x2, [x28, #{address}]
.Ljit_restore:
    ldp x2, x3, [sp]
    ldp x4, x5, [sp, #16]
    ldp x6, x7, [sp, #32]
    add sp, sp, #48
    ldp x0, x1, [sp], #16
    // SPSR contains the original faulting NZCV/PSTATE. Every native register
    // and stack slot is intact; retry or precise exit owns instruction commit.
    eret
"#,
    read_base = const offset_of!(Frame, hard_read_base),
    faults = const offset_of!(Frame, hard_faults),
    count = const offset_of!(Frame, hard_fault_count),
    address = const offset_of!(Frame, hard_fault_address),
    write = const offset_of!(Frame, hard_fault_write),
);
