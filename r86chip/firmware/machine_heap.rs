//! Reusable single-core heap for JIT vectors and invalidated compiled blocks.
//! This owns the same external RAM arena as the interpreter's bump allocator.
use crate::abi::{HEAP, HEAP_BYTES};
use core::{
    alloc::{GlobalAlloc, Layout},
    cell::{Cell, UnsafeCell},
    ptr::null_mut,
};
use dlmalloc::{Allocator, Dlmalloc};

struct Backing {
    allocated: Cell<bool>,
    bytes: usize,
}
// The simulation memory map reserves this aligned arena exclusively for Rust.
// It is handed to dlmalloc once and cannot be released to another owner.
unsafe impl Allocator for Backing {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        if size > self.bytes || self.allocated.replace(true) {
            (null_mut(), 0, 0)
        } else {
            (HEAP as *mut u8, self.bytes, 1)
        }
    }
    fn remap(&self, _: *mut u8, _: usize, _: usize, _: bool) -> *mut u8 {
        null_mut()
    }
    fn free_part(&self, _: *mut u8, _: usize, _: usize) -> bool {
        false
    }
    fn free(&self, _: *mut u8, _: usize) -> bool {
        false
    }
    fn can_release_part(&self, _: u32) -> bool {
        false
    }
    fn allocates_zeros(&self) -> bool {
        false
    }
    fn page_size(&self) -> usize {
        4096
    }
}
struct Heap {
    busy: Cell<bool>,
    allocator: UnsafeCell<Dlmalloc<Backing>>,
    high_water: Cell<usize>,
}
// One core; interrupt handlers must not allocate. No atomics or RISC-V A
// extension are needed; reentry is an explicit firmware bug.
unsafe impl Sync for Heap {}
impl Heap {
    fn with<T>(&self, f: impl FnOnce(&mut Dlmalloc<Backing>) -> T) -> T {
        assert!(!self.busy.replace(true), "firmware heap reentry");
        struct Guard<'a>(&'a Cell<bool>);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _guard = Guard(&self.busy);
        f(unsafe { &mut *self.allocator.get() })
    }

    fn record(&self, ptr: *mut u8, size: usize) -> *mut u8 {
        if !ptr.is_null() {
            self.high_water
                .set(self.high_water.get().max(ptr as usize - HEAP + size));
        }
        ptr
    }
}
// Ownership/layout are forwarded to dlmalloc within the exclusive allocator borrow.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.record(
            self.with(|h| unsafe { h.malloc(layout.size(), layout.align()) }),
            layout.size(),
        )
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.record(
            self.with(|h| unsafe { h.calloc(layout.size(), layout.align()) }),
            layout.size(),
        )
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.with(|h| unsafe { h.free(ptr, layout.size(), layout.align()) });
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        self.record(
            self.with(|h| unsafe { h.realloc(ptr, layout.size(), layout.align(), size) }),
            size,
        )
    }
}
#[global_allocator]
static ALLOCATOR: Heap = Heap {
    busy: Cell::new(false),
    allocator: UnsafeCell::new(Dlmalloc::new_with_allocator(Backing {
        allocated: Cell::new(false),
        bytes: HEAP_BYTES,
    })),
    high_water: Cell::new(0),
};

/// Restrict the backing before the first allocation; never grows a live heap.
#[allow(dead_code)]
pub fn limit_to(bytes: usize) {
    ALLOCATOR.with(|h| {
        let backing = h.allocator_mut();
        assert!(!backing.allocated.get(), "heap already in use");
        assert!(bytes >= 4096 && bytes <= HEAP_BYTES && bytes % 4096 == 0);
        backing.bytes = bytes;
    });
}

/// Highest allocated end relative to HEAP, including gaps below it. Allocator
/// bookkeeping beyond the final user block is not included; backing is capped.
#[allow(dead_code)]
pub fn high_water() -> usize {
    ALLOCATOR.high_water.get()
}
