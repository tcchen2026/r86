//! One process-lifetime MAP_JIT reservation, with sixteen fixed VM arena slots.
//! Slots retain their backing when released; only their owning core may write
//! or invoke them. The pool mutex is used at construction/drop, never execution.
use super::CodeError;
use core::cell::Cell;
use core::ptr::NonNull;
use std::io;
use std::sync::Mutex;
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

const SLOT_COUNT: usize = 16;
pub(super) const CODE_BYTES: usize = 256 * 1024 * 1024;
const SLOT_BYTES: usize = 320 * 1024 * 1024;

struct Pool {
    // Exposed mmap address, not an owned Rust allocation. It is never unmapped.
    // Each live Mapping owns a disjoint slot and remains !Send/!Sync.
    base: usize,
    slot_bytes: usize,
    high_water: usize,
    free: [usize; SLOT_COUNT],
    free_len: usize,
}

static POOL: Mutex<Option<Pool>> = Mutex::new(None);

impl Pool {
    fn reserve(page_size: usize) -> Result<Self, CodeError> {
        let slot_bytes = SLOT_BYTES
            .checked_add(page_size - 1)
            .ok_or(CodeError::Capacity)?
            & !(page_size - 1);
        let bytes = slot_bytes
            .checked_mul(SLOT_COUNT)
            .ok_or(CodeError::Capacity)?;
        // SAFETY: one new anonymous reservation; the mutex serializes its first
        // creation. Per-thread write protection applies to all slots together.
        let base = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            let error = io::Error::last_os_error();
            return Err(CodeError::Os(io::Error::new(
                error.kind(),
                format!(
                    "mmap(MAP_JIT) failed: {error}; Hardened Runtime requires \
                     com.apple.security.cs.allow-jit"
                ),
            )));
        }
        assert!(!base.is_null(), "JIT reservation returned address zero");
        Ok(Self {
            base: base.expose_provenance(),
            slot_bytes,
            high_water: 0,
            free: [0; SLOT_COUNT],
            free_len: 0,
        })
    }

    fn take(&mut self) -> Result<(NonNull<u8>, usize), CodeError> {
        let slot = if self.free_len != 0 {
            self.free_len -= 1;
            self.free[self.free_len]
        } else if self.high_water < SLOT_COUNT {
            let slot = self.high_water;
            self.high_water += 1;
            slot
        } else {
            return Err(CodeError::Os(io::Error::other(format!(
                "macOS JIT VM limit reached: all {SLOT_COUNT} arena slots are in use"
            ))));
        };
        let base = core::ptr::with_exposed_provenance_mut(self.base + slot * self.slot_bytes);
        Ok((NonNull::new(base).expect("reserved JIT slot address"), slot))
    }

    fn release(&mut self, slot: usize) {
        self.free[self.free_len] = slot;
        self.free_len += 1;
    }
}

pub(super) struct Mapping {
    pub(super) base: NonNull<u8>,
    pub(super) writable: NonNull<u8>,
    slot: usize,
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
        if page_size <= 0
            || !(page_size as usize).is_power_of_two()
            || capacity == 0
            || capacity > CODE_BYTES
        {
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
        // Slot-local entry-table displacements fit i32. A direct branch beyond
        // its encoding's reach stays unchained in CodeArena::patch_branch.
        // Small hosts use their existing smaller code/table layout in this slot.
        if mapped > SLOT_BYTES {
            return Err(CodeError::Capacity);
        }
        let mut pool = POOL.lock().expect("JIT reservation mutex poisoned");
        if pool.is_none() {
            *pool = Some(Pool::reserve(page_size)?);
        }
        let (base, slot) = pool.as_mut().unwrap().take()?;
        Ok((
            Self {
                base,
                writable: base,
                slot,
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
        check_owner(self.owner);
        self.execute();
        POOL.lock()
            .expect("JIT reservation mutex poisoned")
            .as_mut()
            .expect("JIT reservation exists")
            .release(self.slot);
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

    // The CI native test invocation uses --test-threads=1, so all sixteen slots
    // are available to this test; generated-code tests reuse them afterwards.
    #[test]
    fn pool_reuses_slots_and_reports_exhaustion() {
        let mut live: Vec<_> = (0..SLOT_COUNT)
            .map(|_| Mapping::new(CODE_BYTES, SLOT_BYTES - CODE_BYTES).unwrap().0)
            .collect();
        for (i, arena) in live.iter().enumerate() {
            for other in &live[..i] {
                assert!(
                    arena
                        .base
                        .as_ptr()
                        .addr()
                        .abs_diff(other.base.as_ptr().addr())
                        >= SLOT_BYTES
                );
            }
        }
        let last = live.pop().unwrap();
        let address = last.base;
        last.write();
        // SAFETY: this live slot is in write mode and the test owns its bytes.
        unsafe { last.writable.as_ptr().write(0xa5) };
        drop(last);
        let (reused, _) = Mapping::new(CODE_BYTES, SLOT_BYTES - CODE_BYTES).unwrap();
        assert_eq!(reused.base, address);
        // SAFETY: the new slot owner can read the initialized retained byte.
        assert_eq!(unsafe { reused.base.as_ptr().read() }, 0xa5);
        let error = match Mapping::new(4096, 0) {
            Err(error) => error,
            Ok(_) => panic!("arena beyond the slot limit was admitted"),
        };
        assert!(format!("{error:?}").contains("all 16 arena slots are in use"));
    }

    #[test]
    fn released_slot_can_change_core_threads() {
        let address = thread::spawn(|| {
            let (arena, _) = Mapping::new(4096, 0).unwrap();
            arena.write();
            // SAFETY: this core thread owns the slot and has entered write mode.
            unsafe { arena.writable.as_ptr().write(0x5a) };
            arena.base.as_ptr().addr()
        })
        .join()
        .unwrap();
        thread::spawn(move || {
            let (arena, _) = Mapping::new(4096, 0).unwrap();
            assert_eq!(arena.base.as_ptr().addr(), address);
            arena.execute();
            // SAFETY: the prior owner finished before this slot was acquired.
            assert_eq!(unsafe { arena.base.as_ptr().read() }, 0x5a);
        })
        .join()
        .unwrap();
    }
}
