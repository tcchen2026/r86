//! Bare-metal r86 machine for a Cortex-M33: QEMU's MPS3 AN524 board.
//!
//! The host build of `r86chip-machine` prepares a direct-kernel TinyCore boot
//! (`guest.ram`, `config.bin`). This firmware loads both through semihosting,
//! runs the shared interpreter or the Thumb-2 JIT (a flag in `config.bin`
//! selects), and forwards the guest's serial output. `run.sh` drives it.
//! With `mcu`, config word 7 can instead select a BIOS-booted PC with 16 MiB
//! guest RAM and a frozen semihosting disk. `pc.sh` drives that bounded mode.
#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

#[cfg(target_os = "none")]
mod abi;
#[cfg(target_os = "none")]
#[path = "../../r86chip/firmware/machine_heap.rs"]
mod heap;
#[cfg(target_os = "none")]
mod runtime;

#[cfg(not(target_os = "none"))]
fn main() {
    eprintln!("r86-cortex-m is bare-metal firmware; see cortex-m/firmware/run.sh");
    std::process::exit(1);
}
