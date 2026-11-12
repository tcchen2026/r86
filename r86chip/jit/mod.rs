//! Experimental x86-to-B1 code generation, isolated from the ordinary engine.
//! Native execution uses the existing RV32 instructions in register bank 1.
#[cfg(all(feature = "machine", target_arch = "riscv32"))]
mod cache;
pub mod compile;
#[cfg(all(feature = "machine", target_arch = "riscv32"))]
pub mod runtime;

#[cfg(all(feature = "machine", target_arch = "riscv32"))]
mod native;
