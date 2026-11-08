use std::io;
use std::mem::size_of;
use std::os::fd::RawFd;
use std::os::raw::{c_int, c_long, c_ulong, c_void};
use std::ptr::NonNull;

use crate::kvm::error::{KvmError, Result};

pub const KVM_API_VERSION: i32 = 12;
pub const KVMIO: u32 = 0xae;

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;

const fn ioc(dir: u32, ty: u32, nr: u32, size: usize) -> c_ulong {
    ((dir as c_ulong) << IOC_DIRSHIFT)
        | ((ty as c_ulong) << IOC_TYPESHIFT)
        | ((nr as c_ulong) << IOC_NRSHIFT)
        | ((size as c_ulong) << IOC_SIZESHIFT)
}

const fn io(ty: u32, nr: u32) -> c_ulong {
    ioc(IOC_NONE, ty, nr, 0)
}

const fn ior<T>(ty: u32, nr: u32) -> c_ulong {
    ioc(IOC_READ, ty, nr, size_of::<T>())
}

const fn iow<T>(ty: u32, nr: u32) -> c_ulong {
    ioc(IOC_WRITE, ty, nr, size_of::<T>())
}

pub const KVM_GET_API_VERSION: c_ulong = io(KVMIO, 0x00);
pub const KVM_CREATE_VM: c_ulong = io(KVMIO, 0x01);
pub const KVM_CHECK_EXTENSION: c_ulong = io(KVMIO, 0x03);
pub const KVM_GET_VCPU_MMAP_SIZE: c_ulong = io(KVMIO, 0x04);
pub const KVM_CREATE_VCPU: c_ulong = io(KVMIO, 0x41);
pub const KVM_SET_USER_MEMORY_REGION: c_ulong = iow::<KvmUserspaceMemoryRegion>(KVMIO, 0x46);
pub const KVM_RUN: c_ulong = io(KVMIO, 0x80);
pub const KVM_GET_ONE_REG: c_ulong = iow::<KvmOneReg>(KVMIO, 0xab);
pub const KVM_SET_ONE_REG: c_ulong = iow::<KvmOneReg>(KVMIO, 0xac);
pub const KVM_ARM_VCPU_INIT: c_ulong = iow::<KvmVcpuInit>(KVMIO, 0xae);
pub const KVM_ARM_PREFERRED_TARGET: c_ulong = ior::<KvmVcpuInit>(KVMIO, 0xaf);

pub const KVM_EXIT_UNKNOWN: u32 = 0;
pub const KVM_EXIT_EXCEPTION: u32 = 1;
pub const KVM_EXIT_IO: u32 = 2;
pub const KVM_EXIT_HYPERCALL: u32 = 3;
pub const KVM_EXIT_DEBUG: u32 = 4;
pub const KVM_EXIT_HLT: u32 = 5;
pub const KVM_EXIT_MMIO: u32 = 6;
pub const KVM_EXIT_IRQ_WINDOW_OPEN: u32 = 7;
pub const KVM_EXIT_SHUTDOWN: u32 = 8;
pub const KVM_EXIT_FAIL_ENTRY: u32 = 9;
pub const KVM_EXIT_INTERNAL_ERROR: u32 = 17;
pub const KVM_EXIT_SYSTEM_EVENT: u32 = 24;

pub const KVM_CAP_USER_MEMORY: u32 = 3;
pub const KVM_CAP_NR_MEMSLOTS: u32 = 10;
pub const KVM_CAP_ARM_VM_IPA_SIZE: u32 = 165;

pub const KVM_VM_TYPE_ARM_IPA_SIZE_MASK: u64 = 0xff;

pub const KVM_REG_ARM64: u64 = 0x6000_0000_0000_0000;
pub const KVM_REG_SIZE_U64: u64 = 0x0030_0000_0000_0000;
pub const KVM_REG_ARM_CORE: u64 = 0x0010_0000;
// KVM ARM64 system register class, S3_0_C0_C4_1.
pub const KVM_ARM64_REG_ID_AA64PFR1_EL1: u64 = 0x6030_0000_0013_c021;

// Linux prctl UAPI; libc does not expose these on all supported targets.
pub const PR_GET_SPECULATION_CTRL: c_int = 52;
pub const PR_SPEC_STORE_BYPASS: c_ulong = 0;
pub const PR_SPEC_PRCTL: c_int = 1 << 0;
pub const PR_SPEC_ENABLE: c_int = 1 << 1;

pub const KVM_ARM_CORE_REG_X0: u64 = 0;
pub const KVM_ARM_CORE_REG_SP: u64 = 62;
pub const KVM_ARM_CORE_REG_PC: u64 = 64;
pub const KVM_ARM_CORE_REG_PSTATE: u64 = 66;
pub const KVM_ARM_CORE_REG_SP_EL1: u64 = 68;
pub const KVM_ARM_CORE_REG_ELR_EL1: u64 = 70;
pub const KVM_ARM_CORE_REG_SPSR_EL1: u64 = 72;

pub const KVM_ARM_VCPU_POWER_OFF: u32 = 0;

pub const PSR_MODE_EL1H: u64 = 0x0000_0005;
pub const PSR_F_BIT: u64 = 0x0000_0040;
pub const PSR_I_BIT: u64 = 0x0000_0080;
pub const PSR_A_BIT: u64 = 0x0000_0100;
pub const PSR_D_BIT: u64 = 0x0000_0200;
pub const PSR_SSBS_BIT: u64 = 1 << 12;
pub const PSR_DAIF_MASKED_EL1H: u64 = PSR_MODE_EL1H | PSR_F_BIT | PSR_I_BIT | PSR_A_BIT | PSR_D_BIT;

pub const KVM_MEM_LOG_DIRTY_PAGES: u32 = 1 << 0;
pub const KVM_MEM_READONLY: u32 = 1 << 1;

pub const PROT_READ: c_int = 0x1;
pub const PROT_WRITE: c_int = 0x2;
pub const MAP_SHARED: c_int = 0x01;
pub const MAP_PRIVATE: c_int = 0x02;
pub const MAP_ANONYMOUS: c_int = 0x20;
pub const EINTR: i32 = 4;
pub const EAGAIN: i32 = 11;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvmUserspaceMemoryRegion {
    pub slot: u32,
    pub flags: u32,
    pub guest_phys_addr: u64,
    pub memory_size: u64,
    pub userspace_addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KvmVcpuInit {
    pub target: u32,
    pub features: [u32; 7],
}

impl Default for KvmVcpuInit {
    fn default() -> Self {
        Self {
            target: 0,
            features: [0; 7],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvmOneReg {
    pub id: u64,
    pub addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvmRunMmio {
    pub phys_addr: u64,
    pub data: [u8; 8],
    pub len: u32,
    pub is_write: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvmRunFailEntry {
    pub hardware_entry_failure_reason: u64,
    pub cpu: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvmRunInternalError {
    pub suberror: u32,
    pub ndata: u32,
    pub data: [u64; 16],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KvmRunSystemEvent {
    pub event_type: u32,
    pub ndata: u32,
    pub data: [u64; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union KvmRunExit {
    pub mmio: KvmRunMmio,
    pub fail_entry: KvmRunFailEntry,
    pub internal: KvmRunInternalError,
    pub system_event: KvmRunSystemEvent,
    pub raw: [u64; 32],
}

impl Default for KvmRunExit {
    fn default() -> Self {
        Self { raw: [0; 32] }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KvmRun {
    pub request_interrupt_window: u8,
    pub immediate_exit: u8,
    pub padding1: [u8; 6],
    pub exit_reason: u32,
    pub ready_for_interrupt_injection: u8,
    pub if_flag: u8,
    pub flags: u16,
    pub cr8: u64,
    pub apic_base: u64,
    pub exit: KvmRunExit,
}

impl KvmRun {
    pub fn mmio(&self) -> KvmRunMmio {
        unsafe { self.exit.mmio }
    }

    pub fn fail_entry(&self) -> KvmRunFailEntry {
        unsafe { self.exit.fail_entry }
    }

    pub fn internal_error(&self) -> KvmRunInternalError {
        unsafe { self.exit.internal }
    }

    pub fn system_event(&self) -> KvmRunSystemEvent {
        unsafe { self.exit.system_event }
    }
}

unsafe extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn mmap(
        addr: *mut c_void,
        length: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: c_long,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, length: usize) -> c_int;
}

pub fn arm_vm_type_for_ipa_bits(bits: u8) -> c_ulong {
    u64::from(bits & KVM_VM_TYPE_ARM_IPA_SIZE_MASK as u8) as c_ulong
}

pub const fn arm_core_reg_id(index: u64) -> u64 {
    KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM_CORE | index
}

pub unsafe fn ioctl_value(fd: RawFd, request: c_ulong, value: c_ulong) -> io::Result<i32> {
    let ret = unsafe { ioctl(fd, request, value) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

pub unsafe fn ioctl_ref<T>(fd: RawFd, request: c_ulong, value: &T) -> io::Result<i32> {
    let ret = unsafe { ioctl(fd, request, value as *const T) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

pub unsafe fn ioctl_mut<T>(fd: RawFd, request: c_ulong, value: &mut T) -> io::Result<i32> {
    let ret = unsafe { ioctl(fd, request, value as *mut T) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

pub unsafe fn mmap_anonymous_rw(len: usize) -> Result<NonNull<u8>> {
    let ptr = unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if ptr as isize == -1 {
        Err(KvmError::io("mmap anonymous"))
    } else {
        NonNull::new(ptr.cast::<u8>()).ok_or_else(|| KvmError::Io {
            op: "mmap anonymous returned null",
            source: io::Error::from_raw_os_error(12),
        })
    }
}

pub unsafe fn mmap_shared_rw(fd: RawFd, len: usize) -> Result<NonNull<u8>> {
    let ptr = unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            fd,
            0,
        )
    };
    if ptr as isize == -1 {
        Err(KvmError::io("mmap shared"))
    } else {
        NonNull::new(ptr.cast::<u8>()).ok_or_else(|| KvmError::Io {
            op: "mmap shared returned null",
            source: io::Error::from_raw_os_error(12),
        })
    }
}

pub unsafe fn munmap_region(ptr: NonNull<u8>, len: usize) -> Result<()> {
    let ret = unsafe { munmap(ptr.as_ptr().cast::<c_void>(), len) };
    if ret < 0 {
        Err(KvmError::io("munmap"))
    } else {
        Ok(())
    }
}

pub fn clear_instruction_cache(start: *mut u8, len: usize) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let begin = start as usize;
        let end = begin.saturating_add(len);
        let ctr_el0: usize;
        core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr_el0, options(nomem, nostack, preserves_flags));
        let d_line = 4_usize << ((ctr_el0 >> 16) & 0xf);
        let i_line = 4_usize << (ctr_el0 & 0xf);

        let mut ptr = begin & !(d_line - 1);
        while ptr < end {
            core::arch::asm!("dc cvau, {}", in(reg) ptr, options(nostack, preserves_flags));
            ptr += d_line;
        }
        core::arch::asm!("dsb ish", options(nostack, preserves_flags));

        let mut ptr = begin & !(i_line - 1);
        while ptr < end {
            core::arch::asm!("ic ivau, {}", in(reg) ptr, options(nostack, preserves_flags));
            ptr += i_line;
        }
        core::arch::asm!("dsb ish", options(nostack, preserves_flags));
        core::arch::asm!("isb", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = (start, len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_linux_uapi_on_64_bit() {
        assert_eq!(KVM_GET_API_VERSION, 0xae00);
        assert_eq!(KVM_CREATE_VM, 0xae01);
        assert_eq!(KVM_CHECK_EXTENSION, 0xae03);
        assert_eq!(KVM_GET_VCPU_MMAP_SIZE, 0xae04);
        assert_eq!(KVM_CREATE_VCPU, 0xae41);
        assert_eq!(KVM_RUN, 0xae80);
        assert_eq!(KVM_GET_ONE_REG, 0x4010_aeab);
        assert_eq!(KVM_SET_ONE_REG, 0x4010_aeac);
        assert_eq!(KVM_SET_USER_MEMORY_REGION, 0x4020_ae46);
        assert_eq!(KVM_ARM_VCPU_INIT, 0x4020_aeae);
        assert_eq!(KVM_ARM_PREFERRED_TARGET, 0x8020_aeaf);
    }

    #[test]
    fn arm_vm_type_is_low_ipa_bits() {
        assert_eq!(arm_vm_type_for_ipa_bits(34), 34);
        assert_eq!(arm_vm_type_for_ipa_bits(40), 40);
        assert_eq!(arm_vm_type_for_ipa_bits(0xff), 0xff);
    }

    #[test]
    fn arm_core_register_ids_match_arm64_uapi_offsets() {
        assert_eq!(arm_core_reg_id(KVM_ARM_CORE_REG_PC), 0x6030_0000_0010_0040);
        assert_eq!(
            arm_core_reg_id(KVM_ARM_CORE_REG_PSTATE),
            0x6030_0000_0010_0042
        );
        assert_eq!(
            arm_core_reg_id(KVM_ARM_CORE_REG_SP_EL1),
            0x6030_0000_0010_0044
        );
        assert_eq!(PSR_DAIF_MASKED_EL1H, 0x3c5);
    }
}
