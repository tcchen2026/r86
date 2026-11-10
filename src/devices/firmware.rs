//! i440FX PAM shadowing. Guest RAM backs the low shadow; ROM bytes never change
//! through guest stores. A20 and PCI overlay priority belong to the bus.

use alloc::{boxed::Box, vec::Vec};
use core::fmt;

const LOW: usize = 0xc0000;
const END: usize = 0x100000;

#[derive(Clone, Default)]
pub(crate) struct FirmwareShadow {
    pam: [u8; 7],
    generation: u64,
    rom: Box<[u8]>,
}

impl fmt::Debug for FirmwareShadow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FirmwareShadow")
            .field("pam", &self.pam)
            .field("enabled", &!self.rom.is_empty())
            .finish()
    }
}

impl FirmwareShadow {
    pub(crate) const HIGH_BASE: u64 = 0xffd0_0000;
    pub(crate) const HIGH_LEN: u64 = 0x30_0000;

    pub(crate) fn initialize(&mut self, ram: &[u8]) {
        self.rom = Vec::from(&ram[LOW..END]).into_boxed_slice();
        self.reset();
    }

    pub(crate) fn load_rom(&mut self, base: usize, bytes: &[u8]) {
        if !self.rom.is_empty() {
            self.rom[base - LOW..base - LOW + bytes.len()].copy_from_slice(bytes);
            self.bump();
        }
    }

    pub(crate) fn reset(&mut self) {
        self.pam.fill(0);
        self.bump();
    }

    fn bump(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("PAM generation exhausted");
    }

    pub(crate) fn mapping_generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn read_pam(&self, offset: u16) -> u8 {
        self.pam[usize::from(offset - 0x59)]
    }

    pub(crate) fn write_pam(&mut self, offset: u16, value: u8) {
        // Retain the cacheability bits, without modeling a separate CPU cache.
        // 86Box intel_4x0.c:708 uses these masks for the 440FX register bank.
        self.pam[usize::from(offset - 0x59)] = value & if offset == 0x59 { 0x70 } else { 0x77 };
        self.bump();
    }

    fn attributes(&self, addr: usize) -> u8 {
        if addr >= 0xf0000 {
            self.pam[0] >> 4
        } else {
            let block = (addr - LOW) >> 14;
            self.pam[1 + block / 2] >> ((block & 1) * 4)
        }
    }

    pub(crate) fn contains_memory(&self, addr: u64, len: u32) -> bool {
        if self.rom.is_empty() || len == 0 {
            return false;
        }
        let low = addr.wrapping_sub(LOW as u64);
        let high = addr.wrapping_sub(Self::HIGH_BASE);
        (low < (END - LOW) as u64 && u64::from(len) <= (END - LOW) as u64 - low)
            || (high < Self::HIGH_LEN && u64::from(len) <= Self::HIGH_LEN - high)
    }

    /// Only low shadow reads can supply ordinary RAM fetch/code-watch tokens.
    pub(crate) fn shadow_reads(&self, addr: u64, len: usize) -> bool {
        if self.rom.is_empty()
            || addr < LOW as u64
            || addr >= END as u64
            || len == 0
            || len > END - addr as usize
        {
            return false;
        }
        let end = addr as usize + len;
        let mut at = addr as usize;
        while at < end {
            if self.attributes(at) & 1 == 0 {
                return false;
            }
            at = ((at >> 14) + 1) << 14;
        }
        true
    }

    /// Read-only shadows may grant reads. PAM=2 must never grant writes:
    /// a shared writable cache token would also expose shadow bytes to reads.
    pub(crate) fn direct_page(&self, ram: &mut [u8], page: u64, write: bool) -> Option<*mut u8> {
        if page & 4095 != 0
            || !self.shadow_reads(page, 4096)
            || (write && self.attributes(page as usize) & 2 == 0)
        {
            return None;
        }
        Some(unsafe { ram.as_mut_ptr().add(page as usize) })
    }

    /// One physical bulk read, with no persistent grant to immutable ROM.
    pub(crate) fn read_range(&self, ram: &[u8], addr: u64, len: usize) -> Option<*const u8> {
        if !self.contains_memory(addr, u32::try_from(len).ok()?) {
            return None;
        }
        if self.shadow_reads(addr, len) {
            return Some(unsafe { ram.as_ptr().add(addr as usize) });
        }
        let index = (addr & 0xfffff) as usize;
        if index < LOW || len > END - index {
            return None;
        }
        if addr < END as u64 {
            let mut at = index;
            while at < index + len {
                if self.attributes(at) & 1 != 0 {
                    return None;
                }
                at = ((at >> 14) + 1) << 14;
            }
        }
        Some(unsafe { self.rom.as_ptr().add(index - LOW) })
    }

    #[inline]
    pub(crate) fn memory_read(&self, ram: &[u8], addr: u64, width: u8) -> Option<u64> {
        if !self.contains_memory(addr, u32::from(width)) {
            return None;
        }
        Some(self.read_value(ram, addr, width))
    }

    #[inline(never)]
    fn read_value(&self, ram: &[u8], addr: u64, width: u8) -> u64 {
        let mut bytes = [0; 8];
        for (i, byte) in bytes[..usize::from(width)].iter_mut().enumerate() {
            let at = addr + i as u64;
            *byte = if at < END as u64 && self.attributes(at as usize) & 1 != 0 {
                ram[at as usize]
            } else {
                let index = (at & 0xfffff).wrapping_sub(LOW as u64);
                self.rom.get(index as usize).copied().unwrap_or(0xff)
            };
        }
        u64::from_le_bytes(bytes)
    }

    /// Some(true) means shadow bytes changed and the bus must notify code watches.
    #[inline]
    pub(crate) fn memory_write(
        &self,
        ram: &mut [u8],
        addr: u64,
        width: u8,
        value: u64,
    ) -> Option<bool> {
        if !self.contains_memory(addr, u32::from(width)) {
            return None;
        }
        Some(self.write_value(ram, addr, width, value))
    }

    #[inline(never)]
    fn write_value(&self, ram: &mut [u8], addr: u64, width: u8, value: u64) -> bool {
        let mut written = false;
        if addr < END as u64 {
            for (i, byte) in value.to_le_bytes()[..usize::from(width)].iter().enumerate() {
                let at = addr as usize + i;
                if self.attributes(at) & 2 != 0 {
                    ram[at] = *byte;
                    written = true;
                }
            }
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn firmware_pam_widths_and_split_attributes_preserve_rom() {
        let mut ram = vec![0x5a; END];
        let mut fw = FirmwareShadow::default();
        fw.initialize(&ram);
        for state in 0..4 {
            fw.write_pam(0x59, state << 4);
            fw.write_pam(0x5a, state | (state << 4));
            fw.write_pam(0x5f, state | (state << 4));
            for addr in [0xc0001, 0xc3ffe, 0xe9910, 0xefffc, 0xffff8] {
                for width in [1, 2, 4, 8] {
                    ram[addr..addr + width].fill(0xa5);
                    let before = fw.memory_read(&ram, addr as u64, width as u8).unwrap();
                    let expected = if state & 1 != 0 { 0xa5 } else { 0x5a };
                    assert_eq!(&before.to_le_bytes()[..width], vec![expected; width]);
                    assert_eq!(
                        fw.memory_write(&mut ram, addr as u64, width as u8, 0x8877665544332211),
                        Some(state & 2 != 0)
                    );
                    let expected = if state & 2 != 0 {
                        0x8877665544332211u64.to_le_bytes()[..width].to_vec()
                    } else {
                        vec![0xa5; width]
                    };
                    assert_eq!(&ram[addr..addr + width], expected);
                    let high = 0xfff00000 + addr as u64;
                    let rom = fw.memory_read(&ram, high, width as u8).unwrap();
                    assert_eq!(&rom.to_le_bytes()[..width], vec![0x5a; width]);
                    assert_eq!(fw.memory_write(&mut ram, high, width as u8, 0), Some(false));
                    assert_eq!(&ram[addr..addr + width], expected);
                }
            }
        }
        // Only the first byte is writable across the PAM1 nibble boundary.
        fw.write_pam(0x5a, 0x13);
        ram[0xc3fff..0xc4003].fill(0x90);
        fw.memory_write(&mut ram, 0xc3fff, 4, 0x44332211);
        assert_eq!(&ram[0xc3fff..0xc4003], &[0x11, 0x90, 0x90, 0x90]);
        assert_eq!(fw.memory_read(&ram, 0xc3fff, 4), Some(0x90909011));
    }

    #[test]
    fn firmware_pam_grants_require_the_selected_read_backing() {
        let mut ram = vec![0x42; END];
        let mut fw = FirmwareShadow::default();
        fw.initialize(&ram);
        for state in 0..4 {
            let before = fw.mapping_generation();
            fw.write_pam(0x5f, state);
            fw.write_pam(0x5f, state);
            assert_eq!(fw.mapping_generation(), before + 2);
            assert_eq!(
                fw.direct_page(&mut ram, 0xe9000, false).is_some(),
                state & 1 != 0
            );
            assert_eq!(
                fw.direct_page(&mut ram, 0xe9000, true).is_some(),
                state == 3
            );
            assert!(fw.direct_page(&mut ram, 0xec000, true).is_none());
            assert!(fw.direct_page(&mut ram, 0xfffe9000, true).is_none());
            assert!(fw.direct_page(&mut ram, 0xe9001, false).is_none());
        }
        let ptr = fw.direct_page(&mut ram, 0xe9000, true).unwrap();
        assert_eq!(ptr, unsafe { ram.as_mut_ptr().add(0xe9000) });
        let before = fw.mapping_generation();
        fw.reset();
        assert!(fw.mapping_generation() > before);
        assert!(fw.direct_page(&mut ram, 0xe9000, true).is_none());
        assert_eq!(fw.memory_read(&ram, 0xfffe9000, 1), Some(0x42));
    }

    #[test]
    fn firmware_pam_direct_access_matches_byte_dispatch() {
        let mut direct = vec![0x5a; END];
        let mut bytes = direct.clone();
        let mut fw = FirmwareShadow::default();
        fw.initialize(&direct);
        let mut rng = 0x98abcdefu32;
        let mut bulk_grants = 0;
        for step in 0..512 {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            let register = 0x59 + (step % 7) as u16;
            fw.write_pam(register, (rng >> 24) as u8);
            if step % 61 == 0 {
                fw.reset();
            }
            let addr = LOW + ((rng as usize >> 10) & 63) * 4096 + (rng as usize & 0xff0);
            let width = [1, 2, 4, 8][step % 4];
            let value = u64::from(rng) | u64::from(!rng) << 32;
            let page = (addr & !4095) as u64;
            if let Some(ptr) = fw.direct_page(&mut direct, page, true) {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        value.to_le_bytes().as_ptr(),
                        ptr.add(addr & 4095),
                        width,
                    );
                }
            } else {
                fw.memory_write(&mut direct, addr as u64, width as u8, value);
            }
            for (i, byte) in value.to_le_bytes()[..width].iter().enumerate() {
                fw.memory_write(&mut bytes, (addr + i) as u64, 1, u64::from(*byte));
            }
            let mut read = [0; 8];
            if let Some(ptr) = fw.direct_page(&mut direct, page, false) {
                unsafe {
                    core::ptr::copy_nonoverlapping(ptr.add(addr & 4095), read.as_mut_ptr(), width);
                }
            } else {
                read = fw
                    .memory_read(&direct, addr as u64, width as u8)
                    .unwrap()
                    .to_le_bytes();
            }
            for i in 0..width {
                assert_eq!(
                    read[i],
                    fw.memory_read(&bytes, (addr + i) as u64, 1).unwrap() as u8
                );
            }
            if step % 8 == 0 {
                // The same page-bounded fill/copy used by cached REP, compared
                // with ordered byte dispatch even when read/write backing differs.
                let offset = addr & 4095;
                let dst = page as usize + (offset ^ 0x800);
                let write_page = fw.direct_page(&mut direct, page, true);
                if let Some(ptr) = write_page {
                    unsafe { core::ptr::write_bytes(ptr.add(offset), rng as u8, 16) };
                    bulk_grants += 1;
                } else {
                    for i in 0..16 {
                        fw.memory_write(&mut direct, (addr + i) as u64, 1, u64::from(rng as u8));
                    }
                }
                for i in 0..16 {
                    fw.memory_write(&mut bytes, (addr + i) as u64, 1, u64::from(rng as u8));
                }
                if let (Some(read), Some(write)) =
                    (fw.direct_page(&mut direct, page, false), write_page)
                {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            read.add(offset),
                            write.add(offset ^ 0x800),
                            16,
                        );
                    }
                } else {
                    for i in 0..16 {
                        let value = fw.memory_read(&direct, (addr + i) as u64, 1).unwrap();
                        fw.memory_write(&mut direct, (dst + i) as u64, 1, value);
                    }
                }
                for i in 0..16 {
                    let value = fw.memory_read(&bytes, (addr + i) as u64, 1).unwrap();
                    fw.memory_write(&mut bytes, (dst + i) as u64, 1, value);
                }
            }
            assert_eq!(direct, bytes);
        }
        assert!(bulk_grants > 0);
    }
}
