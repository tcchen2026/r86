//! Monitor VRAM pages can be mapped read-only into a hardware shadow window.
//! Keep alignment in the allocation's element type so cloning and deallocation
//! use the same layout. Reset moves this allocation without changing its base.
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};

#[repr(C, align(4096))]
#[derive(Clone, PartialEq)]
struct Page([u8; 4096]);

#[derive(Clone, Default, PartialEq)]
pub(super) struct Vram(Vec<Page>);

impl Vram {
    pub fn zeroed() -> Self {
        Self(alloc::vec![Page([0; 4096]); super::VRAM_PAGES])
    }
}

impl Deref for Vram {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // Page has no padding; the initialized array covers the allocation.
        unsafe { core::slice::from_raw_parts(self.0.as_ptr().cast(), self.0.len() * 4096) }
    }
}

impl DerefMut for Vram {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.0.as_mut_ptr().cast(), self.0.len() * 4096) }
    }
}

impl core::fmt::Debug for Vram {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Device diagnostics describe bytes, independently of backing layout.
        core::fmt::Debug::fmt(&**self, f)
    }
}
