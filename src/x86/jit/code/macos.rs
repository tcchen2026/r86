//! One MAP_JIT mapping, owned and mutated by the VM's core thread.
use super::CodeError;
use core::cell::Cell;
use core::ptr::NonNull;
use std::io;
use std::thread::{self, ThreadId};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Execute,
    Write,
}

// Apple protects every MAP_JIT region on the current thread together. Keeping
// this state per arena would be wrong when two arenas alternate on one thread.
// None until this thread first sets a mode: do not assume Apple's default.
thread_local! {
    static MODE: Cell<Option<Mode>> = const { Cell::new(None) };
}

pub(super) struct Mapping {
    pub(super) base: NonNull<u8>,
    pub(super) writable: NonNull<u8>,
    mapped: usize,
    owner: ThreadId,
    writes: Cell<u64>,
    executes: Cell<u64>,
    created: Instant,
}

fn check_owner(owner: ThreadId) {
    assert_eq!(
        owner,
        thread::current().id(),
        "JIT arena used off its core thread"
    );
}

impl Mapping {
    pub(super) fn new(capacity: usize, entry_bytes: usize) -> Result<(Self, usize), CodeError> {
        // SAFETY: sysconf reads a process-independent scalar.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 || !(page_size as usize).is_power_of_two() || capacity == 0 {
            return Err(CodeError::Capacity);
        }
        let page_size = page_size as usize;
        let entries_start = capacity
            .checked_add(page_size - 1)
            .ok_or(CodeError::Capacity)?
            & !(page_size - 1);
        let mapped = entries_start
            .checked_add(entry_bytes)
            .ok_or(CodeError::Capacity)?;
        // Dense entry tables store signed displacements to their native code.
        if mapped > i32::MAX as usize {
            return Err(CodeError::Capacity);
        }
        // SAFETY: new anonymous mapping; no fixed address or existing mapping.
        // Thread-local write protection enforces W^X before any access below.
        let base = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                mapped,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(CodeError::Os(io::Error::last_os_error()));
        }
        let base = NonNull::new(base.cast()).expect("JIT mapping returned address zero");
        Ok((
            Self {
                base,
                writable: base,
                mapped,
                owner: thread::current().id(),
                writes: Cell::new(0),
                executes: Cell::new(0),
                created: Instant::now(),
            },
            entries_start,
        ))
    }

    fn enter(&self, mode: Mode) {
        MODE.with(|current| {
            if current.get() != Some(mode) {
                // Mapping is !Send, so only unsafe code could reach here from
                // another thread; check where it matters, at each transition.
                check_owner(self.owner);
                // SAFETY: only the owner runs code or writes this mapping. No
                // generated code or outstanding mutable table borrow is active
                // while switching. No other code in r86 changes this permission.
                unsafe { libc::pthread_jit_write_protect_np(i32::from(mode == Mode::Execute)) };
                current.set(Some(mode));
                let count = if mode == Mode::Write {
                    &self.writes
                } else {
                    &self.executes
                };
                count.set(count.get() + 1);
            }
        });
    }

    pub(super) fn write(&self) {
        self.enter(Mode::Write);
    }
    pub(super) fn execute(&self) {
        self.enter(Mode::Execute);
    }

    pub(super) fn report(&self) -> String {
        check_owner(self.owner);
        let writes = self.writes.get();
        let executes = self.executes.get();
        let seconds = self.created.elapsed().as_secs_f64();
        format!(
            "jit protection: writes={writes} executes={executes} seconds={seconds:.6} toggles_per_second={:.3}\n",
            (writes + executes) as f64 / seconds.max(f64::MIN_POSITIVE)
        )
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        self.execute();
        // SAFETY: final owner on its core thread, with no native invocation.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.mapped) };
    }
}

pub(super) fn small_host() -> bool {
    let mut bytes = 0_u64;
    let mut len = size_of::<u64>();
    // SAFETY: matching output buffer/size, immutable NUL-terminated key; no
    // new value is provided, so this only reads physical memory capacity.
    let status = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut bytes as *mut u64).cast(),
            &mut len,
            core::ptr::null_mut(),
            0,
        )
    };
    status == 0 && len == size_of::<u64>() && bytes != 0 && bytes < 1024 * 1024 * 1024
}

unsafe extern "C" {
    fn sys_icache_invalidate(start: *mut core::ffi::c_void, len: usize);
}

pub(super) unsafe fn synchronize_instructions(
    writer: *mut u8,
    executable: *const u8,
    regions: &[(usize, usize)],
) {
    debug_assert_eq!(writer.cast_const(), executable);
    for &(offset, len) in regions {
        // SAFETY: caller supplies initialized, exclusively owned mapped ranges.
        unsafe { sys_icache_invalidate(writer.add(offset).cast(), len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_check_rejects_another_thread_in_release_too() {
        let owner = thread::current().id();
        assert!(thread::spawn(move || check_owner(owner)).join().is_err());
    }

    #[test]
    fn protection_mode_is_shared_by_arenas_on_one_thread() {
        let (first, _) = Mapping::new(4096, 0).unwrap();
        let (second, _) = Mapping::new(4096, 0).unwrap();
        first.execute();
        first.write();
        first.write();
        assert_eq!(first.writes.get(), 1);
        second.execute();
        first.write();
        assert_eq!(first.writes.get(), 2);
        second.execute();
        assert_eq!(second.executes.get(), 2);
    }
}
