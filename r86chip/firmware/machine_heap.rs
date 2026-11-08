//! Reusable single-hart heap for JIT vectors and invalidated compiled blocks.
//! This owns the same external RAM arena as the interpreter's bump allocator.
use crate::abi::{HEAP, HEAP_BYTES};
use core::{
    alloc::{GlobalAlloc, Layout},
    cell::{Cell, UnsafeCell},
    ptr::null_mut,
};
use dlmalloc::{Allocator, Dlmalloc};

struct Backing(Cell<bool>);
// The simulation memory map reserves this aligned arena exclusively for Rust.
// It is handed to dlmalloc once and cannot be released to another owner.
unsafe impl Allocator for Backing {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        if size > HEAP_BYTES || self.0.replace(true) {
            (null_mut(), 0, 0)
        } else {
            (HEAP as *mut u8, HEAP_BYTES, 1)
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
}
// There is one hart and no asynchronous native interrupt handler. No atomics
// or RISC-V A extension are needed; reentry is an explicit firmware bug.
unsafe impl Sync for Heap {}
impl Heap {
    fn with<T>(&self, f: impl FnOnce(&mut Dlmalloc<Backing>) -> T) -> T {
        assert!(!self.busy.replace(true), "RV32 heap reentry");
        struct Guard<'a>(&'a Cell<bool>);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _guard = Guard(&self.busy);
        f(unsafe { &mut *self.allocator.get() })
    }
}
// Ownership/layout are forwarded to dlmalloc within the exclusive allocator borrow.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.with(|h| unsafe { h.malloc(layout.size(), layout.align()) })
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.with(|h| unsafe { h.calloc(layout.size(), layout.align()) })
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.with(|h| unsafe { h.free(ptr, layout.size(), layout.align()) });
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        self.with(|h| unsafe { h.realloc(ptr, layout.size(), layout.align(), size) })
    }
}
#[global_allocator]
static ALLOCATOR: Heap = Heap {
    busy: Cell::new(false),
    allocator: UnsafeCell::new(Dlmalloc::new_with_allocator(Backing(Cell::new(false)))),
};
