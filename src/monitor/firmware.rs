//! Fixed firmware input and frame transport for the standalone EL1 observer.
//! All fields are little-endian u64 values; offsets are within trace backing,
//! never Rust pointers. The monitor owns output below INPUT_OFFSET. The VMM
//! keeps assets at/above INPUT_OFFSET immutable until the vCPU has stopped.
//! Guest RAM, firmware execution, IDE and VGA still belong to Machine.

pub const FRAME_OFFSET: usize = 128 * 1024 * 1024;
pub const INPUT_OFFSET: usize = 132 * 1024 * 1024;
pub const INPUT_MAGIC: u64 = 0x5238364649520001;
pub const FRAME_MAGIC: u64 = 0x52383646524d0001;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Inputs {
    pub magic: u64,
    pub iso_offset: u64,
    pub iso_len: u64,
    pub bios_offset: u64,
    pub bios_len: u64,
    pub vga_offset: u64,
    pub vga_len: u64,
    /// Host-time observation only; does not schedule guest devices or ticks.
    pub frame_ms: u64,
}

const _: () = assert!(core::mem::size_of::<Inputs>() == 8 * 8);
