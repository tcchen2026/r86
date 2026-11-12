//! Kernel-only MAP_JIT lifetime probe, signed/launched by apple_jit_check.py.
//! Uses the production arena's mmap flags and captures errno at the syscall.
//! No guest or generated code runs. One process releases its first mapping,
//! allocates a replacement, then requests another while the replacement lives.
//! CI with SIP off is an observation, not evidence of code-signing policy.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() {
    use std::io;

    struct Arena {
        base: *mut libc::c_void,
        bytes: usize,
    }

    fn errno() -> i32 {
        io::Error::last_os_error().raw_os_error().unwrap()
    }

    impl Arena {
        fn new(bytes: usize) -> Result<Self, i32> {
            // SAFETY: fresh anonymous mapping, no fixed address or file.
            let base = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    bytes,
                    libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                    libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                    -1,
                    0,
                )
            };
            if base == libc::MAP_FAILED {
                Err(errno())
            } else {
                Ok(Self { base, bytes })
            }
        }

        // Explicit release lets the report distinguish munmap failure from a
        // successful release followed by denial of the replacement mapping.
        fn release(self) -> i32 {
            // SAFETY: this arena owns exactly this live mapping; no references
            // or native code access it. No implicit destructor unmaps it again.
            if unsafe { libc::munmap(self.base, self.bytes) } == 0 {
                0
            } else {
                errno()
            }
        }
    }

    fn outcome(arena: &Option<Result<Arena, i32>>) -> Option<i32> {
        match arena {
            Some(Ok(_)) => Some(0),
            Some(Err(error)) => Some(*error),
            None => None,
        }
    }

    fn release(arena: Option<Result<Arena, i32>>) -> Option<i32> {
        match arena {
            Some(Ok(arena)) => Some(arena.release()),
            _ => None,
        }
    }

    // SAFETY: sysconf returns a process-independent scalar.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert!(page > 0, "cannot determine page size");
    let bytes = page as usize;
    let first = Some(Arena::new(bytes));
    let first_errno = outcome(&first);
    let first_release_errno = release(first);
    let sequential = (first_release_errno == Some(0)).then(|| Arena::new(bytes));
    let sequential_errno = outcome(&sequential);
    let concurrent = matches!(&sequential, Some(Ok(_))).then(|| Arena::new(bytes));
    let concurrent_errno = outcome(&concurrent);
    let concurrent_release_errno = release(concurrent);
    let sequential_release_errno = release(sequential);
    let json = |error: Option<i32>| error.map_or_else(|| "null".into(), |code| code.to_string());
    println!(
        "MAP_JIT_REGIONS {{\"pid\":{},\"bytes\":{bytes},\"first_errno\":{},\
         \"first_release_errno\":{},\"sequential_errno\":{},\"concurrent_errno\":{},\
         \"sequential_release_errno\":{},\"concurrent_release_errno\":{}}}",
        std::process::id(),
        json(first_errno),
        json(first_release_errno),
        json(sequential_errno),
        json(concurrent_errno),
        json(sequential_release_errno),
        json(concurrent_release_errno),
    );
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {
    eprintln!("apple_jit_regions requires an Apple silicon Mac");
    std::process::exit(1);
}
