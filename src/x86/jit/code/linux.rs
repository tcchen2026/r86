//! Linux ownership of the anonymous RW/NX and RO/RX memfd aliases.
use super::CodeError;
use core::ptr::NonNull;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub(super) struct Mapping {
    pub(super) base: NonNull<u8>,
    pub(super) writable: NonNull<u8>,
    mapped: usize,
}

impl Mapping {
    pub(super) fn new(capacity: usize, entry_bytes: usize) -> Result<(Self, usize), CodeError> {
        // SAFETY: sysconf has no pointer arguments or process-state mutation.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 || !(page_size as usize).is_power_of_two() || capacity == 0 {
            return Err(CodeError::Capacity);
        }
        let page_size = page_size as usize;
        let entries_start = capacity
            .checked_add(page_size - 1)
            .ok_or(CodeError::Capacity)?
            & !(page_size - 1);
        // Native entries are signed displacements from their own dense table.
        // Keep the existing code and metadata limits, in one nearby mapping.
        let mapped = entries_start
            .checked_add(entry_bytes)
            .ok_or(CodeError::Capacity)?;
        if mapped > i32::MAX as usize {
            return Err(CodeError::Capacity);
        }
        // SAFETY: a fixed NUL-terminated name and supported creation flags.
        let fd = unsafe { libc::memfd_create(c"r86-jit".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(CodeError::Os(io::Error::last_os_error()));
        }
        // SAFETY: this fresh descriptor has exactly one owner, including every
        // error path below. The mappings retain the file after the fd is closed.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if unsafe { libc::ftruncate(fd.as_raw_fd(), mapped as libc::off_t) } != 0 {
            return Err(CodeError::Os(io::Error::last_os_error()));
        }
        // SAFETY: new shared mappings of this exclusively owned, sized backing;
        // no existing mapping is replaced, and neither mapping is both W and X.
        let writable = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                mapped,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if writable == libc::MAP_FAILED {
            return Err(CodeError::Os(io::Error::last_os_error()));
        }
        let base = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                mapped,
                libc::PROT_READ | libc::PROT_EXEC,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            let error = io::Error::last_os_error();
            // SAFETY: the first mapping succeeded and has no published users.
            unsafe {
                libc::munmap(writable, mapped);
            }
            return Err(CodeError::Os(error));
        }
        Ok((
            Self {
                base: NonNull::new(base.cast())
                    .expect("JIT execution mapping returned address zero"),
                writable: NonNull::new(writable.cast())
                    .expect("JIT writer mapping returned address zero"),
                mapped,
            },
            entries_start,
        ))
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: exclusive mapping owner, with no invocation or live borrow.
        unsafe {
            libc::munmap(self.base.as_ptr().cast(), self.mapped);
            libc::munmap(self.writable.as_ptr().cast(), self.mapped);
        }
    }
}
