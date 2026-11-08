use std::cell::OnceCell;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::ptr::{NonNull, addr_of_mut};
use std::sync::Once;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::kvm::backing::SlotBacking;
use crate::kvm::error::{KvmError, Result};
use crate::kvm::memslot::{MemslotPlan, PlannedMemslot};
use crate::kvm::sys;

#[derive(Debug)]
pub struct Kvm {
    file: File,
}

impl Kvm {
    pub fn open() -> Result<Self> {
        Self::open_path("/dev/kvm")
    }

    pub fn open_path(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|source| KvmError::Io {
                op: "open /dev/kvm",
                source,
            })?;
        Ok(Self { file })
    }

    pub fn raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    pub fn api_version(&self) -> Result<i32> {
        unsafe { sys::ioctl_value(self.raw_fd(), sys::KVM_GET_API_VERSION, 0) }.map_err(|source| {
            KvmError::Io {
                op: "KVM_GET_API_VERSION",
                source,
            }
        })
    }

    pub fn require_api_version(&self) -> Result<i32> {
        let version = self.api_version()?;
        if version != sys::KVM_API_VERSION {
            return Err(KvmError::BadApiVersion { found: version });
        }
        Ok(version)
    }

    pub fn check_extension(&self, cap: u32) -> Result<i32> {
        unsafe { sys::ioctl_value(self.raw_fd(), sys::KVM_CHECK_EXTENSION, cap.into()) }.map_err(
            |source| KvmError::Io {
                op: "KVM_CHECK_EXTENSION",
                source,
            },
        )
    }

    pub fn require_extension(&self, cap: u32, name: &'static str) -> Result<i32> {
        let value = self.check_extension(cap)?;
        if value <= 0 {
            return Err(KvmError::MissingCapability { cap, name });
        }
        Ok(value)
    }

    pub fn vcpu_mmap_size(&self) -> Result<i32> {
        unsafe { sys::ioctl_value(self.raw_fd(), sys::KVM_GET_VCPU_MMAP_SIZE, 0) }.map_err(
            |source| KvmError::Io {
                op: "KVM_GET_VCPU_MMAP_SIZE",
                source,
            },
        )
    }

    pub fn create_vm(&self, vm_type: u64) -> Result<Vm> {
        let fd = unsafe { sys::ioctl_value(self.raw_fd(), sys::KVM_CREATE_VM, vm_type as _) }
            .map_err(|source| KvmError::Io {
                op: "KVM_CREATE_VM",
                source,
            })?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Vm { fd })
    }

    pub fn create_arm_vm(&self, ipa_bits: u8) -> Result<Vm> {
        self.create_vm(sys::arm_vm_type_for_ipa_bits(ipa_bits) as u64)
    }
}

#[derive(Debug)]
pub struct Vm {
    fd: OwnedFd,
}

impl Vm {
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn set_user_memory_region(&self, slot: &PlannedMemslot, userspace_addr: u64) -> Result<()> {
        slot.validate()?;
        let region = sys::KvmUserspaceMemoryRegion {
            slot: slot.slot,
            flags: slot.flags,
            guest_phys_addr: slot.guest_phys_addr,
            memory_size: slot.memory_size,
            userspace_addr,
        };
        unsafe { sys::ioctl_ref(self.raw_fd(), sys::KVM_SET_USER_MEMORY_REGION, &region) }
            .map_err(|source| KvmError::Io {
                op: "KVM_SET_USER_MEMORY_REGION",
                source,
            })?;
        Ok(())
    }

    pub fn create_vcpu(&self, vcpu_id: u32) -> Result<Vcpu> {
        let fd = unsafe { sys::ioctl_value(self.raw_fd(), sys::KVM_CREATE_VCPU, vcpu_id.into()) }
            .map_err(|source| KvmError::Io {
            op: "KVM_CREATE_VCPU",
            source,
        })?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Vcpu { fd })
    }

    pub fn arm_preferred_target(&self) -> Result<sys::KvmVcpuInit> {
        let mut init = sys::KvmVcpuInit::default();
        unsafe { sys::ioctl_mut(self.raw_fd(), sys::KVM_ARM_PREFERRED_TARGET, &mut init) }
            .map_err(|source| KvmError::Io {
                op: "KVM_ARM_PREFERRED_TARGET",
                source,
            })?;
        Ok(init)
    }

    pub fn register_backing(
        &self,
        plan: &MemslotPlan,
        backing: Vec<SlotBacking>,
    ) -> Result<RegisteredVmMemory> {
        if plan.len() != backing.len() {
            return Err(KvmError::MemslotPlan(
                "memslot backing count does not match plan",
            ));
        }
        for (slot, backing) in plan.slots().iter().zip(backing.iter()) {
            if slot.slot != backing.slot {
                return Err(KvmError::MemslotPlan("memslot backing id mismatch"));
            }
            if slot.memory_size != backing.memory.len() as u64 {
                return Err(KvmError::MemslotPlan("memslot backing size mismatch"));
            }
            self.set_user_memory_region(slot, backing.memory.userspace_addr())?;
        }
        Ok(RegisteredVmMemory { backing })
    }

    pub fn register_memslot_plan(&self, plan: &MemslotPlan) -> Result<RegisteredVmMemory> {
        let backing = plan.allocate_backing()?;
        self.register_backing(plan, backing)
    }
}

#[derive(Debug)]
pub struct Vcpu {
    fd: OwnedFd,
}

impl Vcpu {
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn arm_init(&self, init: &sys::KvmVcpuInit) -> Result<()> {
        unsafe { sys::ioctl_ref(self.raw_fd(), sys::KVM_ARM_VCPU_INIT, init) }.map_err(
            |source| KvmError::Io {
                op: "KVM_ARM_VCPU_INIT",
                source,
            },
        )?;
        Ok(())
    }

    pub fn set_one_reg_u64(&self, id: u64, value: u64) -> Result<()> {
        let reg = sys::KvmOneReg {
            id,
            addr: (&value as *const u64) as usize as u64,
        };
        unsafe { sys::ioctl_ref(self.raw_fd(), sys::KVM_SET_ONE_REG, &reg) }.map_err(|source| {
            KvmError::Io {
                op: "KVM_SET_ONE_REG",
                source,
            }
        })?;
        Ok(())
    }

    pub fn get_one_reg_u64(&self, id: u64) -> Result<u64> {
        let mut value = 0_u64;
        let reg = sys::KvmOneReg {
            id,
            addr: (&mut value as *mut u64) as usize as u64,
        };
        unsafe { sys::ioctl_ref(self.raw_fd(), sys::KVM_GET_ONE_REG, &reg) }.map_err(|source| {
            KvmError::Io {
                op: "KVM_GET_ONE_REG",
                source,
            }
        })?;
        Ok(value)
    }

    pub fn set_arm_core_reg(&self, core_index: u64, value: u64) -> Result<()> {
        self.set_one_reg_u64(sys::arm_core_reg_id(core_index), value)
    }

    pub fn set_arm_entry_state(&self, pc: u64, sp_el1: u64) -> Result<()> {
        self.set_arm_core_reg(sys::KVM_ARM_CORE_REG_PC, pc)?;
        self.set_arm_core_reg(sys::KVM_ARM_CORE_REG_SP_EL1, sp_el1)?;
        let mut pstate = sys::PSR_DAIF_MASKED_EL1H;
        let pfr1 = self.get_one_reg_u64(sys::KVM_ARM64_REG_ID_AA64PFR1_EL1)?;
        if matches!((pfr1 >> 4) & 0xf, 1 | 2) {
            // The monitor runs the same interpreter as this host task. Inherit
            // its store-bypass policy instead of imposing an extra mitigation
            // on every EL1 load. Unsupported or restricted policies stay safe.
            // This queries policy only; it never changes the host mitigation.
            let policy = unsafe {
                libc::prctl(
                    sys::PR_GET_SPECULATION_CTRL,
                    sys::PR_SPEC_STORE_BYPASS,
                    0_usize,
                    0_usize,
                    0_usize,
                )
            };
            if policy == sys::PR_SPEC_ENABLE || policy == (sys::PR_SPEC_PRCTL | sys::PR_SPEC_ENABLE)
            {
                pstate |= sys::PSR_SSBS_BIT;
            }
        }
        self.set_arm_core_reg(sys::KVM_ARM_CORE_REG_PSTATE, pstate)?;
        Ok(())
    }

    pub fn set_arm_entry_state_with_arg0(&self, pc: u64, sp_el1: u64, x0: u64) -> Result<()> {
        self.set_arm_entry_state(pc, sp_el1)?;
        self.set_arm_core_reg(sys::KVM_ARM_CORE_REG_X0, x0)?;
        Ok(())
    }

    pub fn map_run(&self, len: usize) -> Result<RunMap> {
        let ptr = unsafe { sys::mmap_shared_rw(self.raw_fd(), len)? };
        Ok(RunMap {
            ptr,
            len,
            watchdog: OnceCell::new(),
        })
    }

    pub fn run(&self) -> Result<()> {
        unsafe { sys::ioctl_value(self.raw_fd(), sys::KVM_RUN, 0) }.map_err(|source| {
            KvmError::Io {
                op: "KVM_RUN",
                source,
            }
        })?;
        Ok(())
    }

    pub fn run_with_timeout(&self, run_map: &RunMap, timeout: Duration) -> Result<bool> {
        let watchdog = run_map.watchdog.get_or_init(|| RunWatchdog::new(run_map));
        watchdog.arm(timeout);
        let result = loop {
            let result = unsafe { sys::ioctl_value(self.raw_fd(), sys::KVM_RUN, 0) };
            // A timeout signal can arrive after a completed exit. It must not
            // terminate the next run or discard a completed MMIO transaction.
            if result
                .as_ref()
                .is_err_and(|e| e.raw_os_error() == Some(sys::EINTR))
                && !watchdog.expired()
            {
                continue;
            }
            break result;
        };
        let timed_out = watchdog.disarm(run_map);
        match result {
            Ok(_) => Ok(true),
            Err(source)
                if timed_out
                    && matches!(source.raw_os_error(), Some(sys::EINTR) | Some(sys::EAGAIN)) =>
            {
                Ok(false)
            }
            Err(source) => Err(KvmError::Io {
                op: "KVM_RUN",
                source,
            }),
        }
    }
}

#[derive(Debug, Default)]
struct RunDeadline {
    deadline: Option<Instant>,
    // Deadline of the worker's current wait, even while a run is disarmed.
    sleeping_until: Option<Instant>,
    expired: bool,
    stop: bool,
}

/// One watchdog for the mmap lifetime, instead of a new thread per VM exit.
/// RunMap is not Send/Sync; its runner stays on the thread captured here. The
/// worker only accesses immediate_exit while armed, and is joined before unmap.
#[derive(Debug)]
struct RunWatchdog {
    state: Arc<(Mutex<RunDeadline>, Condvar)>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RunWatchdog {
    fn new(run_map: &RunMap) -> Self {
        install_timeout_signal_handler();
        let state = Arc::new((Mutex::new(RunDeadline::default()), Condvar::new()));
        let worker_state = Arc::clone(&state);
        let immediate_exit = run_map.immediate_exit_ptr() as usize;
        let runner = unsafe { libc::pthread_self() } as usize;
        let thread = thread::spawn(move || {
            let (lock, wake) = &*worker_state;
            let mut state = lock.lock().expect("KVM watchdog lock poisoned");
            while !state.stop {
                if state
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    state.expired = true;
                    state.deadline = None;
                    unsafe {
                        (immediate_exit as *mut u8).write_volatile(1);
                        libc::pthread_kill(runner as libc::pthread_t, libc::SIGUSR1);
                    }
                }
                state.sleeping_until = state.deadline;
                state = if let Some(deadline) = state.deadline {
                    wake.wait_timeout(state, deadline.saturating_duration_since(Instant::now()))
                        .expect("KVM watchdog wait poisoned")
                        .0
                } else {
                    wake.wait(state).expect("KVM watchdog wait poisoned")
                };
            }
        });
        Self {
            state,
            thread: Some(thread),
        }
    }

    fn arm(&self, timeout: Duration) {
        let mut state = self.state.0.lock().expect("KVM watchdog lock poisoned");
        let deadline = Instant::now() + timeout;
        // Extending a wait needs no wakeup. Shortening it, or arming an idle
        // worker, must wake it so the new deadline cannot be missed.
        let wake_earlier = state.sleeping_until.is_none_or(|sleep| deadline < sleep);
        state.deadline = Some(deadline);
        state.expired = false;
        if wake_earlier {
            self.state.1.notify_one();
        }
    }

    fn expired(&self) -> bool {
        self.state
            .0
            .lock()
            .expect("KVM watchdog lock poisoned")
            .expired
    }

    fn disarm(&self, run_map: &RunMap) -> bool {
        let mut state = self.state.0.lock().expect("KVM watchdog lock poisoned");
        state.deadline = None;
        run_map.set_immediate_exit(false);
        state.expired
    }
}

impl Drop for RunWatchdog {
    fn drop(&mut self) {
        self.state
            .0
            .lock()
            .expect("KVM watchdog lock poisoned")
            .stop = true;
        self.state.1.notify_one();
        self.thread
            .take()
            .unwrap()
            .join()
            .expect("KVM watchdog panicked");
    }
}

extern "C" fn handle_timeout_signal(_: libc::c_int) {}

fn install_timeout_signal_handler() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| unsafe {
        let mut action: libc::sigaction = core::mem::zeroed();
        action.sa_sigaction = handle_timeout_signal as *const () as usize;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGUSR1, &action, core::ptr::null_mut());
    });
}

#[derive(Debug)]
pub struct RegisteredVmMemory {
    backing: Vec<SlotBacking>,
}

impl RegisteredVmMemory {
    pub fn slot_count(&self) -> usize {
        self.backing.len()
    }

    pub fn backing(&self) -> &[SlotBacking] {
        &self.backing
    }

    pub fn backing_mut(&mut self) -> &mut [SlotBacking] {
        &mut self.backing
    }
}

#[derive(Debug)]
pub struct RunMap {
    ptr: NonNull<u8>,
    len: usize,
    watchdog: OnceCell<RunWatchdog>,
}

impl RunMap {
    pub fn as_run(&self) -> &sys::KvmRun {
        unsafe { &*(self.ptr.as_ptr().cast::<sys::KvmRun>()) }
    }

    pub fn exit_reason(&self) -> u32 {
        self.as_run().exit_reason
    }

    pub fn set_immediate_exit(&self, value: bool) {
        unsafe {
            self.immediate_exit_ptr().write_volatile(u8::from(value));
        }
    }

    pub fn immediate_exit_ptr(&self) -> *mut u8 {
        unsafe { addr_of_mut!((*self.ptr.as_ptr().cast::<sys::KvmRun>()).immediate_exit) }
    }

    pub fn mmio(&self) -> Option<sys::KvmRunMmio> {
        (self.exit_reason() == sys::KVM_EXIT_MMIO).then(|| self.as_run().mmio())
    }

    pub fn fail_entry(&self) -> Option<sys::KvmRunFailEntry> {
        (self.exit_reason() == sys::KVM_EXIT_FAIL_ENTRY).then(|| self.as_run().fail_entry())
    }

    pub fn internal_error(&self) -> Option<sys::KvmRunInternalError> {
        (self.exit_reason() == sys::KVM_EXIT_INTERNAL_ERROR).then(|| self.as_run().internal_error())
    }

    pub fn system_event(&self) -> Option<sys::KvmRunSystemEvent> {
        (self.exit_reason() == sys::KVM_EXIT_SYSTEM_EVENT).then(|| self.as_run().system_event())
    }
}

impl Drop for RunMap {
    fn drop(&mut self) {
        drop(self.watchdog.take());
        let _ = unsafe { sys::munmap_region(self.ptr, self.len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kvm::memslot::{MemslotKind, PlannedMemslot};

    fn watchdog_map() -> RunMap {
        let len = 65536;
        RunMap {
            ptr: unsafe { sys::mmap_anonymous_rw(len).unwrap() },
            len,
            watchdog: OnceCell::new(),
        }
    }

    fn wait_for(mut condition: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(2);
        while !condition() {
            assert!(Instant::now() < end, "watchdog did not make progress");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn watchdog_wakes_for_an_earlier_deadline() {
        let map = watchdog_map();
        let watchdog = map.watchdog.get_or_init(|| RunWatchdog::new(&map));
        watchdog.arm(Duration::from_secs(30));
        wait_for(|| watchdog.state.0.lock().unwrap().sleeping_until.is_some());
        watchdog.arm(Duration::from_millis(20));
        wait_for(|| watchdog.expired());
        assert!(watchdog.disarm(&map));
        assert_eq!(map.as_run().immediate_exit, 0);
    }

    #[test]
    fn watchdog_disarm_cancels_expiry_and_allows_rearm() {
        let map = watchdog_map();
        let watchdog = map.watchdog.get_or_init(|| RunWatchdog::new(&map));
        watchdog.arm(Duration::from_millis(20));
        assert!(!watchdog.disarm(&map));
        thread::sleep(Duration::from_millis(60));
        assert!(!watchdog.expired());
        assert_eq!(map.as_run().immediate_exit, 0);
        watchdog.arm(Duration::from_millis(20));
        wait_for(|| watchdog.expired());
        assert!(watchdog.disarm(&map));
        watchdog.arm(Duration::from_secs(30));
        assert!(!watchdog.expired());
        assert_eq!(map.as_run().immediate_exit, 0);
        assert!(!watchdog.disarm(&map));
    }

    #[test]
    fn watchdog_extending_deadline_does_not_expire_at_old_deadline() {
        let map = watchdog_map();
        let watchdog = map.watchdog.get_or_init(|| RunWatchdog::new(&map));
        watchdog.arm(Duration::from_millis(40));
        wait_for(|| watchdog.state.0.lock().unwrap().sleeping_until.is_some());
        watchdog.arm(Duration::from_secs(30));
        thread::sleep(Duration::from_millis(80));
        assert!(!watchdog.expired());
        assert_eq!(map.as_run().immediate_exit, 0);
        assert!(!watchdog.disarm(&map));
        // Drop must wake and join the sleeping worker, not wait thirty seconds.
    }

    #[test]
    fn planned_memslot_converts_to_uapi_region_shape() {
        let slot = PlannedMemslot::new(7, MemslotKind::Monitor, 0x1_0000_0000, 0x20_0000);
        let region = sys::KvmUserspaceMemoryRegion {
            slot: slot.slot,
            flags: slot.flags,
            guest_phys_addr: slot.guest_phys_addr,
            memory_size: slot.memory_size,
            userspace_addr: 0x1234_5000,
        };

        assert_eq!(region.slot, 7);
        assert_eq!(region.flags, 0);
        assert_eq!(region.guest_phys_addr, 0x1_0000_0000);
        assert_eq!(region.memory_size, 0x20_0000);
        assert_eq!(region.userspace_addr, 0x1234_5000);
    }

    #[test]
    fn run_map_mmio_parser_uses_linux_union_layout() {
        let mut raw = sys::KvmRun {
            request_interrupt_window: 0,
            immediate_exit: 0,
            padding1: [0; 6],
            exit_reason: sys::KVM_EXIT_MMIO,
            ready_for_interrupt_injection: 0,
            if_flag: 0,
            flags: 0,
            cr8: 0,
            apic_base: 0,
            exit: sys::KvmRunExit {
                mmio: sys::KvmRunMmio {
                    phys_addr: 0x1_f000_0000,
                    data: [0x36, 0x38, 0x43, 0x52, 0, 0, 0, 0],
                    len: 4,
                    is_write: 1,
                },
            },
        };
        let map = RunMap {
            ptr: NonNull::from(&mut raw).cast::<u8>(),
            len: 0,
            watchdog: OnceCell::new(),
        };

        let mmio = map.mmio().unwrap();
        assert_eq!(mmio.phys_addr, 0x1_f000_0000);
        assert_eq!(mmio.len, 4);
        assert_eq!(mmio.is_write, 1);
        assert_eq!(&mmio.data[..4], &[0x36, 0x38, 0x43, 0x52]);
        std::mem::forget(map);
    }
}
