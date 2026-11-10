//! Optional PIIX3 UHCI with one full-speed absolute HID pointer.
//! Contracts, limits and provenance: README.txt in this directory.
mod tablet;
mod uhci;
pub(crate) use uhci::Uhci;
pub const TABLET_MAX: u16 = 0x7fff;
pub(crate) const IRQ: u8 = 11;
#[cfg(test)]
mod tests;
