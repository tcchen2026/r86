//! Native memory layout on the MPS3 AN524 model. The boot inputs have the
//! format of the r86chip machine firmware, whose host build writes them.
#[path = "../../r86chip/firmware/machine_abi.rs"]
mod chip;
pub use chip::{GUEST_BYTES, MAGIC, MARKER, WORKLOAD_JIT};

/// Config word 7: BIOS-booted PC (shared r86chip flags occupy 1, 2, 4 and 8).
pub const WORKLOAD_PC: u32 = 16;
/// Config word 7 bit 32: PC keyboard replay from semihosting `keys.txt`.
/// ASCII lines: `<step> <set-2 hex bytes>` or `<step> !shot <name>`; `#`
/// starts a comment. Steps are nondecreasing and at most the run limit.
/// Make/break events are separate, at least 2 M steps apart. Shot records
/// split execution but only the host saves intermediate frames. Input bounds:
/// 64 KiB text, 512 records, 16 bytes per key record. No shared ABI changes.
pub const WORKLOAD_PC_KEYS: u32 = 32;
pub const PC_KEYS_MAX_BYTES: usize = 65536;
pub const PC_KEYS_MAX_EVENTS: usize = 512;
pub const PC_MODE_WINDOWS: usize = 28; // Up to 700 M steps, 25 M per window.
pub const PC_GUEST_BYTES: usize = 16 << 20;
pub const PC_HEAP_BYTES: usize = 8 << 20;
pub const PC_CODE_BYTES: usize = 2 << 20;
pub const PC_STACK_BYTES: usize = 1 << 20;

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
