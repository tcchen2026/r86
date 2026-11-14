//! Native memory layout on the MPS3 AN524 model. The boot inputs have the
//! format of the r86chip machine firmware, whose host build writes them.
#[path = "../../r86chip/firmware/machine_abi.rs"]
mod chip;
pub use chip::{GUEST_BYTES, MAGIC, MARKER, WORKLOAD_JIT};

// DDR: 0x6000_0000, 2 GiB, executable. `qemu.ld` puts the program and its
// stack in the first 32 MiB.
pub const HEAP: usize = 0x6200_0000;
pub const HEAP_BYTES: usize = 64 << 20;
/// Translated code, handed out by `r86_t32_code_region`.
pub const CODE: usize = 0x6600_0000;
pub const CODE_BYTES: usize = 8 << 20;
/// Guest RAM, `GUEST_BYTES` long.
pub const GUEST: usize = 0x6800_0000;
/// Sampled program counters, one word each (`profile.bin`).
pub const PROFILE: usize = 0x7000_0000;
pub const PROFILE_BYTES: usize = 16 << 20;
