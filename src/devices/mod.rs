pub mod ne2000;
pub(crate) mod pic;
pub(crate) mod pit;
// Common PC device-clock units; the machine selects wall time or instruction time.
pub(crate) const TICKS_PER_SECOND: u64 = 16 * 1_193_182;
pub mod ps2;
pub mod vga;

pub use ps2::{I8042, Ps2ButtonState, Ps2Key};
pub use vga::{PcVga, RenderMode, TextSnapshot, VgaModeInfo, Viewer};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceEffect {
    IrqLevel { line: u8, level: bool },
    A20(bool),
    ResetRequest,
}
