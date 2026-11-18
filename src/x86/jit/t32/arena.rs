//! Memory for Thumb-2 blocks and their records' source bytes: one writable,
//! executable region with bump allocation. The owner resets it only between
//! invocations, after discarding every block.
//!
//! Linux maps anonymous RWX memory and publishes with the ARM cacheflush
//! syscall. A bare-metal build (`target_os = "none"`) takes the region and the
//! publish step from two functions the platform must define; see `platform`.
//! That path is compile-checked only.

use alloc::string::String;

pub struct Arena {
    base: *mut u8,
    bytes: usize,
    used: usize,
}

impl Arena {
    /// At most `bytes` of code memory.
    pub fn new(bytes: usize) -> Result<Self, String> {
        let (base, bytes) = platform::map(bytes.next_multiple_of(4096))?;
        assert!(base as usize & 3 == 0);
        Ok(Self {
            base,
            bytes,
            used: 0,
        })
    }

    /// Discard everything after the first `keep` bytes.
    pub fn reset(&mut self, keep: usize) {
        self.used = keep;
    }

    pub fn used_bytes(&self) -> usize {
        self.used
    }

    /// Whether `code` halfwords and `data` bytes can both still be placed.
    pub fn fits(&self, code: usize, data: usize) -> bool {
        (code * 2).next_multiple_of(4) + data.next_multiple_of(4) <= self.bytes - self.used
    }

    /// Reserve `bytes`, rounded up to a multiple of four.
    fn take(&mut self, bytes: usize) -> Option<*mut u8> {
        let bytes = bytes.next_multiple_of(4);
        if bytes > self.bytes - self.used {
            return None;
        }
        // SAFETY: the range is inside this exclusively owned region.
        let start = unsafe { self.base.add(self.used) };
        self.used += bytes;
        Some(start)
    }

    /// Copy and publish code; returns its address (bit 0 clear) or None when
    /// full.
    pub fn publish(&mut self, code: &[u16]) -> Option<usize> {
        let bytes = code.len() * 2;
        let start = self.take(bytes)?;
        // SAFETY: `take` reserved the range, and no generated code runs while
        // the owner publishes. T32 code is a stream of little-endian halfwords.
        unsafe { core::ptr::copy_nonoverlapping(code.as_ptr().cast::<u8>(), start, bytes) };
        platform::publish(start as usize, bytes);
        Some(start as usize)
    }

    /// Copy data that is never executed; None when full.
    pub fn stash(&mut self, data: &[u8]) -> Option<*const u8> {
        let start = self.take(data.len())?;
        // SAFETY: `take` reserved the range.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), start, data.len()) };
        Some(start.cast_const())
    }
}

#[cfg(all(target_os = "linux", feature = "host"))]
mod platform {
    pub fn map(bytes: usize) -> Result<(*mut u8, usize), alloc::string::String> {
        // SAFETY: a new private anonymous mapping replaces no existing memory.
        let base = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(alloc::format!(
                "JIT code mapping: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok((base.cast(), bytes))
    }

    pub fn publish(start: usize, bytes: usize) {
        // ARM-private cacheflush(start, end, 0): clean D, invalidate I.
        const CACHEFLUSH: libc::c_long = 0x0f_0002;
        // SAFETY: the range lies inside our own mapping.
        let result = unsafe { libc::syscall(CACHEFLUSH, start, start + bytes, 0) };
        assert_eq!(result, 0, "ARM cacheflush failed");
    }
}

/// Bare metal: the platform port defines both functions.
#[cfg(target_os = "none")]
mod platform {
    unsafe extern "C" {
        /// A writable and executable region for translated code, aligned to
        /// four bytes and exclusively this engine's. Stores its size in bytes.
        fn r86_t32_code_region(bytes: *mut usize) -> *mut u8;
        /// Make `bytes` of newly written code at `start` visible to
        /// instruction fetch: clean the data cache and invalidate the
        /// instruction cache for the range, then synchronize (DSB, ISB).
        fn r86_t32_code_publish(start: *const u8, bytes: usize);
    }

    pub fn map(bytes: usize) -> Result<(*mut u8, usize), alloc::string::String> {
        let mut size = 0;
        // SAFETY: the platform contract above.
        let base = unsafe { r86_t32_code_region(&mut size) };
        let size = size.min(bytes) & !3;
        if base.is_null() || size < 64 * 1024 {
            return Err("JIT code region unavailable".into());
        }
        Ok((base, size))
    }

    pub fn publish(start: usize, bytes: usize) {
        // SAFETY: the range lies inside the region the platform provided.
        unsafe { r86_t32_code_publish(start as *const u8, bytes) };
    }
}

#[cfg(not(any(all(target_os = "linux", feature = "host"), target_os = "none")))]
compile_error!(
    "the Thumb-2 JIT needs code memory: Linux with the `host` feature, or a bare-metal platform"
);
