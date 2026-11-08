//! A single VM owns these bytes until the monitor stops. The boot mapping code
//! installs an RW/NX identity alias and an RO/RX alias shared by every ASID.
//! Entries use a read-only, non-executable part of the same alias. No guest RAM
//! mapping exposes this backing. Arena reset never reads a previous VM's code.

use super::CodeError;
use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

pub(crate) const CODE_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const STORAGE_BYTES: usize = CODE_BYTES + 64 * 1024 * 1024;
pub(crate) const EXECUTABLE_BASE: usize = 0x30_0000_0000;

#[repr(C, align(2097152))]
struct Storage(UnsafeCell<[u8; STORAGE_BYTES]>);
// The one claimed Mapping owns publication; native invocation excludes writes.
unsafe impl Sync for Storage {}
static STORAGE: Storage = Storage(UnsafeCell::new([0; STORAGE_BYTES]));
static READY: AtomicBool = AtomicBool::new(false);
static CLAIMED: AtomicBool = AtomicBool::new(false);

pub(crate) fn backing() -> *mut u8 {
    STORAGE.0.get().cast()
}

/// # Safety
/// Both aliases must have been installed in every monitor root, with the
/// permissions described above, before any arena can be constructed.
pub(crate) unsafe fn mappings_ready() {
    READY.store(true, Ordering::Release);
}

pub(super) struct Mapping {
    pub(super) base: NonNull<u8>,
    pub(super) writable: NonNull<u8>,
}

impl Mapping {
    pub(super) fn new(capacity: usize, entry_bytes: usize) -> Result<(Self, usize), CodeError> {
        if capacity == 0 || capacity > CODE_BYTES || entry_bytes > STORAGE_BYTES - CODE_BYTES {
            return Err(CodeError::Capacity);
        }
        if !READY.load(Ordering::Acquire) || CLAIMED.swap(true, Ordering::AcqRel) {
            return Err(CodeError::MonitorUnavailable);
        }
        Ok((
            Self {
                base: NonNull::new(EXECUTABLE_BASE as *mut u8).unwrap(),
                writable: NonNull::new(backing()).unwrap(),
            },
            CODE_BYTES,
        ))
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // The monitor retains the mappings; exclusive publication ownership can
        // be reacquired only after the old engine and all its links are gone.
        CLAIMED.store(false, Ordering::Release);
    }
}
