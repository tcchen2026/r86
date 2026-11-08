//! Export ordinary software libm entry points for the reference C x87 engine.
//! Built directly with rustc against the installed libm source; no chip ISA or
//! production emulator semantics are linked into the reference implementation.
#![no_std]
extern crate libm;

macro_rules! unary {
    ($($name:ident),*) => {$(
        #[no_mangle]
        pub extern "C" fn $name(x: f64) -> f64 { libm::$name(x) }
    )*};
}
unary!(sqrt, sin, cos, tan, log10, log2, exp, exp2, floor, ceil, trunc, round);
#[no_mangle]
pub extern "C" fn pow(x: f64, y: f64) -> f64 { libm::pow(x, y) }
#[no_mangle]
pub extern "C" fn atan2(y: f64, x: f64) -> f64 { libm::atan2(y, x) }
#[no_mangle]
pub extern "C" fn nearbyint(x: f64) -> f64 { libm::rint(x) }
#[no_mangle]
pub extern "C" fn nearbyintf(x: f32) -> f32 { libm::rintf(x) }
#[no_mangle]
pub extern "C" fn frexp(x: f64, exp: &mut i32) -> f64 {
    let (value, exponent) = libm::frexp(x);
    *exp = exponent;
    value
}
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    unsafe { core::arch::asm!("ebreak"); }
    loop { core::hint::spin_loop(); }
}
