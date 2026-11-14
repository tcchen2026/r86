//! i386 -> Thumb-2 translation for Armv7-M and Armv8-M Mainline cores
//! (Cortex-M33 class microcontrollers). Selected by the `jit-t32` feature on
//! 32-bit ARM targets, where it replaces the ARMv6K backend as
//! `crate::x86::jit::runtime`.
//!
//! The backend is self-contained: its own encoder ([`asm`]), lowering
//! ([`lower`]), block index ([`store`]), code memory ([`arena`]) and dispatch
//! ([`runtime`]). It shares x86 decoding, flag liveness and code-page validity
//! with the other JITs. It translates single-entry blocks of 32-bit
//! protected-mode integer code, including SoftMMU loads and stores through the
//! CPU's software TLB. Everything else, including every miss, fault, device
//! access and unsupported instruction, runs in the shared interpreter at a
//! precise boundary. Generated code assumes unaligned LDR/STR/LDRH/STRH
//! support (CCR.UNALIGN_TRP clear on M-profile cores).
//!
//! Validation: `asm` and `store` have host unit tests. `tests.rs` runs the same
//! x86 bytes through the interpreter and native blocks, and the `arm_jit_*`
//! Machine tests run whole programs; both execute under
//! `qemu-arm -cpu cortex-a7`, whose T32 is a superset of the encodings used
//! here. The assembler tests hold every emitter to the Armv8-M Mainline
//! encodings. No Cortex-M device has run this code; the bare-metal code memory
//! path is compile-checked only. Commands: design/VALIDATION.md.

#[cfg(all(feature = "machine", target_arch = "arm"))]
pub mod arena;
pub mod asm;
pub mod lower;
#[cfg(all(feature = "machine", target_arch = "arm"))]
pub mod runtime;
pub mod store;
#[cfg(all(test, target_arch = "arm"))]
mod tests;
