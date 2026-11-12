//! Flat physical RAM for the native instruction interpreter.
//!
//! The core validates instruction/data alignment and access bounds before
//! performing native effects. This backing contains no x86 segment descriptors,
//! code-page versions or staged guest stores. Core owns the guest TLB/window;
//! firmware supplies qualified mappings and x86 policy. Ordinary RV memory
//! instructions do not use that guest lookup path.

use super::cache_estimate::Estimate;
use std::cell::RefCell;

pub struct Memory {
    /// Zero-filled host backing for repeatable image loading. This describes
    /// simulation setup, not a requirement to reset all physical RAM in hardware.
    pub ram: Vec<u8>,
    pub cache_estimate: Option<RefCell<Estimate>>,
}
impl Memory {
    pub fn new(size: usize) -> Self {
        Self {
            ram: vec![0; size],
            cache_estimate: None,
        }
    }

    pub fn observe_fetch(&self, pc: u32) {
        if let Some(e) = &self.cache_estimate {
            e.borrow_mut().fetch(pc);
        }
    }
    pub fn observe_operands(&self) {
        if let Some(e) = &self.cache_estimate {
            e.borrow_mut().operands();
        }
    }
    pub fn observe_fence_i(&self) {
        if let Some(e) = &self.cache_estimate {
            e.borrow_mut().fence_i();
        }
    }

    /// Copy bytes for image loading or a validated native store. Reject an
    /// overflowing/out-of-RAM range before changing any byte. Native access
    /// faults are selected by Core before calling this host boundary.
    ///
    /// Writes never update the core's fetched-word table. After changing code,
    /// firmware must execute FENCE.I to guarantee publication to later fetches.
    pub fn write(&mut self, addr: u32, bytes: &[u8]) -> Result<(), String> {
        let start = addr as usize;
        let end = start.checked_add(bytes.len()).ok_or("write overflow")?;
        let destination = self
            .ram
            .get_mut(start..end)
            .ok_or("write outside native RAM")?;
        if let Some(e) = &mut self.cache_estimate {
            e.get_mut().data(addr, bytes.len());
        }
        destination.copy_from_slice(bytes);
        Ok(())
    }

    /// Read a validated 8/16/32-bit quantity in little-endian order and zero
    /// extend to u32. LB/LH sign extension belongs to the instruction decoder.
    /// No device effects, address translation or additional guest checks occur.
    pub fn read(&self, addr: u32, bits: u8) -> u32 {
        if let Some(e) = &self.cache_estimate {
            e.borrow_mut().data(addr, usize::from(bits / 8));
        }
        let mut value = 0;
        for i in 0..bits / 8 {
            value |= u32::from(self.ram[addr as usize + i as usize]) << (i * 8);
        }
        value
    }
}
