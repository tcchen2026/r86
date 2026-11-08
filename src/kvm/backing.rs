use std::ptr::NonNull;
use std::slice;

use crate::monitor::abi::PAGE_SIZE_4K;

use crate::kvm::error::{KvmError, Result};
use crate::kvm::sys;

#[derive(Debug)]
pub struct MappedMemory {
    ptr: NonNull<u8>,
    len: usize,
}

impl MappedMemory {
    pub fn new_zeroed(len: usize) -> Result<Self> {
        if len == 0 {
            return Err(KvmError::MemslotPlan("zero-length backing mapping"));
        }
        if len as u64 & (PAGE_SIZE_4K - 1) != 0 {
            return Err(KvmError::MemslotPlan("backing length is not page aligned"));
        }
        let ptr = unsafe { sys::mmap_anonymous_rw(len)? };
        Ok(Self { ptr, len })
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub fn userspace_addr(&self) -> u64 {
        self.as_ptr() as usize as u64
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    pub fn fill(&mut self, value: u8) {
        self.as_mut_slice().fill(value);
    }
}

impl Drop for MappedMemory {
    fn drop(&mut self) {
        let _ = unsafe { sys::munmap_region(self.ptr, self.len) };
    }
}

#[derive(Debug)]
pub struct SlotBacking {
    pub slot: u32,
    pub memory: MappedMemory,
}

impl SlotBacking {
    pub fn new(slot: u32, len: u64) -> Result<Self> {
        let len = usize::try_from(len).map_err(|_| KvmError::MemslotPlan("slot too large"))?;
        Ok(Self {
            slot,
            memory: MappedMemory::new_zeroed(len)?,
        })
    }

    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let offset =
            usize::try_from(offset).map_err(|_| KvmError::MemslotPlan("slot write offset"))?;
        let end = offset
            .checked_add(data.len())
            .ok_or(KvmError::MemslotPlan("slot write end overflows"))?;
        if end > self.memory.len() {
            return Err(KvmError::MemslotPlan("slot write exceeds backing"));
        }
        self.memory.as_mut_slice()[offset..end].copy_from_slice(data);
        Ok(())
    }

    pub fn flush_instruction_cache_at(&mut self, offset: u64, len: usize) -> Result<()> {
        let offset =
            usize::try_from(offset).map_err(|_| KvmError::MemslotPlan("cache flush offset"))?;
        let end = offset
            .checked_add(len)
            .ok_or(KvmError::MemslotPlan("cache flush end overflows"))?;
        if end > self.memory.len() {
            return Err(KvmError::MemslotPlan("cache flush exceeds backing"));
        }
        let ptr = unsafe { self.memory.as_ptr().add(offset) };
        sys::clear_instruction_cache(ptr, len);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_backing_is_page_aligned_and_zeroed() {
        let memory = MappedMemory::new_zeroed(PAGE_SIZE_4K as usize * 2).unwrap();
        assert_eq!(memory.userspace_addr() & (PAGE_SIZE_4K - 1), 0);
        assert!(memory.as_slice().iter().all(|byte| *byte == 0));
    }

    #[test]
    fn rejects_unaligned_backing() {
        assert!(MappedMemory::new_zeroed(PAGE_SIZE_4K as usize + 1).is_err());
    }
}
