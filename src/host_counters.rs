//! Explicit Apple process diagnostics; never sampled by the execution loop.
//! Counters include all process threads/VMs. Some virtualized hosts return zero.
use std::io;

pub(crate) fn report(guest_steps: u64) -> String {
    // SAFETY: zero is a valid initial representation of this C scalar record.
    let mut usage: libc::rusage_info_v4 = unsafe { core::mem::zeroed() };
    // SAFETY: V4 selects exactly this output structure. The C API's buffer is
    // spelled rusage_info_t*, but receives the record itself, not an indirect
    // pointer. We query only our own PID and never modify process state.
    let result = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V4,
            (&mut usage as *mut libc::rusage_info_v4).cast(),
        )
    };
    if result != 0 {
        return format!(
            "host counters: scope=process guest_steps={guest_steps} unavailable={}\n",
            io::Error::last_os_error()
        );
    }
    let instructions = usage.ri_instructions;
    let cycles = usage.ri_cycles;
    format!(
        "host counters: scope=process guest_steps={guest_steps} instructions={instructions} cycles={cycles} available={}\n",
        instructions != 0 && cycles != 0
    )
}
