//! Executable memory for ARMv6K blocks: one RWX region with bump allocation.
//! The owner resets it only between invocations, after discarding every block.
//!
//! Linux maps anonymous RWX memory and publishes with the ARM cacheflush
//! syscall. Horizon (3DS) changes a page-aligned heap allocation to RWX with
//! svcControlProcessMemory, cleans it with svcFlushProcessDataCache and
//! invalidates the instruction cache with Luma3DS's custom SVC 0x93. The
//! Horizon path requires Luma3DS and is not exercised by the QEMU tests.

pub struct Arena {
    base: *mut u32,
    words: usize,
    used: usize,
    #[cfg(target_os = "horizon")]
    process: u32,
}

impl Arena {
    pub fn new(bytes: usize) -> Result<Self, alloc::string::String> {
        let bytes = bytes.next_multiple_of(4096);
        #[cfg_attr(not(target_os = "horizon"), allow(unused_variables))]
        let (base, process) = platform::map(bytes)?;
        Ok(Self {
            base,
            words: bytes / 4,
            used: 0,
            #[cfg(target_os = "horizon")]
            process,
        })
    }

    /// Discard everything after the first `keep` words.
    pub fn reset(&mut self, keep: usize) {
        self.used = keep;
    }

    pub fn used_bytes(&self) -> usize {
        self.used * 4
    }

    /// Copy and publish code; returns its address or None when full.
    pub fn publish(&mut self, words: &[u32]) -> Option<usize> {
        if words.len() > self.words - self.used {
            return None;
        }
        // SAFETY: the range is inside this exclusively owned mapping, and no
        // generated code runs while the owner publishes.
        let start = unsafe { self.base.add(self.used) };
        unsafe { core::ptr::copy_nonoverlapping(words.as_ptr(), start, words.len()) };
        self.used += words.len();
        #[cfg(target_os = "horizon")]
        platform::publish(self.process, start as usize, words.len() * 4);
        #[cfg(not(target_os = "horizon"))]
        platform::publish(start as usize, words.len() * 4);
        Some(start as usize)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    pub fn map(bytes: usize) -> Result<(*mut u32, u32), alloc::string::String> {
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
        Ok((base.cast(), 0))
    }

    pub fn publish(start: usize, bytes: usize) {
        // ARM-private cacheflush(start, end, 0): clean D, invalidate I.
        const CACHEFLUSH: libc::c_long = 0x0f_0002;
        // SAFETY: the range lies inside our own mapping.
        let result = unsafe { libc::syscall(CACHEFLUSH, start, start + bytes, 0) };
        assert_eq!(result, 0, "ARM cacheflush failed");
    }
}

#[cfg(target_os = "horizon")]
mod platform {
    use core::arch::asm;

    const CURRENT_PROCESS: u32 = 0xffff_8001;
    const MEMOP_PROT: u32 = 6;
    const MEMPERM_RWX: u32 = 7;

    pub fn map(bytes: usize) -> Result<(*mut u32, u32), alloc::string::String> {
        let layout = core::alloc::Layout::from_size_align(bytes, 4096)
            .map_err(|_| alloc::string::String::from("JIT code layout"))?;
        // SAFETY: nonzero, page-aligned layout; the arena owns it for life.
        let base = unsafe { alloc::alloc::alloc(layout) };
        if base.is_null() {
            return Err("JIT code allocation failed".into());
        }
        let (mut result, process): (i32, u32);
        // SAFETY: svcDuplicateHandle(r1 = original) returns r0 = result,
        // r1 = handle; SVCs preserve other registers.
        unsafe {
            asm!("svc 0x27", inout("r0") 0i32 => result, inout("r1") CURRENT_PROCESS => process,
                 out("r2") _, out("r3") _, options(nostack));
        }
        if result < 0 {
            return Err(alloc::format!("svcDuplicateHandle: {result:#x}"));
        }
        // SAFETY: svcControlProcessMemory(process, addr0, addr1, size, op, perm)
        // changes only the permissions of this owned, page-aligned range.
        unsafe {
            asm!("svc 0x70", inout("r0") process => result, inout("r1") base as u32 => _,
                 inout("r2") base as u32 => _, inout("r3") bytes as u32 => _,
                 in("r4") MEMOP_PROT, in("r5") MEMPERM_RWX, options(nostack));
        }
        if result < 0 {
            return Err(alloc::format!(
                "svcControlProcessMemory (Luma3DS required): {result:#x}"
            ));
        }
        Ok((base.cast(), process))
    }

    pub fn publish(process: u32, start: usize, bytes: usize) {
        // SAFETY: cache maintenance on our own range has no other effect.
        unsafe {
            asm!("svc 0x54", inout("r0") process => _, inout("r1") start as u32 => _,
                 inout("r2") bytes as u32 => _, out("r3") _, options(nostack));
            asm!("svc 0x93", inout("r0") start as u32 => _, inout("r1") bytes as u32 => _,
                 out("r2") _, out("r3") _, options(nostack));
        }
    }
}
