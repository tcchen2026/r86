//! Bounded reusable heap for the single-vCPU JIT monitor. Compilation drops
//! temporary vectors and retires page metadata throughout a boot; a bump-only
//! allocator would retain all of them. dlmalloc owns subdivision/coalescing of
//! one static segment. The monitor never requests host memory while running.
//!
//! Consulted dlmalloc 0.2.14's Allocator/malloc/free/realloc contracts. This is a
//! platform backing adapter, not a second implementation of its allocation logic.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::{Cell, UnsafeCell};
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, Ordering};
use dlmalloc::{Allocator, Dlmalloc};

const HEAP_BYTES: usize = 128 * 1024 * 1024;
#[repr(C, align(4096))]
struct Memory(UnsafeCell<[u8; HEAP_BYTES]>);
// Only the one allocator below can claim and subdivide this storage.
unsafe impl Sync for Memory {}
static MEMORY: Memory = Memory(UnsafeCell::new([0; HEAP_BYTES]));

struct Backing {
    claimed: Cell<bool>,
}

// SAFETY: this backing returns one live aligned segment exactly once. Every
// release/remap is declined, so no still-owned byte can be reused externally.
unsafe impl Allocator for Backing {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        if size > HEAP_BYTES || self.claimed.replace(true) {
            (null_mut(), 0, 0)
        } else {
            (MEMORY.0.get().cast(), HEAP_BYTES, 1) // EXTERN_BIT: static segment.
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
    busy: AtomicBool,
    allocator: UnsafeCell<Dlmalloc<Backing>>,
}
// The monitor has one vCPU and no allocating interrupt handler. The guard also
// rejects accidental allocator reentry instead of aliasing Dlmalloc's state.
unsafe impl Sync for Heap {}
impl Heap {
    fn with<T>(&self, f: impl FnOnce(&mut Dlmalloc<Backing>) -> T) -> T {
        assert!(
            !self.busy.swap(true, Ordering::Acquire),
            "monitor heap reentry"
        );
        struct Guard<'a>(&'a AtomicBool);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _guard = Guard(&self.busy);
        // SAFETY: the guard owns the allocator until this closure returns.
        f(unsafe { &mut *self.allocator.get() })
    }
}

// SAFETY: layouts and allocation ownership are forwarded unchanged to Dlmalloc;
// its state is exclusively borrowed and the backing outlives every allocation.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.with(|heap| unsafe { heap.malloc(layout.size(), layout.align()) })
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        self.with(|heap| unsafe { heap.calloc(layout.size(), layout.align()) })
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.with(|heap| unsafe { heap.free(ptr, layout.size(), layout.align()) });
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        self.with(|heap| unsafe { heap.realloc(ptr, layout.size(), layout.align(), size) })
    }
}

#[global_allocator]
static HEAP: Heap = Heap {
    busy: AtomicBool::new(false),
    allocator: UnsafeCell::new(Dlmalloc::new_with_allocator(Backing {
        claimed: Cell::new(false),
    })),
};
