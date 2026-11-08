#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::ptr::{read_volatile, write_volatile};

global_asm!(
    ".section .text.entry",
    ".global _start",
    "_start:",
    // RF storage has no reset. Define boot register contents before Rust's
    // prologue saves callee-saved registers; the ISA oracle rejects unknowns.
    ".irp reg, x1,x2,x3,x4,x5,x6,x7,x8,x9,x10,x11,x12,x13,x14,x15,x16,x17,x18,x19,x20,x21,x22,x23,x24,x25,x26,x27,x28,x29,x30,x31",
    "li \\reg, 0",
    ".endr",
    "lui sp, 0x1f",
    "call main",
    "ebreak",
    "1: j 1b",
);

// Standalone rustc test binary: no second Cargo package and no host services.
#[unsafe(no_mangle)]
extern "C" fn main() {
    let data = 0x1e000 as *mut u32;
    let mut sum = 0x0123_4567u32;
    for i in 0..40 {
        let a = unsafe { read_volatile(data.add(i)) };
        let b = unsafe { read_volatile(data.add(i + 40)) } | 1;
        sum = sum.rotate_left(3) ^ arithmetic(a, b);
        unsafe { write_volatile(data.add(i + 80), sum) };
    }
    unsafe { write_volatile(data.add(127), sum) };
}

#[inline(never)]
fn arithmetic(a: u32, b: u32) -> u32 {
    let product = (a as u64).wrapping_mul(b as u64);
    (product >> 32) as u32 ^ a.wrapping_mul(b) ^ a / b ^ a % b
        ^ (a as i32).wrapping_div(b as i32) as u32
        ^ (a as i32).wrapping_rem(b as i32) as u32
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop { unsafe { asm!("ebreak") }; }
}
