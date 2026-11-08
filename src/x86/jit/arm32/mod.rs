//! i386 -> ARMv6K translation for the ARM11 MPCore (Nintendo 3DS).
//!
//! The backend shares x86 decoding, flag liveness and code-page validity with
//! the AArch64 JIT; instruction selection, register assignment and the frame
//! ABI are its own. It translates single-entry blocks of 32-bit protected-mode
//! integer code, including SoftMMU loads and stores through the CPU's software
//! TLB. Everything else, including every miss, fault, device access and
//! unsupported instruction, runs in the shared interpreter at a precise
//! boundary. See [`compile`] for the lowering contract and [`runtime`] for
//! dispatch and validity. ARM code assumes ARMv6 unaligned LDR/STR/LDRH/STRH
//! support (SCTLR.U), as on Linux and Horizon.
//!
//! Validation: differential tests in `tests.rs` run the same x86 bytes through
//! the interpreter and native blocks under `qemu-arm -cpu arm11mpcore`.

pub mod asm;
#[cfg(feature = "machine")]
pub mod code;
pub mod compile;
#[cfg(feature = "machine")]
pub mod runtime;
#[cfg(test)]
mod tests;
