#[cfg(all(
    feature = "jit",
    feature = "machine",
    target_arch = "aarch64",
    target_os = "none"
))]
mod jit_heap;

pub mod abi;
pub mod firmware;
pub mod shadow;

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod entry;

use crate::x86::mem::{
    ram_read_u16, ram_read_u32, ram_read_u64, ram_write_u16, ram_write_u32, ram_write_u64,
};
#[cfg(target_arch = "aarch64")]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{Ordering, compiler_fence, fence};

use crate::monitor::abi::{
    ABI_VERSION, AbiError, BootBlockV1, HOST_COMMAND_A_OFFSET, HOST_COMMAND_B_OFFSET,
    HOST_COMMAND_C_OFFSET, HOST_COMMAND_D_OFFSET, HOST_COMMAND_FLAGS_OFFSET,
    HOST_COMMAND_KIND_OFFSET, HOST_COMMAND_SEQ_OFFSET, HOST_COMMAND_SIZE_OFFSET, HostCommandKindV1,
    HostCommandV1, HostResponseV1, IpaLayoutV1, MAILBOX_ABI_VERSION_OFFSET, MAILBOX_COMMAND_OFFSET,
    MAILBOX_COMMAND_SEQ_OFFSET, MAILBOX_HEADER_SIZE_OFFSET, MAILBOX_REQ_SEQ_OFFSET,
    MAILBOX_REQUEST_OFFSET, MAILBOX_RESP_SEQ_OFFSET, MAILBOX_RESPONSE_OFFSET, MIN_ARM_IPA_BITS,
    MonitorRequestKindV1, MonitorRequestV1, PAGE_SIZE_4K, REQUEST_A_OFFSET, REQUEST_B_OFFSET,
    REQUEST_C_OFFSET, REQUEST_D_OFFSET, REQUEST_FLAGS_OFFSET, REQUEST_KIND_OFFSET,
    REQUEST_SEQ_OFFSET, REQUEST_SIZE_OFFSET, RESPONSE_A_OFFSET, RESPONSE_B_OFFSET,
    RESPONSE_C_OFFSET, RESPONSE_RESERVED_OFFSET, RESPONSE_SEQ_OFFSET, RESPONSE_SIZE_OFFSET,
    RESPONSE_STATUS_OFFSET, RESPONSE_VALUE_OFFSET, RegionClassV1, RegionKindV1, RegionTableV1,
    ResponseStatusV1, SHADOW_SUP_BASE, SHADOW_USR_BASE, SegmentStateV1,
    debug::{DebugPhaseV1, DebugStateBlockV1},
};
use crate::monitor::shadow::{
    ContextCache, ContextKey, RESIDENT_CONTEXTS, ShadowWindow, TableSource,
};
use crate::x86::cpu::{CR0_PE, CR0_PG, CR4_PAE, EFLAGS_IF, SegmentCache, TableReg};
use crate::x86::interp::{BatchStepStop, BatchStop};
use crate::x86::mem::{
    BackingToken, Bus, MemError, NativeRead, PhysRegionKind, PhysWrite, WriteOrigin,
};
use crate::x86::{
    AccessKind, AccessPriv, Cpu, Interpreter, LinearAddr, PhysAddr, Reg32, SegmentReg, StepError,
    StepOutcome, Translation, TranslationPageSize, deliver_exception, deliver_external_interrupt,
    deliver_software_interrupt_event,
};

pub const DEFAULT_RESPONSE_SPIN_LIMIT: u32 = 1_000_000;
pub const SERIAL_FLUSH_PACKET_CAPACITY: usize = 24;
pub const DOORBELL_MAGIC: u32 = 0x5243_3836;
pub const RUST_MONITOR_SMOKE_MARKER: &[u8] = b"R86_RUST_MONITOR_OK\n";
pub const X86_MONITOR_SMOKE_MARKER: &[u8] = b"R86_X86_MONITOR_OK\n";
pub const NULL_HARDMMU_STEP_MARKER: &[u8] = b"R86_NULL_HMMU_OK\n";
pub const NULL_HARDMMU_ERROR_MARKER: &[u8] = b"R86_NULL_HMMU_ERR\n";
pub const NATIVE_HARDMMU_READ_MARKER: &[u8] = b"R86_HMMU_NATIVE\n";
pub const HARDMMU_PAGED_SMOKE_MARKER: &[u8] = b"R86_HMMU_PAGED_OK\n";
pub const HARDMMU_PAGED_ERROR_MARKER: &[u8] = b"R86_HMMU_PAGED_ERR\n";
pub const HARDMMU_STATS_PARTS: u32 = 8;
pub const X86_MONITOR_SMOKE_BASE: u32 = 0x0000_7000;
pub const X86_MONITOR_SMOKE_MAX_STEPS: u32 = 256;
pub const NULL_HARDMMU_DEFAULT_STEPS: u64 = 2_000_000;
pub const X86_MONITOR_SMOKE_PROGRAM_LEN: usize = 5 + X86_MONITOR_SMOKE_MARKER.len() * 3 + 1;
pub const HARDMMU_PAGED_CODE_BASE: u32 = 0x0000_1000;
pub const HARDMMU_PAGED_PD_BASE: u32 = 0x0000_2000;
pub const HARDMMU_PAGED_PT_BASE: u32 = 0x0000_3000;
pub const HARDMMU_PAGED_DATA_BASE: u32 = 0x0000_4000;
pub const HARDMMU_PAGED_DATA_VALUE: u32 = 0x7856_3412;
pub const HARDMMU_PAGED_SMOKE_MAX_STEPS: u64 = 64;
pub const HARDMMU_CONTEXT_B_VALUE: u32 = 0x1234_abcd;
pub const HARDMMU_CONTEXT_C_VALUE: u32 = 0xfedc_9876;
pub const HARDMMU_PAGED_SMOKE_PROGRAM_LEN: usize = 141;
pub const STAGE1_TABLE_ENTRIES: usize = 512;
pub const STAGE1_L1_BLOCK_SIZE: u64 = 1 << 30;
pub const STAGE1_L2_BLOCK_SIZE: u64 = 1 << 21;
pub const STAGE1_NORMAL_WBWA_ATTR: u64 = 0xff;
pub const STAGE1_DEVICE_NGNRE_ATTR: u64 = 0x04;
pub const STAGE1_NORMAL_NC_ATTR: u64 = 0x44;
pub const STAGE1_MAIR_EL1: u64 =
    STAGE1_NORMAL_WBWA_ATTR | (STAGE1_DEVICE_NGNRE_ATTR << 8) | (STAGE1_NORMAL_NC_ATTR << 16);
pub const STAGE1_TCR_EL1_34BIT: u64 = 0x0000_0002_0000_351e;
pub const STAGE1_TCR_EL1_39BIT: u64 = 0x0000_0002_0000_3519;
pub const STAGE1_MAX_TTBR0_VA: u64 = 1 << 39;
pub const STAGE1_LOW_4K_WINDOW: u64 = STAGE1_L2_BLOCK_SIZE;
pub const MONITOR_SHADOW_FRAMES: usize = 65_536;
pub const MONITOR_SHADOW_L3_TABLES: usize = 512;
pub const HOST_COMMAND_POLL_INTERVAL: u64 = 4096;
const EFLAGS_VM: u32 = 1 << 17;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
const NATIVE_THUNK_STATUS_HIT: u64 = 0;
const NATIVE_THUNK_STATUS_TRANSLATION_ABORT: u64 = 1;
const NATIVE_THUNK_STATUS_ACCESS_FLAG_ABORT: u64 = 2;
const NATIVE_THUNK_STATUS_PERMISSION_ABORT: u64 = 3;
pub type DefaultMonitorShadowRuntime =
    MonitorShadowRuntime<MONITOR_SHADOW_FRAMES, MONITOR_SHADOW_L3_TABLES>;

#[cfg(target_arch = "aarch64")]
static EXCEPTION_PANIC_MAILBOX: AtomicU64 = AtomicU64::new(0);
#[cfg(target_arch = "aarch64")]
static EXCEPTION_PANIC_DOORBELL: AtomicU64 = AtomicU64::new(0);
#[cfg(target_arch = "aarch64")]
static EXCEPTION_PANIC_DEBUG_STATE: AtomicU64 = AtomicU64::new(0);
#[cfg(target_arch = "aarch64")]
static EXCEPTION_PANIC_BLOCK: AtomicU64 = AtomicU64::new(0);
#[cfg(target_arch = "aarch64")]
static EXCEPTION_PANIC_DEPTH: AtomicU64 = AtomicU64::new(0);

#[cfg(target_arch = "aarch64")]
pub fn install_exception_panic_transport(mailbox: *mut u8, doorbell: *mut u32) {
    install_exception_panic_transport_with_debug(
        mailbox,
        doorbell,
        core::ptr::null_mut(),
        core::ptr::null_mut(),
    );
}

#[cfg(target_arch = "aarch64")]
pub fn install_exception_panic_transport_with_debug(
    mailbox: *mut u8,
    doorbell: *mut u32,
    debug_state: *mut DebugStateBlockV1,
    panic_block: *mut crate::monitor::abi::debug::PanicBlockV1,
) {
    EXCEPTION_PANIC_MAILBOX.store(mailbox as u64, Ordering::Release);
    EXCEPTION_PANIC_DOORBELL.store(doorbell as u64, Ordering::Release);
    EXCEPTION_PANIC_DEBUG_STATE.store(debug_state as u64, Ordering::Release);
    EXCEPTION_PANIC_BLOCK.store(panic_block as u64, Ordering::Release);
}

#[cfg(target_arch = "aarch64")]
#[cfg(not(all(feature = "jit", feature = "machine", target_os = "none")))]
core::arch::global_asm!(
    ".global __r86_sync_current_el\n.set __r86_sync_current_el, __r86_sync_hard_read"
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    r#"
    .section .text.r86_vectors, "ax"
    .balign 2048
    .global __r86_vectors
__r86_vectors:
    b __r86_sync_current_el
    .balign 128
    b __r86_unexpected_exception
    .balign 128
    b __r86_unexpected_exception
    .balign 128
    b __r86_unexpected_exception
    .balign 128
    b __r86_sync_current_el
    .balign 128
    b __r86_unexpected_exception
    .balign 128
    b __r86_unexpected_exception
    .balign 128
    b __r86_unexpected_exception
    .balign 128
    b __r86_unexpected_exception
    .balign 128
    b __r86_unexpected_exception
    .balign 128
    b __r86_unexpected_exception
    .balign 128
    b __r86_unexpected_exception
    .balign 128
    b __r86_unexpected_exception
    .balign 128
    b __r86_unexpected_exception
    .balign 128
    b __r86_unexpected_exception
    .balign 128
    b __r86_unexpected_exception

    .global __r86_sync_hard_read
__r86_sync_hard_read:
    mrs x2, esr_el1
    // Current-EL data abort, read, no CM/EA/FnV: same checks in one mask.
    mov w6, #0x0740
    movk w6, #0xfc00, lsl #16
    and w5, w2, w6
    mov w6, #0x94000000
    cmp w5, w6
    b.ne __r86_unexpected_exception

    // DFSC[5:2] is 1=translation, 2=AF, 3=permission; all other types panic.
    ubfx x8, x2, #2, #4
    sub x6, x8, #1
    cmp x6, #2
    b.hi __r86_unexpected_exception

    // Each shadow window spans exactly 4 GiB at high word 0x10 or 0x20.
    mrs x3, far_el1
    lsr x5, x3, #32
    cmp x5, #0x20
    ccmp x5, #0x10, #4, ne
    b.ne __r86_unexpected_exception

    // Only offsets 0, 16, 32 and 48 are faulting instructions. The thunk
    // layout below fixes those offsets; padding and interior PCs are rejected.
    mrs x4, elr_el1
    adr x5, r86_hard_read_u8
    sub x6, x4, x5
    bic x6, x6, #0x30
    cbnz x6, __r86_unexpected_exception

__r86_fixup_hard_read:
    adr x5, r86_hard_read_abort_fixup
    msr elr_el1, x5
    mov x0, x3
    mov x1, x8
    eret

    .global __r86_unexpected_exception
__r86_unexpected_exception:
    mrs x0, esr_el1
    mrs x1, far_el1
    mrs x2, elr_el1
    mov x3, sp
    bl r86_monitor_exception_panic
    b __r86_unexpected_exception

    .section .text.r86_thunks, "ax"
    .balign 64
    .global r86_hard_read_u8
    .type r86_hard_read_u8, %function
r86_hard_read_u8:
    ldrb w0, [x0]
    mov x1, #0
    ret

    .org r86_hard_read_u8 + 16
    .global r86_hard_read_u16
    .type r86_hard_read_u16, %function
r86_hard_read_u16:
    ldrh w0, [x0]
    mov x1, #0
    ret

    .org r86_hard_read_u8 + 32
    .global r86_hard_read_u32
    .type r86_hard_read_u32, %function
r86_hard_read_u32:
    ldr w0, [x0]
    mov x1, #0
    ret

    .org r86_hard_read_u8 + 48
    .global r86_hard_read_u64
    .type r86_hard_read_u64, %function
r86_hard_read_u64:
    ldr x0, [x0]
    mov x1, #0
    ret

    .org r86_hard_read_u8 + 64
    .global r86_hard_read_abort_fixup
    .type r86_hard_read_abort_fixup, %function
r86_hard_read_abort_fixup:
    ret
"#
);

#[cfg(target_arch = "aarch64")]
#[unsafe(no_mangle)]
extern "C" fn r86_monitor_exception_panic(esr: u64, far: u64, elr: u64, sp: u64) -> ! {
    monitor_panic_halt(esr, far, elr, sp)
}

#[cfg(target_arch = "aarch64")]
pub fn monitor_panic_halt(esr: u64, far: u64, elr: u64, sp: u64) -> ! {
    publish_exception_panic_blocks(esr, far, elr, sp);
    let mailbox_addr = EXCEPTION_PANIC_MAILBOX.load(Ordering::Acquire);
    let doorbell_addr = EXCEPTION_PANIC_DOORBELL.load(Ordering::Acquire);
    if mailbox_addr != 0 && doorbell_addr != 0 {
        unsafe {
            let mailbox = core::slice::from_raw_parts_mut(
                mailbox_addr as *mut u8,
                core::mem::size_of::<crate::monitor::abi::MailboxV1>(),
            );
            let seq = read_u32(mailbox, MAILBOX_REQ_SEQ_OFFSET)
                .map(|value| value.wrapping_add(1).max(1))
                .unwrap_or(1);
            let request = MonitorRequestV1 {
                kind: MonitorRequestKindV1::Panic as u32,
                size: core::mem::size_of::<MonitorRequestV1>() as u32,
                seq,
                flags: 0,
                a: esr,
                b: far,
                c: elr,
                d: sp,
            };
            if write_request(mailbox, &request).is_ok() {
                compiler_fence(Ordering::Release);
                if write_u32_volatile(mailbox, MAILBOX_REQ_SEQ_OFFSET, seq).is_ok() {
                    fence(Ordering::Release);
                    clean_request_to_host(mailbox);
                    let value = DOORBELL_MAGIC;
                    core::arch::asm!(
                        "str {value:w}, [{doorbell}]",
                        doorbell = in(reg) doorbell_addr,
                        value = in(reg) value,
                        options(nostack, preserves_flags)
                    );
                }
            }
        }
    }
    loop {
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(target_arch = "aarch64")]
fn publish_exception_panic_blocks(esr: u64, far: u64, elr: u64, sp: u64) {
    let depth = EXCEPTION_PANIC_DEPTH.fetch_add(1, Ordering::AcqRel) + 1;
    let panic_addr = EXCEPTION_PANIC_BLOCK.load(Ordering::Acquire);
    if panic_addr != 0 {
        let mut block = crate::monitor::abi::debug::PanicBlockV1::new();
        block.panic_code = crate::monitor::abi::debug::PanicCodeV1::SyncException as u32;
        block.in_panic_depth = depth as u32;
        block.esr = esr;
        block.far = far;
        block.elr = elr;
        block.sp = sp;
        unsafe {
            (panic_addr as *mut crate::monitor::abi::debug::PanicBlockV1).write_volatile(block);
        }
        fence(Ordering::Release);
    }
    let debug_addr = EXCEPTION_PANIC_DEBUG_STATE.load(Ordering::Acquire);
    if debug_addr != 0 {
        unsafe {
            let ptr = debug_addr as *mut DebugStateBlockV1;
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).phase),
                DebugPhaseV1::Panic as u32,
            );
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).panic_flag), 2);
        }
        fence(Ordering::Release);
    }
}

#[cfg(not(target_arch = "aarch64"))]
pub fn monitor_panic_halt(_esr: u64, _far: u64, _elr: u64, _sp: u64) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[repr(C)]
#[derive(Clone, Copy)]
struct NativeThunkRead {
    value: u64,
    status: u64,
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
unsafe extern "C" {
    fn r86_hard_read_u8(va: u64) -> NativeThunkRead;
    fn r86_hard_read_u16(va: u64) -> NativeThunkRead;
    fn r86_hard_read_u32(va: u64) -> NativeThunkRead;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MonitorError {
    Abi(AbiError),
    MailboxTooSmall {
        len: usize,
    },
    BadMailboxVersion {
        version: u16,
    },
    BadMailboxSize {
        size: u16,
    },
    BadPacket {
        seq: u32,
    },
    Unsupported {
        seq: u32,
        kind: u32,
        a: u64,
        b: u64,
        c: u64,
    },
    Fault {
        seq: u32,
        value: u64,
    },
    ResponseTimeout {
        seq: u32,
    },
    ResponseSequenceMismatch {
        expected: u32,
        got: u32,
    },
    ResponseSizeMismatch {
        size: u32,
    },
    SerialPacketTooLong {
        len: usize,
    },
    RingFull,
    RingEmpty,
    CommandSizeMismatch {
        size: u32,
    },
    CommandSequenceMismatch {
        expected: u32,
        got: u32,
    },
    DoorbellFailed,
    Stage1UnsupportedLayout,
    Stage1UnsupportedRegion,
    Stage1MisalignedMapping {
        base: u64,
        len: u64,
    },
    ShadowStage1BadWindow,
    ShadowStage1PoolFull,
}

impl From<AbiError> for MonitorError {
    fn from(value: AbiError) -> Self {
        Self::Abi(value)
    }
}

#[repr(C, align(4096))]
pub struct MonitorStage1Tables {
    pub l1: [u64; STAGE1_TABLE_ENTRIES],
    pub low_l2: [u64; STAGE1_TABLE_ENTRIES],
    pub low_l3: [u64; STAGE1_TABLE_ENTRIES],
    pub monitor_l2: [u64; STAGE1_TABLE_ENTRIES],
    pub shared_l2: [u64; STAGE1_TABLE_ENTRIES],
    pub doorbell_l2: [u64; STAGE1_TABLE_ENTRIES],
}

impl MonitorStage1Tables {
    pub const fn new() -> Self {
        Self {
            l1: [0; STAGE1_TABLE_ENTRIES],
            low_l2: [0; STAGE1_TABLE_ENTRIES],
            low_l3: [0; STAGE1_TABLE_ENTRIES],
            monitor_l2: [0; STAGE1_TABLE_ENTRIES],
            shared_l2: [0; STAGE1_TABLE_ENTRIES],
            doorbell_l2: [0; STAGE1_TABLE_ENTRIES],
        }
    }

    pub fn clear(&mut self) {
        self.l1.fill(0);
        self.low_l2.fill(0);
        self.low_l3.fill(0);
        self.monitor_l2.fill(0);
        self.shared_l2.fill(0);
        self.doorbell_l2.fill(0);
    }

    pub const fn byte_len(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl Default for MonitorStage1Tables {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C, align(4096))]
#[derive(Clone, Copy)]
struct ShadowTables {
    root: [u64; STAGE1_TABLE_ENTRIES],
    supervisor_l2: [[u64; STAGE1_TABLE_ENTRIES]; 4],
    user_l2: [[u64; STAGE1_TABLE_ENTRIES]; 4],
}
impl ShadowTables {
    const EMPTY: Self = Self {
        root: [0; STAGE1_TABLE_ENTRIES],
        supervisor_l2: [[0; STAGE1_TABLE_ENTRIES]; 4],
        user_l2: [[0; STAGE1_TABLE_ENTRIES]; 4],
    };
    fn clear(&mut self) {
        for table in self.supervisor_l2.iter_mut().chain(self.user_l2.iter_mut()) {
            table.fill(0);
        }
    }
}

#[repr(C, align(4096))]
pub struct MonitorShadowStage1Tables<const L3_TABLES: usize> {
    contexts: [ShadowTables; RESIDENT_CONTEXTS],
    l3_pool: [[u64; STAGE1_TABLE_ENTRIES]; L3_TABLES],
    // Each live L2 entry records its owning ASID slot and parent location.
    // A block reserves a slot without using its L3 storage. A context flush
    // clears these links instead of 32 KiB of mostly empty L2s.
    links: [u16; L3_TABLES],
    sources: [TableSource; L3_TABLES],
    present: [u16; L3_TABLES],
    pending_tlbi: u8,
    // Few leaf removals should not discard unrelated hardware translations.
    // Count > 8 requests the existing whole-ASID flush. Parent retirement and
    // overflow use that path; table storage is never reused before completion.
    pending_pages: [[u64; 8]; RESIDENT_CONTEXTS],
    pending_page_count: [u8; RESIDENT_CONTEXTS],
    next_l3: usize,
    active: usize,
}

impl<const L3_TABLES: usize> MonitorShadowStage1Tables<L3_TABLES> {
    pub const fn new() -> Self {
        Self {
            contexts: [ShadowTables::EMPTY; RESIDENT_CONTEXTS],
            l3_pool: [[0; STAGE1_TABLE_ENTRIES]; L3_TABLES],
            links: [0; L3_TABLES],
            sources: [TableSource::EMPTY; L3_TABLES],
            present: [0; L3_TABLES],
            pending_tlbi: 0,
            pending_pages: [[0; 8]; RESIDENT_CONTEXTS],
            pending_page_count: [0; RESIDENT_CONTEXTS],
            next_l3: 0,
            active: 0,
        }
    }
    pub fn clear(&mut self) {
        for context in &mut self.contexts {
            context.clear();
        }
        self.links.fill(0);
        self.pending_tlbi = 0;
        self.pending_page_count.fill(0);
        self.next_l3 = 0;
    }
    pub const fn byte_len(&self) -> usize {
        core::mem::size_of::<Self>()
    }
    fn asid(&self) -> u16 {
        self.active as u16 + 1
    }
    fn activate(&mut self, index: usize) {
        self.active = index;
        select_stage1_root(table_pa(&self.contexts[index].root), self.asid());
    }
    fn l2_table_mut(
        &mut self,
        window: ShadowWindow,
        slot: usize,
    ) -> &mut [u64; STAGE1_TABLE_ENTRIES] {
        match window {
            ShadowWindow::Supervisor => &mut self.contexts[self.active].supervisor_l2[slot],
            ShadowWindow::User => &mut self.contexts[self.active].user_l2[slot],
        }
    }
    fn allocate_slot(&mut self, parent: u16) -> Result<usize, MonitorError> {
        let index = (self.next_l3..L3_TABLES)
            .chain(0..self.next_l3)
            .find(|&index| self.links[index] == 0)
            .ok_or(MonitorError::ShadowStage1PoolFull)?;
        self.next_l3 = index + 1;
        self.links[index] = ((self.active as u16 + 1) << 12) | parent;
        self.sources[index] = TableSource::EMPTY;
        self.present[index] = 0;
        Ok(index)
    }
    fn allocate_l3(&mut self, parent: u16) -> Result<u64, MonitorError> {
        let index = self.allocate_slot(parent)?;
        // An old table is zeroed before its new parent link becomes visible.
        self.l3_pool[index].fill(0);
        Ok(table_pa(&self.l3_pool[index]))
    }
    pub fn install_leaf(
        &mut self,
        window: ShadowWindow,
        linear: LinearAddr,
        phys: PhysAddr,
    ) -> Result<bool, MonitorError> {
        self.install_data_page(
            window,
            linear,
            abi::X86_RAM_IPA_BASE + (phys.0 & !(PAGE_SIZE_4K - 1)),
            false,
        )
    }
    /// A complete, stable, side-effect-free backing page, approved by the bus.
    /// Writes additionally require completed x86 Dirty handling and exclusion
    /// of all watched code/table frames. Device backing remains read-only.
    fn install_data_page(
        &mut self,
        window: ShadowWindow,
        linear: LinearAddr,
        ipa: u64,
        write: bool,
    ) -> Result<bool, MonitorError> {
        assert_eq!(ipa & (PAGE_SIZE_4K - 1), 0, "unaligned native backing");
        let va = shadow_stage1_va(window, linear);
        let slot = (linear.0 >> 30) as usize;
        let l2_index = l2_index(va)?;
        let window_index = usize::from(window == ShadowWindow::User);
        let parent = ((window_index * 4 + slot) * STAGE1_TABLE_ENTRIES + l2_index) as u16;
        let descriptor = shadow_page_descriptor(ipa) & if write { !(1 << 7) } else { u64::MAX };
        let previous = self.l2_table_mut(window, slot)[l2_index];
        if previous & 3 == 1 {
            if (previous | 2 | (u64::from(linear.0) & 0x1f_f000)) == descriptor {
                return Ok(false);
            }
            // Split a block only after its old translation is gone. Its link
            // already owns a slot whose L3 storage can now hold the new page.
            let link = ((self.active as u16 + 1) << 12) | parent;
            let index = self
                .links
                .iter()
                .position(|&entry| entry == link)
                .ok_or(MonitorError::ShadowStage1BadWindow)?;
            self.l2_table_mut(window, slot)[l2_index] = 0;
            flush_stage1_tlb_asid(self.asid());
            self.l3_pool[index].fill(0);
            self.present[index] = 0;
            self.l2_table_mut(window, slot)[l2_index] =
                table_descriptor(table_pa(&self.l3_pool[index]));
            stage1_publish();
        } else if previous == 0 {
            let pa = self.allocate_l3(parent)?;
            self.l2_table_mut(window, slot)[l2_index] = table_descriptor(pa);
            stage1_publish();
        }
        let pa = self.l2_table_mut(window, slot)[l2_index] & 0x0000_ffff_ffff_f000;
        let index = self
            .l3_pool_index(pa)
            .ok_or(MonitorError::ShadowStage1BadWindow)?;
        let entry = l3_index(va)?;
        if self.l3_pool[index][entry] == descriptor
            || (!write && self.l3_pool[index][entry] | (1 << 7) == descriptor)
        {
            return Ok(false);
        }
        if self.l3_pool[index][entry] != 0 {
            self.l3_pool[index][entry] = 0;
            flush_stage1_tlb_va(va, self.asid());
        } else {
            self.present[index] += 1;
        }
        self.l3_pool[index][entry] = descriptor;
        stage1_publish();
        Ok(true)
    }
    /// The caller has approved the entire 2 MiB physical span as ordinary RAM
    /// using a completed x86 large-leaf walk, including permission and A bits.
    pub fn install_block(
        &mut self,
        window: ShadowWindow,
        linear: LinearAddr,
        phys: PhysAddr,
    ) -> Result<bool, MonitorError> {
        debug_assert_eq!(u64::from(linear.0) & 0x1f_ffff, phys.0 & 0x1f_ffff);
        let va = shadow_stage1_va(window, linear);
        let slot = (linear.0 >> 30) as usize;
        let entry = l2_index(va)?;
        let descriptor = shadow_page_descriptor(abi::X86_RAM_IPA_BASE + (phys.0 & !0x1f_ffff)) & !2;
        let previous = self.l2_table_mut(window, slot)[entry];
        if previous == descriptor {
            return Ok(false);
        }
        let window_index = usize::from(window == ShadowWindow::User);
        let parent = ((window_index * 4 + slot) * STAGE1_TABLE_ENTRIES + entry) as u16;
        let index = if previous == 0 {
            self.allocate_slot(parent)?
        } else {
            // Break before make, including every old 4 KiB entry when replacing
            // an L3 table. Its parent-registry slot remains owned by this entry.
            self.l2_table_mut(window, slot)[entry] = 0;
            flush_stage1_tlb_asid(self.asid());
            let link = ((self.active as u16 + 1) << 12) | parent;
            self.links
                .iter()
                .position(|&value| value == link)
                .expect("live block parent")
        };
        self.present[index] = 1;
        self.l2_table_mut(window, slot)[entry] = descriptor;
        stage1_publish();
        Ok(true)
    }
    pub fn flush_active(&mut self) {
        let owner = self.active as u16 + 1;
        let tables = &mut self.contexts[self.active];
        for link in &mut self.links {
            if *link >> 12 == owner {
                let l2 = if *link & 0x800 == 0 {
                    &mut tables.supervisor_l2
                } else {
                    &mut tables.user_l2
                };
                l2[usize::from((*link >> 9) & 3)][usize::from(*link & 511)] = 0;
                *link = 0;
            }
        }
        // No allocation can occur until this synchronous flush returns.
        // Finish invalidation of the broken links before recycling their L3s.
        flush_stage1_tlb_asid(self.asid());
        self.pending_tlbi &= !(1 << self.active);
        self.pending_page_count[self.active] = 0;
        self.next_l3 = 0;
    }

    fn parent_mut(&mut self, link: u16) -> &mut u64 {
        let tables = &mut self.contexts[usize::from((link >> 12) - 1)];
        let l2 = if link & 0x800 == 0 {
            &mut tables.supervisor_l2
        } else {
            &mut tables.user_l2
        };
        &mut l2[usize::from((link >> 9) & 3)][usize::from(link & 511)]
    }

    fn index_for(&self, window: ShadowWindow, linear: LinearAddr) -> Option<usize> {
        let slot = (linear.0 >> 30) as usize;
        let entry = ((linear.0 >> 21) & 511) as usize;
        let tables = &self.contexts[self.active];
        let descriptor = match window {
            ShadowWindow::Supervisor => tables.supervisor_l2[slot][entry],
            ShadowWindow::User => tables.user_l2[slot][entry],
        };
        if descriptor & 3 == 1 {
            let parent =
                ((usize::from(window == ShadowWindow::User) * 4 + slot) * 512 + entry) as u16;
            let link = ((self.active as u16 + 1) << 12) | parent;
            self.links.iter().position(|&value| value == link)
        } else if descriptor != 0 {
            self.l3_pool_index(descriptor & 0x0000_ffff_ffff_f000)
        } else {
            None
        }
    }

    fn record_source(&mut self, window: ShadowWindow, linear: LinearAddr, source: TableSource) {
        let index = self
            .index_for(window, linear)
            .expect("installed shadow source");
        self.sources[index] = source;
    }

    fn retire_parent(&mut self, index: usize) {
        let link = self.links[index];
        *self.parent_mut(link) = 0;
        self.present[index] = 0;
        // Keep its registry slot occupied until this ASID has completed TLBI.
        self.pending_tlbi |= 1 << ((link >> 12) - 1);
        self.pending_page_count[usize::from((link >> 12) - 1)] = 9;
    }

    fn retire_leaf(&mut self, index: usize, entry: usize) -> bool {
        if self.l3_pool[index][entry] == 0 {
            return false;
        }
        self.l3_pool[index][entry] = 0;
        self.present[index] -= 1;
        let link = self.links[index];
        let owner = usize::from((link >> 12) - 1);
        self.pending_tlbi |= 1 << owner;
        let count = &mut self.pending_page_count[owner];
        if *count < 8 {
            let window = if link & 0x800 == 0 {
                ShadowWindow::Supervisor
            } else {
                ShadowWindow::User
            };
            let linear = (u32::from(link & 0x7ff) << 21) | ((entry as u32) << 12);
            self.pending_pages[owner][usize::from(*count)] =
                shadow_stage1_va(window, LinearAddr(linear));
            *count += 1;
        } else {
            *count = 9;
        }
        if self.present[index] == 0 {
            self.retire_parent(index);
        }
        true
    }

    /// The physical store has not happened yet. Remove its dependent leaves
    /// now; no guest access can resume until the active dirty boundary completes
    /// TLBI. Inactive ASIDs keep their reserved slots until selected and flushed.
    #[inline(never)]
    fn invalidate_write(&mut self, write: PhysWrite, users: u8) {
        let end = write.addr.0.checked_add(u64::from(write.len) - 1);
        for index in 0..L3_TABLES {
            let link = self.links[index];
            if link == 0 || users & (1 << ((link >> 12) - 1)) == 0 || *self.parent_mut(link) == 0 {
                continue;
            }
            let source = self.sources[index];
            let Some(end) = end else {
                self.retire_parent(index);
                continue;
            };
            let overlaps =
                |base: u64, len: u64| base != u64::MAX && write.addr.0 < base + len && end >= base;
            if source == TableSource::EMPTY
                || source
                    .upper
                    .iter()
                    .any(|&base| overlaps(base, u64::from(source.entry_bytes)))
            {
                self.retire_parent(index);
            } else if overlaps(source.leaves, 512 * u64::from(source.entry_bytes)) {
                let width = u64::from(source.entry_bytes);
                let first = (write.addr.0.saturating_sub(source.leaves) / width) as usize;
                let last = ((end - source.leaves) / width).min(511) as usize;
                for entry in first..=last {
                    self.retire_leaf(index, entry);
                }
            }
        }
    }

    fn finish_invalidations(&mut self) -> bool {
        let bit = 1 << self.active;
        if self.pending_tlbi & bit == 0 {
            return false;
        }
        let count = core::mem::take(&mut self.pending_page_count[self.active]);
        if count <= 8 {
            flush_stage1_tlb_pages(
                &self.pending_pages[self.active][..usize::from(count)],
                self.asid(),
            );
        } else {
            flush_stage1_tlb_asid(self.asid());
        }
        self.pending_tlbi &= !bit;
        let owner = self.active as u16 + 1;
        for index in 0..L3_TABLES {
            let link = self.links[index];
            if link >> 12 == owner && *self.parent_mut(link) == 0 {
                self.links[index] = 0;
            }
        }
        true
    }

    /// INVLPG retires the addressed leaf, including all fragments of a large
    /// guest page. The walker-recorded page size survives in both read windows.
    pub fn invalidate_linear(&mut self, linear: LinearAddr) -> u64 {
        let base4m = linear.0 & !0x3f_ffff;
        let mut shift = 12;
        for window in [ShadowWindow::Supervisor, ShadowWindow::User] {
            for half in [base4m, base4m | 0x20_0000] {
                if let Some(index) = self.index_for(window, LinearAddr(half)) {
                    let size = self.sources[index].page_shift;
                    if size == 0 || size == 22 {
                        shift = 22;
                    } else if half == linear.0 & !0x1f_ffff {
                        shift = shift.max(size);
                    }
                }
            }
        }
        let mut removed = 0;
        for window in [ShadowWindow::Supervisor, ShadowWindow::User] {
            if shift == 12 {
                if let Some(index) = self.index_for(window, linear) {
                    removed +=
                        u64::from(self.retire_leaf(index, ((linear.0 >> 12) & 511) as usize));
                }
            } else {
                let base = linear.0 & !((1 << shift) - 1);
                for offset in (0..1 << shift).step_by(0x20_0000) {
                    if let Some(index) = self.index_for(window, LinearAddr(base + offset)) {
                        self.retire_parent(index);
                        removed += 1;
                    }
                }
            }
        }
        self.finish_invalidations();
        removed
    }

    pub fn flush(&mut self) {
        self.clear();
        flush_stage1_tlb();
    }
    pub fn descriptor_for(&self, window: ShadowWindow, linear: LinearAddr) -> Option<u64> {
        let va = shadow_stage1_va(window, linear);
        let slot = (linear.0 >> 30) as usize;
        let tables = &self.contexts[self.active];
        let l2 = match window {
            ShadowWindow::Supervisor => &tables.supervisor_l2[slot],
            ShadowWindow::User => &tables.user_l2[slot],
        };
        let descriptor = l2[l2_index(va).ok()?];
        if descriptor & 3 == 1 {
            // Expose a page-sized view to diagnostics regardless of hardware
            // leaf size. The offset stays inside the approved block.
            return Some(descriptor | 2 | (u64::from(linear.0) & 0x1f_f000));
        }
        let pa = descriptor & 0x0000_ffff_ffff_f000;
        let index = self.l3_pool_index(pa)?;
        let entry = self.l3_pool[index][l3_index(va).ok()?];
        (entry != 0).then_some(entry)
    }
    fn l3_pool_index(&self, pa: u64) -> Option<usize> {
        let offset = pa.checked_sub(self.l3_pool.as_ptr() as u64)?;
        if offset & (PAGE_SIZE_4K - 1) != 0 {
            return None;
        }
        let index = (offset / PAGE_SIZE_4K) as usize;
        if index >= L3_TABLES {
            return None;
        }
        // Every nonzero L2 link belongs to this context: allocation sets its
        // owner, and flush_active breaks all owner links before pool reuse.
        debug_assert_eq!(self.links[index] >> 12, self.active as u16 + 1);
        Some(index)
    }
}
impl<const L3_TABLES: usize> Default for MonitorShadowStage1Tables<L3_TABLES> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum X86MonitorError {
    Monitor(MonitorError),
    Mem(MemError),
    Step {
        steps: u32,
        eip: u32,
        error: StepError,
    },
    StepLimit {
        max_steps: u32,
        eip: u32,
    },
    AddressTooLarge {
        addr: u64,
    },
    RamOutOfRange {
        addr: PhysAddr,
        len: u64,
        ram_len: u64,
    },
    ProgramOutOfRange {
        base: PhysAddr,
        len: usize,
        ram_len: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct X86MonitorErrorReport {
    pub kind: u32,
    pub eip: u32,
    pub arg0: u32,
    pub arg1: u32,
    pub arg2: u32,
}

impl X86MonitorErrorReport {
    pub const NONE: Self = Self {
        kind: 0,
        eip: 0,
        arg0: 0,
        arg1: 0,
        arg2: 0,
    };
}

impl From<MonitorError> for X86MonitorError {
    fn from(value: MonitorError) -> Self {
        Self::Monitor(value)
    }
}

impl From<MemError> for X86MonitorError {
    fn from(value: MemError) -> Self {
        Self::Mem(value)
    }
}

pub fn x86_monitor_error_report(error: X86MonitorError) -> X86MonitorErrorReport {
    match error {
        X86MonitorError::Monitor(err) => {
            let (eip, arg0, arg1, arg2) = monitor_error_args(err);
            X86MonitorErrorReport {
                kind: 0x1000 | monitor_error_code(err),
                eip,
                arg0,
                arg1,
                arg2,
            }
        }
        X86MonitorError::Mem(err) => X86MonitorErrorReport {
            kind: 0x2000 | mem_error_code(err),
            arg0: mem_error_arg(err),
            ..X86MonitorErrorReport::NONE
        },
        X86MonitorError::Step {
            steps: _,
            eip,
            error,
        } => {
            let (kind, arg0, arg1) = step_error_code_args(error);
            X86MonitorErrorReport {
                kind,
                eip,
                arg0,
                arg1,
                ..X86MonitorErrorReport::NONE
            }
        }
        X86MonitorError::StepLimit { max_steps, eip } => X86MonitorErrorReport {
            kind: 0x4000,
            eip,
            arg0: max_steps,
            arg1: 0,
            ..X86MonitorErrorReport::NONE
        },
        X86MonitorError::AddressTooLarge { addr } => X86MonitorErrorReport {
            kind: 0x4001,
            eip: 0,
            arg0: addr as u32,
            arg1: (addr >> 32) as u32,
            ..X86MonitorErrorReport::NONE
        },
        X86MonitorError::RamOutOfRange { addr, len, ram_len } => X86MonitorErrorReport {
            kind: 0x4002,
            eip: len as u32,
            arg0: addr.0 as u32,
            arg1: ram_len as u32,
            ..X86MonitorErrorReport::NONE
        },
        X86MonitorError::ProgramOutOfRange { base, len, ram_len } => X86MonitorErrorReport {
            kind: 0x4003,
            eip: len as u32,
            arg0: base.0 as u32,
            arg1: ram_len as u32,
            ..X86MonitorErrorReport::NONE
        },
    }
}

pub fn send_x86_monitor_error_report<D: Doorbell>(
    client: &mut MailboxClient<'_>,
    doorbell: &mut D,
    error: X86MonitorError,
) {
    let report = x86_monitor_error_report(error);
    send_error_report_field(client, doorbell, b"R86_ERR_K=", report.kind);
    send_error_report_field(client, doorbell, b"R86_ERR_E=", report.eip);
    send_error_report_field(client, doorbell, b"R86_ERR_A=", report.arg0);
    send_error_report_field(client, doorbell, b"R86_ERR_B=", report.arg1);
    send_error_report_field(client, doorbell, b"R86_ERR_C=", report.arg2);
}

fn send_error_report_field<D: Doorbell>(
    client: &mut MailboxClient<'_>,
    doorbell: &mut D,
    prefix: &[u8; 10],
    value: u32,
) {
    let mut bytes = [0_u8; 19];
    bytes[..10].copy_from_slice(prefix);
    write_hex_u32(&mut bytes[10..18], value);
    bytes[18] = b'\n';
    if let Ok(packet) = SerialFlushPacket::from_bytes(&bytes) {
        let _ = client.serial_flush(doorbell, packet);
    }
}

fn write_hex_u32(out: &mut [u8], value: u32) {
    let mut index = 0;
    while index < 8 && index < out.len() {
        let shift = (7 - index) * 4;
        let nibble = ((value >> shift) & 0x0f) as u8;
        out[index] = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + (nibble - 10)
        };
        index += 1;
    }
}

fn monitor_error_code(error: MonitorError) -> u32 {
    match error {
        MonitorError::Abi(_) => 0x001,
        MonitorError::MailboxTooSmall { .. } => 0x002,
        MonitorError::BadMailboxVersion { .. } => 0x003,
        MonitorError::BadMailboxSize { .. } => 0x004,
        MonitorError::BadPacket { .. } => 0x005,
        MonitorError::Unsupported { .. } => 0x006,
        MonitorError::Fault { .. } => 0x007,
        MonitorError::ResponseTimeout { .. } => 0x008,
        MonitorError::ResponseSequenceMismatch { .. } => 0x009,
        MonitorError::ResponseSizeMismatch { .. } => 0x00a,
        MonitorError::SerialPacketTooLong { .. } => 0x00b,
        MonitorError::RingFull => 0x00c,
        MonitorError::RingEmpty => 0x00d,
        MonitorError::CommandSizeMismatch { .. } => 0x00e,
        MonitorError::CommandSequenceMismatch { .. } => 0x00f,
        MonitorError::DoorbellFailed => 0x010,
        MonitorError::Stage1UnsupportedLayout => 0x011,
        MonitorError::Stage1UnsupportedRegion => 0x012,
        MonitorError::Stage1MisalignedMapping { .. } => 0x013,
        MonitorError::ShadowStage1BadWindow => 0x014,
        MonitorError::ShadowStage1PoolFull => 0x015,
    }
}

fn monitor_error_args(error: MonitorError) -> (u32, u32, u32, u32) {
    match error {
        MonitorError::BadPacket { seq }
        | MonitorError::ResponseTimeout { seq }
        | MonitorError::ResponseSizeMismatch { size: seq }
        | MonitorError::CommandSizeMismatch { size: seq } => (seq, 0, 0, 0),
        MonitorError::Unsupported { seq, kind, a, b, c } => {
            let _ = c;
            (seq, kind, a as u32, b as u32)
        }
        MonitorError::Fault { seq, value } => (seq, value as u32, (value >> 32) as u32, 0),
        MonitorError::MailboxTooSmall { len } => (0, len as u32, 0, 0),
        MonitorError::BadMailboxVersion { version } => (0, u32::from(version), 0, 0),
        MonitorError::BadMailboxSize { size } => (0, u32::from(size), 0, 0),
        MonitorError::ResponseSequenceMismatch { expected, got }
        | MonitorError::CommandSequenceMismatch { expected, got } => (0, expected, got, 0),
        MonitorError::SerialPacketTooLong { len } => (0, len as u32, 0, 0),
        MonitorError::Stage1MisalignedMapping { base, len } => (0, base as u32, len as u32, 0),
        MonitorError::Abi(_)
        | MonitorError::RingFull
        | MonitorError::RingEmpty
        | MonitorError::DoorbellFailed
        | MonitorError::Stage1UnsupportedLayout
        | MonitorError::Stage1UnsupportedRegion
        | MonitorError::ShadowStage1BadWindow
        | MonitorError::ShadowStage1PoolFull => (0, 0, 0, 0),
    }
}

fn mem_error_code(error: MemError) -> u32 {
    match error {
        MemError::OutOfRange { .. } => 0x001,
        MemError::Mmio { .. } => 0x002,
        MemError::IoUnhandled { .. } => 0x003,
    }
}

fn mem_error_arg(error: MemError) -> u32 {
    match error {
        MemError::OutOfRange { addr } | MemError::Mmio { addr } => addr.0 as u32,
        MemError::IoUnhandled { port } => u32::from(port),
    }
}

fn step_error_code_args(error: StepError) -> (u32, u32, u32) {
    if let Some(err) = error.fault_mem_error() {
        return (0x3000 | mem_error_code(err), mem_error_arg(err), 0);
    }
    if let Some(err) = error.direct_mem_error() {
        return (0x3030 | mem_error_code(err), mem_error_arg(err), 0);
    }
    match error {
        StepError::PageFault(page) => (0x3010, page.linear.0, page.code),
        StepError::UnsupportedOpcode { eip, opcode } => (0x3020, eip, u32::from(opcode)),
        StepError::UnsupportedOpcode2 { eip, opcode } => (0x3021, eip, u32::from(opcode)),
        StepError::UnsupportedModRm { eip, opcode, modrm } => {
            (0x3022, eip, (u32::from(opcode) << 8) | u32::from(modrm))
        }
        StepError::InvalidOpcode { eip } => (0x3023, eip, 0),
        StepError::SoftwareInterrupt {
            vector, return_eip, ..
        } => (0x3024, return_eip, u32::from(vector)),
        StepError::DivideError { eip } => (0x3026, eip, 0),
        StepError::DeviceNotAvailable { eip } => (0x3027, eip, 0),
        StepError::BoundRangeExceeded { eip } => (0x3028, eip, 0),
        StepError::GeneralProtection { eip, code } => (0x3029, eip, code),
        StepError::InvalidTss { eip, code } => (0x302a, eip, code),
        StepError::StackFault { eip, code } => (0x302b, eip, code),
        StepError::SegmentNotPresent { eip, code } => (0x302c, eip, code),
        StepError::TripleFault { vector, code, .. } => (0x302d, u32::from(vector), code),
        StepError::TaskSwitchPageFault { page, .. } => (0x302e, page.linear.0, page.code),
        StepError::TaskSwitchFault { eip, vector, code } => {
            // Keep the existing architectural fault codes on the monitor
            // wire. The address already identifies the incoming task context.
            let kind = match vector {
                10 => 0x302a,
                11 => 0x302c,
                12 => 0x302b,
                13 => 0x3029,
                _ => unreachable!("invalid task qualification fault"),
            };
            (kind, eip, code)
        }
        StepError::TaskSwitchTrap { eip } => (0x302f, eip, 0),
        StepError::FaultOutOfRange { .. }
        | StepError::FaultMmio { .. }
        | StepError::FaultIoUnhandled { .. }
        | StepError::MemOutOfRange { .. }
        | StepError::MemMmio { .. }
        | StepError::MemIoUnhandled { .. } => unreachable!(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MonitorBoot {
    pub boot_block_ipa: u64,
    pub ram_base: u64,
    pub ram_size: u64,
    pub regions: RegionTableV1,
    pub cpu_state_ipa: u64,
    pub mailbox_ipa: u64,
    pub debug_state_ipa: u64,
    pub panic_block_ipa: u64,
    pub trace_ipa: u64,
    pub timer_poll_interval: u64,
    pub step_limit: u64,
    pub trace_mode: u32,
    pub feature_bits: u64,
}

impl MonitorBoot {
    pub fn from_boot_block(
        boot_block_ipa: u64,
        boot: &BootBlockV1,
        ipa_bits: u8,
    ) -> Result<Self, MonitorError> {
        boot.validate(ipa_bits)?;
        let shared_base = boot.layout.shared_base;
        Ok(Self {
            boot_block_ipa,
            ram_base: boot.layout.ram_base,
            ram_size: boot.layout.ram_size,
            regions: boot.regions,
            cpu_state_ipa: checked_ipa_add(shared_base, boot.initial_cpu_state_offset)?,
            mailbox_ipa: checked_ipa_add(shared_base, boot.mailbox_offset)?,
            debug_state_ipa: checked_ipa_add(shared_base, boot.debug_state_offset())?,
            panic_block_ipa: checked_ipa_add(shared_base, boot.panic_block_offset())?,
            trace_ipa: checked_ipa_add(boot.layout.trace_base, boot.trace_offset)?,
            timer_poll_interval: boot.timer_poll_interval,
            step_limit: boot.step_limit,
            trace_mode: boot.trace_mode,
            feature_bits: boot.feature_bits,
        })
    }
}

pub fn build_monitor_stage1_tables(
    tables: &mut MonitorStage1Tables,
    layout: IpaLayoutV1,
    regions: RegionTableV1,
) -> Result<(), MonitorError> {
    layout.validate(MIN_ARM_IPA_BITS)?;
    regions.validate()?;
    tables.clear();

    let low_l2_pa = table_pa(&tables.low_l2);
    let low_l3_pa = table_pa(&tables.low_l3);
    let monitor_l2_pa = table_pa(&tables.monitor_l2);
    let shared_l2_pa = table_pa(&tables.shared_l2);
    let doorbell_l2_pa = table_pa(&tables.doorbell_l2);

    tables.l1[l1_index(layout.ram_base)?] = table_descriptor(low_l2_pa);
    tables.low_l2[l2_index(0)?] = table_descriptor(low_l3_pa);
    tables.l1[l1_index(layout.monitor_base)?] = table_descriptor(monitor_l2_pa);
    tables.l1[l1_index(layout.shared_base)?] = table_descriptor(shared_l2_pa);
    tables.l1[l1_index(layout.doorbell_ipa)?] = table_descriptor(doorbell_l2_pa);

    let mut index = 0;
    while index < regions.count as usize {
        let entry = regions.entries[index];
        if entry.kind()? == RegionKindV1::Ram {
            map_low_ram_range(tables, entry.base, entry.len)?;
        }
        index += 1;
    }

    map_l2_range(
        &mut tables.monitor_l2,
        layout.monitor_base,
        layout.monitor_size,
        normal_block_descriptor,
    )?;
    map_l2_range(
        &mut tables.shared_l2,
        layout.shared_base,
        layout.shared_size,
        normal_xn_block_descriptor,
    )?;
    // Shared debug/panic/trace pages are monitor-write and host-read while KVM_RUN
    // is active. They must stay Normal WB Inner-Shareable (attr index 0, SH=3),
    // never Device or Non-cacheable, or the debug seqlock can report stale data.
    map_l2_range(
        &mut tables.shared_l2,
        layout.trace_base,
        layout.trace_size,
        normal_xn_block_descriptor,
    )?;
    map_doorbell_block(&mut tables.doorbell_l2, layout.doorbell_ipa)?;
    Ok(())
}

pub fn attach_shadow_stage1_tables<const L3_TABLES: usize>(
    tables: &MonitorStage1Tables,
    shadow: &mut MonitorShadowStage1Tables<L3_TABLES>,
) -> Result<(), MonitorError> {
    for context in &mut shadow.contexts {
        context.root = tables.l1;
        for slot in 0..4 {
            context.root[l1_index(SHADOW_SUP_BASE + slot as u64 * STAGE1_L1_BLOCK_SIZE)?] =
                table_descriptor(table_pa(&context.supervisor_l2[slot]));
            context.root[l1_index(SHADOW_USR_BASE + slot as u64 * STAGE1_L1_BLOCK_SIZE)?] =
                table_descriptor(table_pa(&context.user_l2[slot]));
        }
    }
    Ok(())
}

/// Install the monitor-owned JIT aliases before enabling stage 1. All ASIDs
/// share this global mapping; x86 shadow windows can never address these bytes.
#[cfg(all(
    feature = "jit",
    feature = "machine",
    target_arch = "aarch64",
    target_os = "none"
))]
fn attach_jit_code_mapping<const L3_TABLES: usize>(
    tables: &mut MonitorStage1Tables,
    shadow: &mut MonitorShadowStage1Tables<L3_TABLES>,
) -> Result<(), MonitorError> {
    use crate::x86::jit::code::monitor as code;
    #[repr(C, align(4096))]
    struct Table([u64; STAGE1_TABLE_ENTRIES]);
    static mut ALIAS: Table = Table([0; STAGE1_TABLE_ENTRIES]);
    // SAFETY: single-vCPU boot, before MMU enable or any JIT invocation. The
    // static table and dedicated 2 MiB-aligned storage live for the entire VM.
    let alias = unsafe { &mut *core::ptr::addr_of_mut!(ALIAS) };
    let backing = code::backing() as u64;
    for offset in (0..code::STORAGE_BYTES).step_by(STAGE1_L2_BLOCK_SIZE as usize) {
        let pa = backing + offset as u64;
        let identity = &mut tables.monitor_l2[l2_index(pa)?];
        if *identity != normal_block_descriptor(pa) {
            return Err(MonitorError::Stage1UnsupportedLayout);
        }
        *identity = normal_xn_block_descriptor(pa);
        let va = code::EXECUTABLE_BASE as u64 + offset as u64;
        // AP=read-only EL1, UXN always; metadata also PXN.
        alias.0[l2_index(va)?] =
            block_descriptor(pa, 0, offset >= code::CODE_BYTES) | (1 << 7) | (1_u64 << 54);
    }
    let slot = l1_index(code::EXECUTABLE_BASE as u64)?;
    for context in &mut shadow.contexts {
        assert_eq!(
            context.root[slot], 0,
            "JIT alias overlaps an existing window"
        );
        context.root[slot] = table_descriptor(table_pa(&alias.0));
    }
    clean_to_poc(
        alias as *const Table as *const u8,
        core::mem::size_of::<Table>(),
    );
    // SAFETY: all roots now own both aliases with the required permissions.
    unsafe { code::mappings_ready() };
    Ok(())
}

/// Enables monitor identity mappings and the nonzero guest-RAM direct map.
///
/// # Safety
///
/// The caller must provide a valid, 4 KiB-aligned table block that remains live forever,
/// and must call this only while executing at an identity-mapped monitor IPA.
/// Guest RAM uses its nonzero ARM IPA base; ARM VA zero remains unmapped.
pub unsafe fn enable_identity_stage1_from_boot(
    tables: *mut MonitorStage1Tables,
    boot: &BootBlockV1,
    ipa_bits: u8,
) -> Result<(), MonitorError> {
    boot.validate(ipa_bits)?;
    let tables = unsafe { &mut *tables };
    build_monitor_stage1_tables(tables, boot.layout, boot.regions)?;
    clean_to_poc(
        tables as *const MonitorStage1Tables as *const u8,
        tables.byte_len(),
    );
    unsafe {
        enable_stage1_registers(tables.l1.as_ptr() as u64, STAGE1_TCR_EL1_34BIT);
    }
    Ok(())
}

/// Enables the monitor/direct mappings and the ASID-tagged shadow roots.
///
/// # Safety
///
/// The caller must provide valid, 4 KiB-aligned table blocks that remain live for
/// the monitor lifetime. Monitor code stays identity-mapped. Each root shares
/// those mappings and owns separate supervisor/user native-read windows.
pub unsafe fn enable_identity_stage1_with_shadow_from_boot<const L3_TABLES: usize>(
    tables: *mut MonitorStage1Tables,
    shadow: *mut MonitorShadowStage1Tables<L3_TABLES>,
    boot: &BootBlockV1,
    ipa_bits: u8,
) -> Result<(), MonitorError> {
    boot.validate(ipa_bits)?;
    let tables = unsafe { &mut *tables };
    let shadow = unsafe { &mut *shadow };
    shadow.clear();
    build_monitor_stage1_tables(tables, boot.layout, boot.regions)?;
    attach_shadow_stage1_tables(tables, shadow)?;
    #[cfg(all(
        feature = "jit",
        feature = "machine",
        target_arch = "aarch64",
        target_os = "none"
    ))]
    attach_jit_code_mapping(tables, shadow)?;
    clean_to_poc(
        tables as *const MonitorStage1Tables as *const u8,
        tables.byte_len(),
    );
    clean_to_poc(
        shadow as *const MonitorShadowStage1Tables<L3_TABLES> as *const u8,
        shadow.byte_len(),
    );
    unsafe {
        enable_stage1_registers(
            table_pa(&shadow.contexts[0].root) | (1 << 48),
            STAGE1_TCR_EL1_39BIT,
        );
    }
    Ok(())
}

fn table_pa(table: &[u64; STAGE1_TABLE_ENTRIES]) -> u64 {
    table.as_ptr() as usize as u64
}

fn map_low_ram_range(
    tables: &mut MonitorStage1Tables,
    base: u64,
    len: u64,
) -> Result<(), MonitorError> {
    let end = base
        .checked_add(len)
        .ok_or(MonitorError::Stage1UnsupportedRegion)?;
    if end > STAGE1_L1_BLOCK_SIZE {
        return Err(MonitorError::Stage1UnsupportedRegion);
    }
    if base < STAGE1_LOW_4K_WINDOW {
        let low_end = core::cmp::min(end, STAGE1_LOW_4K_WINDOW);
        map_l3_range(
            &mut tables.low_l3,
            abi::X86_RAM_IPA_BASE + base,
            low_end - base,
            normal_xn_page_descriptor,
        )?;
    }
    if end > STAGE1_LOW_4K_WINDOW {
        let block_base = core::cmp::max(base, STAGE1_LOW_4K_WINDOW);
        map_l2_range(
            &mut tables.low_l2,
            abi::X86_RAM_IPA_BASE + block_base,
            end - block_base,
            normal_xn_block_descriptor,
        )?;
    }
    Ok(())
}

fn map_l2_range(
    table: &mut [u64; STAGE1_TABLE_ENTRIES],
    base: u64,
    len: u64,
    descriptor: fn(u64) -> u64,
) -> Result<(), MonitorError> {
    if len == 0 {
        return Ok(());
    }
    if base & (STAGE1_L2_BLOCK_SIZE - 1) != 0 || len & (STAGE1_L2_BLOCK_SIZE - 1) != 0 {
        return Err(MonitorError::Stage1MisalignedMapping { base, len });
    }
    let mut addr = base;
    let end = base
        .checked_add(len)
        .ok_or(MonitorError::Stage1UnsupportedRegion)?;
    while addr < end {
        table[l2_index(addr)?] = descriptor(addr);
        addr += STAGE1_L2_BLOCK_SIZE;
    }
    Ok(())
}

fn map_l3_range(
    table: &mut [u64; STAGE1_TABLE_ENTRIES],
    base: u64,
    len: u64,
    descriptor: fn(u64) -> u64,
) -> Result<(), MonitorError> {
    if len == 0 {
        return Ok(());
    }
    if base & (PAGE_SIZE_4K - 1) != 0 || len & (PAGE_SIZE_4K - 1) != 0 {
        return Err(MonitorError::Stage1MisalignedMapping { base, len });
    }
    let mut addr = base;
    let end = base
        .checked_add(len)
        .ok_or(MonitorError::Stage1UnsupportedRegion)?;
    while addr < end {
        table[l3_index(addr)?] = descriptor(addr);
        addr += PAGE_SIZE_4K;
    }
    Ok(())
}

fn map_doorbell_block(
    table: &mut [u64; STAGE1_TABLE_ENTRIES],
    doorbell_ipa: u64,
) -> Result<(), MonitorError> {
    let block_base = doorbell_ipa & !(STAGE1_L2_BLOCK_SIZE - 1);
    table[l2_index(block_base)?] = device_xn_block_descriptor(block_base);
    Ok(())
}

fn l1_index(addr: u64) -> Result<usize, MonitorError> {
    if addr >= STAGE1_MAX_TTBR0_VA {
        return Err(MonitorError::Stage1UnsupportedLayout);
    }
    Ok(((addr >> 30) & 0x1ff) as usize)
}

fn l2_index(addr: u64) -> Result<usize, MonitorError> {
    if addr >= STAGE1_MAX_TTBR0_VA {
        return Err(MonitorError::Stage1UnsupportedLayout);
    }
    Ok(((addr >> 21) & 0x1ff) as usize)
}

fn l3_index(addr: u64) -> Result<usize, MonitorError> {
    if addr >= STAGE1_MAX_TTBR0_VA {
        return Err(MonitorError::Stage1UnsupportedLayout);
    }
    Ok(((addr >> 12) & 0x1ff) as usize)
}

fn table_descriptor(table_pa: u64) -> u64 {
    (table_pa & 0x0000_ffff_ffff_f000) | 0b11
}

fn normal_block_descriptor(output_addr: u64) -> u64 {
    block_descriptor(output_addr, 0, false)
}

fn normal_xn_block_descriptor(output_addr: u64) -> u64 {
    block_descriptor(output_addr, 0, true)
}

fn device_xn_block_descriptor(output_addr: u64) -> u64 {
    block_descriptor(output_addr, 1, true)
}

fn normal_xn_page_descriptor(output_addr: u64) -> u64 {
    page_descriptor(output_addr, 0, true)
}

fn shadow_page_descriptor(output_addr: u64) -> u64 {
    // Read-only, EL1-only, non-global, non-executable. A qualified write fill
    // may clear AP[2] only after Dirty handling and code/table write exclusion.
    normal_xn_page_descriptor(output_addr) | (1 << 7) | (1 << 11)
}

fn block_descriptor(output_addr: u64, attr_index: u64, xn: bool) -> u64 {
    descriptor_common(output_addr, attr_index, xn) | 0b01
}

fn page_descriptor(output_addr: u64, attr_index: u64, xn: bool) -> u64 {
    descriptor_common(output_addr, attr_index, xn) | 0b11
}

fn descriptor_common(output_addr: u64, attr_index: u64, xn: bool) -> u64 {
    let mut entry = output_addr & 0x0000_ffff_ffff_f000;
    entry |= (attr_index & 0x7) << 2;
    entry |= 0x3 << 8;
    entry |= 1 << 10;
    if xn {
        entry |= (1_u64 << 53) | (1_u64 << 54);
    }
    entry
}

#[cfg(target_arch = "aarch64")]
unsafe fn enable_stage1_registers(ttbr0: u64, tcr: u64) {
    let mair = STAGE1_MAIR_EL1;
    unsafe {
        install_exception_vectors();
        core::arch::asm!(
            "msr mair_el1, {mair}",
            "msr tcr_el1, {tcr}",
            "msr ttbr0_el1, {ttbr0}",
            "dsb ish",
            "isb",
            mair = in(reg) mair,
            tcr = in(reg) tcr,
            ttbr0 = in(reg) ttbr0,
            options(nostack, preserves_flags)
        );
        let mut sctlr: u64;
        core::arch::asm!("mrs {sctlr}, sctlr_el1", sctlr = out(reg) sctlr, options(nostack, preserves_flags));
        sctlr |= 1 | (1 << 2) | (1 << 12);
        // x86 scalar RAM accesses may be unaligned. Device accesses use the
        // separately constrained, aligned MMIO assembly below.
        sctlr &= !(1 << 1);
        core::arch::asm!(
            "msr sctlr_el1, {sctlr}",
            "isb",
            sctlr = in(reg) sctlr,
            options(nostack, preserves_flags)
        );
    }
}

#[cfg(not(target_arch = "aarch64"))]
unsafe fn enable_stage1_registers(_: u64, _: u64) {}

#[cfg(target_arch = "aarch64")]
unsafe fn install_exception_vectors() {
    unsafe extern "C" {
        static __r86_vectors: u8;
    }
    let vectors = &raw const __r86_vectors as u64;
    unsafe {
        core::arch::asm!(
            "msr vbar_el1, {vectors}",
            "isb",
            vectors = in(reg) vectors,
            options(nostack, preserves_flags)
        );
    }
}

fn shadow_stage1_va(window: ShadowWindow, linear: LinearAddr) -> u64 {
    let base = match window {
        ShadowWindow::Supervisor => SHADOW_SUP_BASE,
        ShadowWindow::User => SHADOW_USR_BASE,
    };
    base | u64::from(linear.0)
}

fn context_key_from_parts(cr0: u32, cr3: u32, cr4: u32) -> ContextKey {
    ContextKey::new(cr0, cr3, cr4)
}

fn hardmmu_mode_eligible(cr0: u32, _cr4: u32, eflags: u32) -> bool {
    (cr0 & (CR0_PE | CR0_PG)) == (CR0_PE | CR0_PG) && (eflags & EFLAGS_VM) == 0
}

#[cfg(any(all(target_arch = "aarch64", target_os = "none"), test))]
fn native_read_window(access: AccessKind, priv_: AccessPriv) -> Option<ShadowWindow> {
    if !matches!(access, AccessKind::Read) {
        return None;
    }
    if priv_.is_user() {
        Some(ShadowWindow::User)
    } else {
        Some(ShadowWindow::Supervisor)
    }
}

#[cfg(any(all(target_arch = "aarch64", target_os = "none"), test))]
fn native_read_in_single_page(linear: LinearAddr, width: u8) -> bool {
    matches!(width, 1 | 2 | 4) && (linear.0 & 0xfff) <= 4096 - u32::from(width)
}

// These leaf thunks and the recoverable vector above use only x0..x8 and
// x30. A C call would unnecessarily spill all other caller-saved registers.
// On an abort ERET restores NZCV; the successful load/MOV/RET preserves it.
// Keep these clobbers in sync with __r86_sync_current_el and its fixup.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
macro_rules! native_read_thunk {
    ($wrapper:ident, $thunk:ident) => {
        #[inline(always)]
        unsafe fn $wrapper(va: u64) -> NativeThunkRead {
            let value;
            let status;
            unsafe {
                core::arch::asm!(
                    "bl {thunk}",
                    thunk = sym $thunk,
                    inlateout("x0") va => value,
                    lateout("x1") status,
                    lateout("x2") _, lateout("x3") _, lateout("x4") _,
                    lateout("x5") _, lateout("x6") _, lateout("x7") _,
                    lateout("x8") _, lateout("x30") _,
                    options(nostack, preserves_flags),
                );
            }
            NativeThunkRead { value, status }
        }
    };
}
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
native_read_thunk!(hard_read_u8, r86_hard_read_u8);
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
native_read_thunk!(hard_read_u16, r86_hard_read_u16);
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
native_read_thunk!(hard_read_u32, r86_hard_read_u32);

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn stage1_publish() {
    unsafe {
        core::arch::asm!("dsb ishst", "isb", options(nostack, preserves_flags));
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn stage1_publish() {}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn flush_stage1_tlb() {
    unsafe {
        core::arch::asm!(
            "dsb ishst",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags)
        );
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn flush_stage1_tlb() {}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn flush_stage1_tlb_va(va: u64, asid: u16) {
    let operand = (va >> 12) | (u64::from(asid) << 48);
    unsafe {
        core::arch::asm!(
            "dsb ishst",
            "tlbi vae1is, {operand}",
            "dsb ish",
            "isb",
            operand = in(reg) operand,
            options(nostack, preserves_flags)
        );
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn flush_stage1_tlb_va(_: u64, _: u16) {}

// All descriptors have already been cleared. Publish the complete batch before
// invalidating, and finish every TLBI before any native access or table reuse.
// Consulted Linux v6.18 arm64 tlbflush.h for ordering and VA/ASID encoding.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn flush_stage1_tlb_pages(pages: &[u64], asid: u16) {
    unsafe {
        core::arch::asm!("dsb ishst", options(nostack, preserves_flags));
        for &va in pages {
            let operand = (va >> 12) | (u64::from(asid) << 48);
            core::arch::asm!("tlbi vae1is, {operand}", operand = in(reg) operand,
                options(nostack, preserves_flags));
        }
        core::arch::asm!("dsb ish", "isb", options(nostack, preserves_flags));
    }
}
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn flush_stage1_tlb_pages(_: &[u64], _: u16) {}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn select_stage1_root(root: u64, asid: u16) {
    unsafe {
        core::arch::asm!("dsb ishst", "msr ttbr0_el1, {root}", "isb", root = in(reg) root | (u64::from(asid) << 48), options(nostack, preserves_flags));
    }
}
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn select_stage1_root(_: u64, _: u16) {}
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn flush_stage1_tlb_asid(asid: u16) {
    unsafe {
        core::arch::asm!("dsb ishst", "tlbi aside1is, {operand}", "dsb ish", "isb", operand = in(reg) u64::from(asid) << 48, options(nostack, preserves_flags));
    }
}
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn flush_stage1_tlb_asid(_: u16) {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MonitorTransportStats {
    pub requests: u64,
    pub doorbells: u64,
    pub response_timeouts: u64,
    pub hello: u64,
    pub pio_in: u64,
    pub pio_out: u64,
    pub timer_poll: u64,
    pub inta: u64,
    pub halt: u64,
    pub serial_flush: u64,
    pub trace_full: u64,
    pub hardmmu_stats: u64,
    pub last_seq: u32,
    pub last_kind: u32,
    pub last_status: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PioRead {
    pub value: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimerPollResult {
    pub pending_mask: u64,
    pub next_timer_vtime: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingCommand {
    pub seq: u32,
    pub command: HostCommandV1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SerialFlushPacket {
    len: u8,
    bytes: [u8; SERIAL_FLUSH_PACKET_CAPACITY],
}

impl SerialFlushPacket {
    pub const EMPTY: Self = Self {
        len: 0,
        bytes: [0; SERIAL_FLUSH_PACKET_CAPACITY],
    };

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MonitorError> {
        if bytes.len() > SERIAL_FLUSH_PACKET_CAPACITY {
            return Err(MonitorError::SerialPacketTooLong { len: bytes.len() });
        }
        let mut packet = Self::EMPTY;
        let mut index = 0;
        while index < bytes.len() {
            packet.bytes[index] = bytes[index];
            index += 1;
        }
        packet.len = bytes.len() as u8;
        Ok(packet)
    }

    pub const fn len(self) -> usize {
        self.len as usize
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len()]
    }

    pub fn request(self, seq: u32) -> MonitorRequestV1 {
        let mut first = [0_u8; 8];
        let mut second = [0_u8; 8];
        let mut third = [0_u8; 8];
        first.copy_from_slice(&self.bytes[0..8]);
        second.copy_from_slice(&self.bytes[8..16]);
        third.copy_from_slice(&self.bytes[16..24]);
        MonitorRequestV1 {
            kind: MonitorRequestKindV1::SerialFlush as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq,
            flags: 0,
            a: self.len as u64,
            b: u64::from_le_bytes(first),
            c: u64::from_le_bytes(second),
            d: u64::from_le_bytes(third),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct X86MonitorBusStats {
    pub phys_reads: u64,
    pub phys_write_probes: u64,
    pub phys_writes: u64,
    pub tracked_writes: u64,
    pub native_read_attempts: u64,
    pub native_read_hits: u64,
    pub native_read_aborts: u64,
    pub native_read_descriptor_aborts: u64,
    pub native_read_translation_aborts: u64,
    pub native_read_access_flag_aborts: u64,
    pub native_read_permission_aborts: u64,
    pub page_table_frames: u64,
    pub pio_in: u64,
    pub pio_out: u64,
    pub mmio_read: u64,
    pub mmio_write: u64,
    pub irq_polls: u64,
    pub irqs_delivered: u64,
    pub exceptions_delivered: u64,
    pub last_vtime: u64,
    pub hlt_wakes: u64,
    pub last_pio_port_dir: u32,
    pub last_mmio_gpa: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MonitorDebugCpuSnapshot {
    pub eip: u32,
    pub eflags: u32,
    pub cr0: u32,
    pub cr2: u32,
    pub cr3: u32,
    pub cr4: u32,
    pub tsc: u64,
    pub cpl: u8,
    pub halted: bool,
    pub if_flag: bool,
    pub fetch_cache: [u64; 3],
    pub linear_cache: [u64; 3],
}

impl MonitorDebugCpuSnapshot {
    pub fn from_cpu(cpu: &Cpu) -> Self {
        Self {
            eip: cpu.eip,
            eflags: cpu.effective_eflags(),
            cr0: cpu.cr0,
            cr2: cpu.cr2,
            cr3: cpu.cr3,
            cr4: cpu.cr4,
            tsc: cpu.tsc(),
            cpl: cpu.cpl(),
            halted: cpu.halted,
            if_flag: cpu.flag(EFLAGS_IF),
            fetch_cache: [
                cpu.cache_stats.fetch_translation_hits,
                cpu.cache_stats.fetch_translation_fills,
                cpu.cache_stats.fetch_translation_invalidations,
            ],
            linear_cache: [
                cpu.cache_stats.linear_translation_hits,
                cpu.cache_stats.linear_translation_fills,
                cpu.cache_stats.linear_translation_invalidations,
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MonitorDebugSnapshot {
    pub phase: DebugPhaseV1,
    pub steps: u64,
    pub cpu: Option<MonitorDebugCpuSnapshot>,
    pub bus: X86MonitorBusStats,
    pub shadow: MonitorShadowRuntimeStats,
    pub transport: MonitorTransportStats,
}

impl MonitorDebugSnapshot {
    pub const fn boot_phase(phase: DebugPhaseV1) -> Self {
        Self {
            phase,
            steps: 0,
            cpu: None,
            bus: X86MonitorBusStats {
                phys_reads: 0,
                phys_write_probes: 0,
                phys_writes: 0,
                tracked_writes: 0,
                native_read_attempts: 0,
                native_read_hits: 0,
                native_read_aborts: 0,
                native_read_descriptor_aborts: 0,
                native_read_translation_aborts: 0,
                native_read_access_flag_aborts: 0,
                native_read_permission_aborts: 0,
                page_table_frames: 0,
                pio_in: 0,
                pio_out: 0,
                mmio_read: 0,
                mmio_write: 0,
                irq_polls: 0,
                irqs_delivered: 0,
                exceptions_delivered: 0,
                last_vtime: 0,
                hlt_wakes: 0,
                last_pio_port_dir: 0,
                last_mmio_gpa: 0,
            },
            shadow: MonitorShadowRuntimeStats {
                translations: 0,
                eligible_4k: 0,
                read_fills: 0,
                write_fills: 0,
                upgrades: 0,
                unchanged_fills: 0,
                soft_non_ram: 0,
                soft_large_or_direct: 0,
                soft_unsupported_mode: 0,
                soft_pt_protected: 0,
                stage1_installs: 0,
                stage1_flushes: 0,
                stage1_leaf_invalidations: 0,
                page_table_write_flushes: 0,
                page_table_write_invalidations: 0,
                page_walk_write_ignores: 0,
                control_flushes: 0,
                invlpg_flushes: 0,
                native_read_attempts: 0,
                native_read_hits: 0,
                native_read_aborts: 0,
                native_read_descriptor_aborts: 0,
                native_read_translation_aborts: 0,
                native_read_access_flag_aborts: 0,
                native_read_permission_aborts: 0,
                shadow_errors: 0,
                context_hits: 0,
                context_misses: 0,
                context_evictions: 0,
            },
            transport: MonitorTransportStats {
                requests: 0,
                doorbells: 0,
                response_timeouts: 0,
                hello: 0,
                pio_in: 0,
                pio_out: 0,
                timer_poll: 0,
                inta: 0,
                halt: 0,
                serial_flush: 0,
                trace_full: 0,
                hardmmu_stats: 0,
                last_seq: 0,
                last_kind: 0,
                last_status: 0,
            },
        }
    }
}

pub struct MonitorDebugPublisher {
    ptr: *mut DebugStateBlockV1,
    seq: u64,
    heartbeat: u64,
    eip_histogram: [u32; 64],
}

impl MonitorDebugPublisher {
    /// Creates a monitor-write-only publisher over the shared debug state block.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a dedicated, writable `DebugStateBlockV1` page that is
    /// not read by monitor control flow. The host may read it concurrently using
    /// the seqlock protocol.
    pub unsafe fn new(ptr: *mut DebugStateBlockV1) -> Self {
        unsafe {
            ptr.write_volatile(DebugStateBlockV1::new());
        }
        fence(Ordering::Release);
        Self {
            ptr,
            seq: 0,
            heartbeat: 0,
            eip_histogram: [0; 64],
        }
    }

    pub fn publish(&mut self, snapshot: MonitorDebugSnapshot) {
        if self.ptr.is_null() {
            return;
        }
        self.heartbeat = self.heartbeat.wrapping_add(1).max(1);
        if let Some(cpu) = snapshot.cpu {
            let bucket = ((cpu.eip >> 26) & 0x3f) as usize;
            self.eip_histogram[bucket] = self.eip_histogram[bucket].wrapping_add(1);
        }

        let odd = self.seq.wrapping_add(1) | 1;
        let even = odd.wrapping_add(1) & !1;
        unsafe {
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*self.ptr).seq), odd);
        }
        compiler_fence(Ordering::Release);
        unsafe {
            self.write_payload(snapshot);
        }
        fence(Ordering::Release);
        unsafe {
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*self.ptr).seq), even);
        }
        self.seq = even;
    }

    pub fn publish_phase(&mut self, phase: DebugPhaseV1) {
        self.publish(MonitorDebugSnapshot::boot_phase(phase));
    }

    pub fn publish_panic_flag(&mut self, value: u32) {
        if self.ptr.is_null() {
            return;
        }
        unsafe {
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*self.ptr).panic_flag), value);
        }
        fence(Ordering::Release);
    }

    unsafe fn write_payload(&mut self, snapshot: MonitorDebugSnapshot) {
        let cpu = snapshot.cpu;
        let ptr = self.ptr;
        unsafe {
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).magic),
                crate::monitor::abi::debug::DEBUG_MAGIC,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).version),
                crate::monitor::abi::debug::DEBUG_ABI_VERSION,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).size),
                core::mem::size_of::<DebugStateBlockV1>() as u32,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).fields_valid),
                crate::monitor::abi::debug::DEBUG_STATE_FIELDS_ALL,
            );
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).heartbeat), self.heartbeat);
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).phase), snapshot.phase as u32);
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).steps), snapshot.steps);
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).vtime),
                cpu.map(|cpu| cpu.tsc).unwrap_or(snapshot.bus.last_vtime),
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).eip),
                cpu.map(|cpu| cpu.eip).unwrap_or(0),
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).eflags),
                cpu.map(|cpu| cpu.eflags).unwrap_or(0),
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).cr0),
                cpu.map(|cpu| cpu.cr0).unwrap_or(0),
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).cr2),
                cpu.map(|cpu| cpu.cr2).unwrap_or(0),
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).cr3),
                cpu.map(|cpu| cpu.cr3).unwrap_or(0),
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).cr4),
                cpu.map(|cpu| cpu.cr4).unwrap_or(0),
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).cpl_halted_if),
                cpu.map(pack_debug_cpl_halted_if).unwrap_or(0),
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).last_exc_vector),
                if snapshot.bus.exceptions_delivered == 0 {
                    0
                } else {
                    u32::MAX
                },
            );
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).last_exc_error), 0);
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).last_mailbox_kind),
                snapshot.transport.last_kind,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).last_mailbox_seq),
                u64::from(snapshot.transport.last_seq),
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).last_pio_port_dir),
                snapshot.bus.last_pio_port_dir,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).last_mmio_gpa),
                snapshot.bus.last_mmio_gpa,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).stage1_installs),
                snapshot.shadow.stage1_installs,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).stage1_leaf_invalidations),
                snapshot.shadow.stage1_leaf_invalidations,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).shadow_flushes),
                snapshot.shadow.stage1_flushes,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).shadow_flush_by_reason),
                [
                    snapshot.shadow.control_flushes,
                    snapshot.shadow.invlpg_flushes,
                    snapshot.shadow.page_table_write_flushes,
                    snapshot.shadow.page_walk_write_ignores,
                ],
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).shadow_translations),
                snapshot.shadow.translations,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).shadow_eligible_4k),
                snapshot.shadow.eligible_4k,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).shadow_read_fills),
                snapshot.shadow.read_fills,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).shadow_write_fills),
                snapshot.shadow.write_fills,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).shadow_upgrades),
                snapshot.shadow.upgrades,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).shadow_unchanged_fills),
                snapshot.shadow.unchanged_fills,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).shadow_soft_counts),
                [
                    snapshot.shadow.soft_non_ram,
                    snapshot.shadow.soft_large_or_direct,
                    snapshot.shadow.soft_unsupported_mode,
                    snapshot.shadow.soft_pt_protected,
                ],
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).shadow_page_table_write_invalidations),
                snapshot.shadow.page_table_write_invalidations,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).shadow_page_walk_write_ignores),
                snapshot.shadow.page_walk_write_ignores,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).shadow_errors),
                snapshot.shadow.shadow_errors,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).native_reads),
                snapshot.bus.native_read_attempts,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).native_read_hits),
                snapshot.bus.native_read_hits,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).native_read_aborts),
                snapshot.bus.native_read_aborts,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).fetch_cache),
                cpu.map(|cpu| cpu.fetch_cache).unwrap_or([0; 3]),
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).linear_cache),
                cpu.map(|cpu| cpu.linear_cache).unwrap_or([0; 3]),
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).irq_injections),
                snapshot.bus.irqs_delivered,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).hlt_wakes),
                snapshot.bus.hlt_wakes,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).timer_polls),
                snapshot.transport.timer_poll,
            );
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).serial_bytes),
                snapshot.bus.pio_out,
            );
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).trace_write_index), 0);
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*ptr).trace_dropped), 0);
            core::ptr::write_volatile(
                core::ptr::addr_of_mut!((*ptr).eip_histogram),
                self.eip_histogram,
            );
        }
    }
}

fn pack_debug_cpl_halted_if(cpu: MonitorDebugCpuSnapshot) -> u32 {
    u32::from(cpu.cpl) | (u32::from(cpu.halted) << 8) | (u32::from(cpu.if_flag) << 9)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MonitorShadowRuntimeStats {
    pub translations: u64,
    pub eligible_4k: u64,
    pub read_fills: u64,
    pub write_fills: u64,
    /// Successful fills that retain a large x86 leaf as an ARM 2 MiB block.
    pub upgrades: u64,
    pub unchanged_fills: u64,
    pub soft_non_ram: u64,
    pub soft_large_or_direct: u64,
    pub soft_unsupported_mode: u64,
    pub soft_pt_protected: u64,
    pub stage1_installs: u64,
    pub stage1_flushes: u64,
    pub stage1_leaf_invalidations: u64,
    /// Dirty-owner refreshes after table writes; surviving leaves remain mapped.
    /// Only refreshes with retired hardware entries contribute to stage1_flushes.
    pub page_table_write_flushes: u64,
    pub page_table_write_invalidations: u64,
    pub page_walk_write_ignores: u64,
    pub control_flushes: u64,
    pub invlpg_flushes: u64,
    pub native_read_attempts: u64,
    pub native_read_hits: u64,
    pub native_read_aborts: u64,
    pub native_read_descriptor_aborts: u64,
    pub native_read_translation_aborts: u64,
    pub native_read_access_flag_aborts: u64,
    pub native_read_permission_aborts: u64,
    pub shadow_errors: u64,
    pub context_hits: u64,
    pub context_misses: u64,
    pub context_evictions: u64,
}

#[repr(C)]
pub struct MonitorShadowRuntime<const FRAMES: usize, const L3_TABLES: usize> {
    // Keep hot hit counters at small offsets, ahead of the dependency bitmap.
    stats: MonitorShadowRuntimeStats,
    // Both PC adapters use this owner for table promotion and writable aliases.
    pub(crate) core: ContextCache<FRAMES>,
    stage1: *mut MonitorShadowStage1Tables<L3_TABLES>,
    // A positive bit may outlive a native mapping. Before promoting a physical
    // frame to code/table, a positive bit revokes every native alias. No reverse
    // map is needed, and no native write can escape the shared write barriers.
    #[cfg(feature = "jit")]
    native_writable: [u64; MONITOR_SHADOW_FRAMES / 64],
}

impl<const FRAMES: usize, const L3_TABLES: usize> MonitorShadowRuntime<FRAMES, L3_TABLES> {
    pub const fn new(stage1: *mut MonitorShadowStage1Tables<L3_TABLES>) -> Self {
        Self {
            core: ContextCache::new(),
            stage1,
            #[cfg(feature = "jit")]
            native_writable: [0; MONITOR_SHADOW_FRAMES / 64],
            stats: MonitorShadowRuntimeStats {
                translations: 0,
                eligible_4k: 0,
                read_fills: 0,
                write_fills: 0,
                upgrades: 0,
                unchanged_fills: 0,
                soft_non_ram: 0,
                soft_large_or_direct: 0,
                soft_unsupported_mode: 0,
                soft_pt_protected: 0,
                stage1_installs: 0,
                stage1_flushes: 0,
                stage1_leaf_invalidations: 0,
                page_table_write_flushes: 0,
                page_table_write_invalidations: 0,
                page_walk_write_ignores: 0,
                control_flushes: 0,
                invlpg_flushes: 0,
                native_read_attempts: 0,
                native_read_hits: 0,
                native_read_aborts: 0,
                native_read_descriptor_aborts: 0,
                native_read_translation_aborts: 0,
                native_read_access_flag_aborts: 0,
                native_read_permission_aborts: 0,
                shadow_errors: 0,
                context_hits: 0,
                context_misses: 0,
                context_evictions: 0,
            },
        }
    }

    pub fn bind_stage1(&mut self, stage1: *mut MonitorShadowStage1Tables<L3_TABLES>) {
        self.stage1 = stage1;
    }

    pub fn reset(&mut self, cpu: &Cpu) {
        self.core = ContextCache::new();
        self.stats = MonitorShadowRuntimeStats::default();
        self.stage1_mut().flush();
        #[cfg(feature = "jit")]
        self.native_writable.fill(0);
        self.select_context(context_key_from_parts(cpu.cr0, cpu.cr3, cpu.cr4));
    }

    pub fn stats(&self) -> MonitorShadowRuntimeStats {
        let mut stats = self.stats;
        stats.native_read_attempts = stats
            .native_read_hits
            .wrapping_add(stats.native_read_aborts);
        stats
    }

    pub fn descriptor_for(&mut self, window: ShadowWindow, linear: LinearAddr) -> Option<u64> {
        self.flush_dirty_context();
        unsafe { &*self.stage1 }.descriptor_for(window, linear)
    }

    pub fn record_native_read_hit(&mut self) {
        self.stats.native_read_hits = self.stats.native_read_hits.wrapping_add(1);
    }

    /// Generated reads have no per-access Rust hook. Prepare the active root
    /// and complete deferred page-table invalidation before native entry.
    #[cfg(all(feature = "jit", target_arch = "aarch64", target_os = "none"))]
    pub(crate) fn prepare_jit_reads(&mut self, cpu: &Cpu) -> u64 {
        assert!(hardmmu_mode_eligible(cpu.cr0, cpu.cr4, cpu.eflags));
        self.select_context(context_key_from_parts(cpu.cr0, cpu.cr3, cpu.cr4));
        if cpu.cpl() == 3 {
            SHADOW_USR_BASE
        } else {
            SHADOW_SUP_BASE
        }
    }

    pub fn native_hit_translation(
        &self,
        linear: LinearAddr,
        priv_: AccessPriv,
    ) -> Option<(PhysAddr, TranslationPageSize)> {
        let window = if priv_.is_user() {
            ShadowWindow::User
        } else {
            ShadowWindow::Supervisor
        };
        // The preceding native hit already flushed the active context. Its
        // descriptor is a completed shared-walker translation, including
        // permissions and Accessed effects; no second x86 walk is needed.
        let tables = unsafe { &*self.stage1 };
        let descriptor = tables.descriptor_for(window, linear)?;
        let index = tables.index_for(window, linear)?;
        let page_size = match tables.sources[index].page_shift {
            12 => TranslationPageSize::Size4K,
            21 => TranslationPageSize::Size2M,
            // Raw table construction without a walk source keeps the previous
            // conservative large-page behavior; production fills record a size.
            0 | 22 => TranslationPageSize::Size4M,
            _ => unreachable!("invalid native guest page size"),
        };
        let ipa = descriptor & 0x0000_ffff_ffff_f000;
        Some((
            PhysAddr(ipa.checked_sub(abi::X86_RAM_IPA_BASE)? + u64::from(linear.0 & 4095)),
            page_size,
        ))
    }

    pub fn record_native_read_abort(&mut self) {
        self.stats.native_read_aborts = self.stats.native_read_aborts.wrapping_add(1);
    }

    pub fn record_native_read_descriptor_abort(&mut self) {
        self.stats.native_read_descriptor_aborts =
            self.stats.native_read_descriptor_aborts.wrapping_add(1);
    }

    pub fn record_native_read_abort_status(&mut self, status: u64) {
        match status {
            NATIVE_THUNK_STATUS_TRANSLATION_ABORT => {
                self.stats.native_read_translation_aborts =
                    self.stats.native_read_translation_aborts.wrapping_add(1);
            }
            NATIVE_THUNK_STATUS_ACCESS_FLAG_ABORT => {
                self.stats.native_read_access_flag_aborts =
                    self.stats.native_read_access_flag_aborts.wrapping_add(1);
            }
            NATIVE_THUNK_STATUS_PERMISSION_ABORT => {
                self.stats.native_read_permission_aborts =
                    self.stats.native_read_permission_aborts.wrapping_add(1);
            }
            _ => {}
        }
    }

    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    #[cold]
    fn native_read_miss(&mut self, window: ShadowWindow, linear: LinearAddr, status: u64) {
        self.record_native_read_abort();
        self.record_native_read_abort_status(status);
        // Missing cache entries are expected; an installed readable entry
        // must never fault. AF and permission faults indicate monitor bugs.
        assert_eq!(status, NATIVE_THUNK_STATUS_TRANSLATION_ABORT);
        assert!(self.descriptor_for(window, linear).is_none());
    }

    pub fn try_native_read_linear_u8(
        &mut self,
        linear: LinearAddr,
        access: AccessKind,
        priv_: AccessPriv,
        cr0: u32,
        cr3: u32,
        cr4: u32,
        eflags: u32,
    ) -> NativeRead<u8> {
        let _ = cr3;
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            let _ = (linear, access, priv_, cr0, cr4, eflags);
            NativeRead::Unsupported
        }
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            let Some(window) = native_read_window(access, priv_) else {
                return NativeRead::Unsupported;
            };
            if !hardmmu_mode_eligible(cr0, cr4, eflags) || !native_read_in_single_page(linear, 1) {
                return NativeRead::Unsupported;
            }
            let shadow = self;
            shadow.flush_dirty_context();
            let result = unsafe { hard_read_u8(shadow_stage1_va(window, linear)) };
            if result.status == NATIVE_THUNK_STATUS_HIT {
                shadow.record_native_read_hit();
                NativeRead::Hit(result.value as u8)
            } else {
                shadow.native_read_miss(window, linear, result.status);
                NativeRead::Miss
            }
        }
    }

    pub fn try_native_read_linear_u16(
        &mut self,
        linear: LinearAddr,
        access: AccessKind,
        priv_: AccessPriv,
        cr0: u32,
        cr3: u32,
        cr4: u32,
        eflags: u32,
    ) -> NativeRead<u16> {
        let _ = cr3;
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            let _ = (linear, access, priv_, cr0, cr4, eflags);
            NativeRead::Unsupported
        }
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            let Some(window) = native_read_window(access, priv_) else {
                return NativeRead::Unsupported;
            };
            if !hardmmu_mode_eligible(cr0, cr4, eflags) || !native_read_in_single_page(linear, 2) {
                return NativeRead::Unsupported;
            }
            let shadow = self;
            shadow.flush_dirty_context();
            let result = unsafe { hard_read_u16(shadow_stage1_va(window, linear)) };
            if result.status == NATIVE_THUNK_STATUS_HIT {
                shadow.record_native_read_hit();
                NativeRead::Hit(result.value as u16)
            } else {
                shadow.native_read_miss(window, linear, result.status);
                NativeRead::Miss
            }
        }
    }

    #[inline(always)]
    pub fn try_native_read_linear_u32(
        &mut self,
        linear: LinearAddr,
        access: AccessKind,
        priv_: AccessPriv,
        cr0: u32,
        cr3: u32,
        cr4: u32,
        eflags: u32,
    ) -> NativeRead<u32> {
        let _ = cr3;
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            let _ = (linear, access, priv_, cr0, cr4, eflags);
            NativeRead::Unsupported
        }
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            let Some(window) = native_read_window(access, priv_) else {
                return NativeRead::Unsupported;
            };
            if !hardmmu_mode_eligible(cr0, cr4, eflags) || !native_read_in_single_page(linear, 4) {
                return NativeRead::Unsupported;
            }
            let shadow = self;
            shadow.flush_dirty_context();
            let result = unsafe { hard_read_u32(shadow_stage1_va(window, linear)) };
            if result.status == NATIVE_THUNK_STATUS_HIT {
                shadow.record_native_read_hit();
                NativeRead::Hit(result.value as u32)
            } else {
                shadow.native_read_miss(window, linear, result.status);
                NativeRead::Miss
            }
        }
    }

    pub fn after_linear_translate(
        &mut self,
        regions: &RegionTableV1,
        linear: LinearAddr,
        _access: AccessKind,
        priv_: AccessPriv,
        cr0: u32,
        cr3: u32,
        cr4: u32,
        eflags: u32,
        translation: Translation,
    ) {
        let target = translation.phys.0 & !(PAGE_SIZE_4K - 1);
        let ordinary_ram_page = matches!(
            regions.classify(target, PAGE_SIZE_4K),
            Ok(RegionClassV1::Ram)
        );
        let ordinary_ram_block = ordinary_ram_page
            && matches!(
                translation.page_size,
                TranslationPageSize::Size2M | TranslationPageSize::Size4M
            )
            && matches!(
                regions.classify(target & !0x1f_ffff, 0x20_0000),
                Ok(RegionClassV1::Ram)
            );
        self.after_linear_translate_ram(
            ordinary_ram_page,
            ordinary_ram_block,
            linear,
            _access,
            priv_,
            cr0,
            cr3,
            cr4,
            eflags,
            translation,
        );
    }

    /// The shared PC's region decoder must approve the entire physical page.
    pub fn after_linear_translate_ram(
        &mut self,
        ordinary_ram_page: bool,
        ordinary_ram_block: bool,
        linear: LinearAddr,
        _access: AccessKind,
        priv_: AccessPriv,
        cr0: u32,
        cr3: u32,
        cr4: u32,
        eflags: u32,
        translation: Translation,
    ) {
        self.after_linear_translate_backing(
            ordinary_ram_page
                .then_some(abi::X86_RAM_IPA_BASE + (translation.phys.0 & !(PAGE_SIZE_4K - 1))),
            ordinary_ram_block,
            false,
            linear,
            _access,
            priv_,
            cr0,
            cr3,
            cr4,
            eflags,
            translation,
        );
    }

    /// `read_page` is the ARM IPA of an entire stable page whose reads have no
    /// device side effects. The owner must revoke mappings on decode changes.
    /// Whole-block admission remains restricted to ordinary guest RAM.
    pub(crate) fn after_linear_translate_backing(
        &mut self,
        read_page: Option<u64>,
        ordinary_ram_block: bool,
        write: bool,
        linear: LinearAddr,
        _access: AccessKind,
        priv_: AccessPriv,
        cr0: u32,
        cr3: u32,
        cr4: u32,
        eflags: u32,
        translation: Translation,
    ) {
        self.stats.translations += 1;
        self.select_context(context_key_from_parts(cr0, cr3, cr4));
        let tracked = self.core.track(translation);
        if !hardmmu_mode_eligible(cr0, cr4, eflags) {
            self.stats.soft_unsupported_mode += 1;
            return;
        }
        if translation.page_size == TranslationPageSize::Direct {
            self.stats.soft_large_or_direct += 1;
            return;
        }
        if !tracked || read_page.is_none() {
            self.stats.soft_non_ram += 1;
            return;
        }
        if write {
            assert!(_access == AccessKind::Write && translation.leaf_dirty);
            #[cfg(feature = "jit")]
            {
                let page = (translation.phys.0 >> 12) as usize;
                let Some(word) = self.native_writable.get_mut(page / 64) else {
                    return;
                };
                *word |= 1 << (page % 64);
            }
            #[cfg(not(feature = "jit"))]
            unreachable!("native stores require the JIT write barrier");
        }
        assert!(
            translation.all_accessed,
            "native fill before x86 Accessed writeback"
        );
        self.stats.eligible_4k += 1;
        let window = if priv_.is_user() {
            ShadowWindow::User
        } else {
            ShadowWindow::Supervisor
        };
        let block = ordinary_ram_block
            && !write
            && matches!(
                translation.page_size,
                TranslationPageSize::Size2M | TranslationPageSize::Size4M
            );
        let installed = if block {
            self.stage1_mut()
                .install_block(window, linear, translation.phys)
        } else {
            self.stage1_mut()
                .install_data_page(window, linear, read_page.unwrap(), write)
        };
        if installed.is_ok() {
            self.stage1_mut().record_source(
                window,
                linear,
                TableSource::from_walk(linear, cr3, cr4, translation),
            );
        }
        match installed {
            Ok(true) => {
                self.stats.upgrades += u64::from(block);
                self.stats.stage1_installs += 1;
                self.stats.read_fills += u64::from(!write);
                self.stats.write_fills += u64::from(write);
            }
            Ok(false) => {
                self.stats.unchanged_fills += 1;
            }
            Err(MonitorError::ShadowStage1PoolFull) => {
                // Finite cache capacity, not an architectural fault. Keep the
                // dependency bits: the CPU's software cache still needs them.
                self.stage1_mut().flush();
                self.stats.stage1_flushes += 1;
            }
            Err(error) => panic!("invalid shadow table: {:?}", error),
        }
    }

    /// A20/device decode changes invalidate all resident mappings. Keep walk
    /// dependencies for CPU caches until their architectural invalidation.
    pub fn flush_for_physical_map_change(&mut self) {
        self.stage1_mut().flush();
        #[cfg(feature = "jit")]
        self.native_writable.fill(0);
        self.stats.stage1_flushes += 1;
    }

    /// Called before a physical frame acquires a code/table write watch.
    /// A full alias flush is rare; stale bitmap bits only cause extra flushing.
    #[cfg(feature = "machine")]
    pub(crate) fn protect_native_writes(&mut self, frame: crate::x86::PhysFrame) {
        #[cfg(feature = "jit")]
        if self
            .native_writable
            .get(frame.0 as usize / 64)
            .is_some_and(|word| word & (1 << (frame.0 % 64)) != 0)
        {
            self.flush_for_physical_map_change();
        }
        let _ = frame;
    }

    pub fn observe_phys_write(&mut self, write: PhysWrite) {
        if write.origin == WriteOrigin::PageWalk {
            self.stats.page_walk_write_ignores += 1;
        }
        let users = self.core.observe_write(write);
        if users != 0 {
            self.stats.page_table_write_invalidations += 1;
            self.stage1_mut().invalidate_write(write, users);
        }
    }
    pub fn write_touches_page_table_frame(&self, write: PhysWrite) -> bool {
        self.core.write_affects_active(write)
    }
    pub fn flush_for_control_write(&mut self, index: u8, _old: u32, new: u32) {
        let Some(key) = self.core.key() else {
            return;
        };
        let next = match index {
            0 => ContextKey::new(new, key.cr3, key.cr4),
            3 => ContextKey::new(key.cr0, new, key.cr4),
            4 => ContextKey::new(key.cr0, key.cr3, new),
            _ => return,
        };
        self.select_context(next);
        // Legacy PAE caches PDPTRs in the architectural CPU. Reloading CR3 or
        // the relevant mode controls can change those values without changing
        // this key. Do not retain PAE translations across those reloads.
        if next.cr0 & CR0_PG != 0 && next.cr4 & CR4_PAE != 0 {
            self.stats.control_flushes += 1;
            self.flush_active();
        }
    }
    pub fn flush_for_invlpg(&mut self, linear: LinearAddr) {
        self.stats.invlpg_flushes += 1;
        self.flush_dirty_context();
        let removed = self.stage1_mut().invalidate_linear(linear);
        self.stats.stage1_leaf_invalidations += removed;
        self.stats.stage1_flushes += u64::from(removed != 0);
        // Unrelated mappings survive, so their page-table dependencies must too.
    }
    fn select_context(&mut self, key: ContextKey) {
        let selected = self.core.select(key);
        if selected.switched {
            if selected.hit {
                self.stats.context_hits += 1;
            } else {
                self.stats.context_misses += 1;
            }
            if selected.evicted {
                self.stats.context_evictions += 1;
            }
            let active = self.core.active();
            // Flush a recycled ASID before selecting its root. Monitor and
            // direct-map global entries are identical in every root.
            self.stage1_mut().active = active;
            if selected.evicted {
                self.flush_active();
            } else {
                self.flush_dirty_context();
            }
            self.stage1_mut().activate(active);
        } else {
            self.flush_dirty_context();
        }
    }
    #[inline(always)]
    fn flush_dirty_context(&mut self) {
        if self.core.dirty() {
            self.flush_dirty_context_slow();
        }
    }
    #[cold]
    #[inline(never)]
    fn flush_dirty_context_slow(&mut self) {
        self.stats.page_table_write_flushes += 1;
        // Keep native-write guards only for surviving hardware sources. The
        // shared Machine's software TLB can retain old mappings until CRx or
        // INVLPG; those entries need no physical table-write watch. A future
        // shadow admission rediscovers its sources and revokes writable aliases.
        let tables = unsafe { &mut *self.stage1 };
        self.stats.stage1_flushes += u64::from(tables.finish_invalidations());
        self.core.finish_flush();
        let owner = self.core.active() as u16 + 1;
        for index in 0..L3_TABLES {
            if tables.links[index] >> 12 != owner {
                continue;
            }
            let source = tables.sources[index];
            for addr in source
                .upper
                .into_iter()
                .chain(core::iter::once(source.leaves))
            {
                if addr != u64::MAX {
                    self.core
                        .track_frame(crate::x86::PhysFrame::containing(PhysAddr(addr)));
                }
            }
        }
    }
    fn flush_active(&mut self) {
        self.stage1_mut().flush_active();
        self.core.finish_flush();
        self.stats.stage1_flushes += 1;
    }
    fn stage1_mut(&mut self) -> &mut MonitorShadowStage1Tables<L3_TABLES> {
        unsafe { &mut *self.stage1 }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct X86MonitorRunReport {
    pub steps: u32,
    pub halted: bool,
    pub eip: u32,
    pub tsc: u64,
    pub bus: X86MonitorBusStats,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct X86MonitorStepReport {
    pub steps: u64,
    pub halted: bool,
    pub eip: u32,
    pub tsc: u64,
    pub bus: X86MonitorBusStats,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MonitorStopAckPacket {
    pub steps: u64,
    pub eip: u32,
    pub eflags: u32,
    pub phys_writes: u32,
    pub mmio_writes: u32,
    pub pio_outs: u32,
    pub irq_polls: u32,
}

impl MonitorStopAckPacket {
    pub fn from_cpu_and_report(cpu: &Cpu, report: Option<&X86MonitorStepReport>) -> Self {
        let steps = report.map(|report| report.steps).unwrap_or(0);
        let stats = report.map(|report| report.bus).unwrap_or_default();
        Self {
            steps,
            eip: cpu.eip,
            eflags: cpu.effective_eflags(),
            phys_writes: saturating_u64_to_u32(stats.phys_writes),
            mmio_writes: saturating_u64_to_u32(stats.mmio_write),
            pio_outs: saturating_u64_to_u32(stats.pio_out),
            irq_polls: saturating_u64_to_u32(stats.irq_polls),
        }
    }

    pub const fn request(self, seq: u32) -> MonitorRequestV1 {
        MonitorRequestV1 {
            kind: MonitorRequestKindV1::StopAck as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq,
            flags: 0,
            a: self.steps,
            b: pack_u32_pair(self.eip, self.eflags),
            c: pack_u32_pair(self.phys_writes, self.mmio_writes),
            d: pack_u32_pair(self.pio_outs, self.irq_polls),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MonitorHardMmuStatsPacket {
    pub part: u32,
    pub b: u64,
    pub c: u64,
    pub d: u64,
}

impl MonitorHardMmuStatsPacket {
    pub fn from_cpu_report_and_shadow_part(
        cpu: &Cpu,
        report: Option<&X86MonitorStepReport>,
        shadow: MonitorShadowRuntimeStats,
        part: u32,
    ) -> Option<Self> {
        let bus = report.map(|report| report.bus).unwrap_or_default();
        let steps = report.map(|report| report.steps).unwrap_or(0);
        let packet = match part {
            0 => Self {
                part,
                b: pack_u32_pair(cpu.cr0, cpu.cr3),
                c: pack_u32_pair(cpu.cr4, cpu.effective_eflags()),
                d: steps,
            },
            1 => Self {
                part,
                b: pack_u32_pair(
                    saturating_u64_to_u32(bus.native_read_attempts),
                    saturating_u64_to_u32(bus.native_read_hits),
                ),
                c: pack_u32_pair(
                    saturating_u64_to_u32(bus.native_read_aborts),
                    saturating_u64_to_u32(bus.page_table_frames),
                ),
                d: pack_u32_pair(
                    saturating_u64_to_u32(bus.phys_reads),
                    saturating_u64_to_u32(bus.tracked_writes),
                ),
            },
            2 => Self {
                part,
                b: pack_u32_pair(
                    saturating_u64_to_u32(shadow.translations),
                    saturating_u64_to_u32(shadow.eligible_4k),
                ),
                c: pack_u32_pair(
                    saturating_u64_to_u32(shadow.read_fills),
                    saturating_u64_to_u32(shadow.write_fills),
                ),
                d: pack_u32_pair(
                    saturating_u64_to_u32(shadow.upgrades),
                    saturating_u64_to_u32(shadow.stage1_installs),
                ),
            },
            3 => Self {
                part,
                b: pack_u32_pair(
                    saturating_u64_to_u32(shadow.soft_unsupported_mode),
                    saturating_u64_to_u32(shadow.soft_large_or_direct),
                ),
                c: pack_u32_pair(
                    saturating_u64_to_u32(shadow.soft_non_ram),
                    saturating_u64_to_u32(shadow.soft_pt_protected),
                ),
                d: pack_u32_pair(
                    saturating_u64_to_u32(shadow.shadow_errors),
                    saturating_u64_to_u32(shadow.stage1_flushes),
                ),
            },
            4 => Self {
                part,
                b: pack_u32_pair(
                    saturating_u64_to_u32(shadow.page_table_write_flushes),
                    saturating_u64_to_u32(shadow.control_flushes),
                ),
                c: pack_u32_pair(
                    saturating_u64_to_u32(shadow.invlpg_flushes),
                    saturating_u64_to_u32(shadow.native_read_attempts),
                ),
                d: pack_u32_pair(
                    saturating_u64_to_u32(shadow.native_read_hits),
                    saturating_u64_to_u32(shadow.native_read_aborts),
                ),
            },
            5 => Self {
                part,
                b: pack_u32_pair(
                    saturating_u64_to_u32(bus.native_read_descriptor_aborts),
                    saturating_u64_to_u32(shadow.native_read_descriptor_aborts),
                ),
                c: pack_u32_pair(
                    saturating_u64_to_u32(bus.native_read_translation_aborts),
                    saturating_u64_to_u32(shadow.native_read_translation_aborts),
                ),
                d: pack_u32_pair(
                    saturating_u64_to_u32(bus.native_read_access_flag_aborts),
                    saturating_u64_to_u32(shadow.native_read_access_flag_aborts),
                ),
            },
            6 => Self {
                part,
                b: pack_u32_pair(
                    saturating_u64_to_u32(bus.native_read_permission_aborts),
                    saturating_u64_to_u32(shadow.native_read_permission_aborts),
                ),
                c: pack_u32_pair(
                    saturating_u64_to_u32(shadow.context_hits),
                    saturating_u64_to_u32(shadow.context_misses),
                ),
                d: shadow.context_evictions,
            },
            7 => Self {
                part,
                b: pack_u32_pair(
                    saturating_u64_to_u32(cpu.cache_stats.fetch_translation_hits),
                    saturating_u64_to_u32(cpu.cache_stats.fetch_translation_fills),
                ),
                c: pack_u32_pair(
                    saturating_u64_to_u32(cpu.cache_stats.fetch_translation_invalidations),
                    saturating_u64_to_u32(cpu.cache_stats.linear_translation_hits),
                ),
                d: pack_u32_pair(
                    saturating_u64_to_u32(cpu.cache_stats.linear_translation_fills),
                    saturating_u64_to_u32(cpu.cache_stats.linear_translation_invalidations),
                ),
            },
            _ => return None,
        };
        Some(packet)
    }

    pub const fn request(self, seq: u32) -> MonitorRequestV1 {
        MonitorRequestV1 {
            kind: MonitorRequestKindV1::HardMmuStats as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq,
            flags: 0,
            a: self.part as u64,
            b: self.b,
            c: self.c,
            d: self.d,
        }
    }
}

fn saturating_u64_to_u32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

const fn pack_u32_pair(low: u32, high: u32) -> u64 {
    low as u64 | ((high as u64) << 32)
}

#[derive(Debug, Eq, PartialEq)]
pub struct MonitorRam {
    base: usize,
    len: u64,
}

impl MonitorRam {
    /// Creates a bounded physical-memory view.
    ///
    /// # Safety
    ///
    /// The caller must ensure `[base, base + len)` is valid RAM for plain scalar
    /// accesses and that this `MonitorRam` owns the only mutable emulator access to
    /// that range while it is in use. The base must be nonzero: a mapped ARM
    /// address zero is still an invalid Rust pointer. Monitor callers use the
    /// identity-mapped ARM RAM IPA base from the boot block.
    pub unsafe fn from_raw_parts(base: u64, len: u64) -> Result<Self, X86MonitorError> {
        let base =
            usize::try_from(base).map_err(|_| X86MonitorError::AddressTooLarge { addr: base })?;
        Ok(Self { base, len })
    }

    pub fn from_slice(bytes: &mut [u8]) -> Self {
        Self {
            base: bytes.as_mut_ptr() as usize,
            len: bytes.len() as u64,
        }
    }

    pub const fn len(&self) -> u64 {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn read_u8(&self, addr: PhysAddr) -> Result<u8, MemError> {
        let ptr = self.checked_ptr(addr)?;
        Ok(unsafe { ptr.read() })
    }

    pub fn read_u16(&self, addr: PhysAddr) -> Result<u16, MemError> {
        let ptr = self.checked_range_ptr(addr, 2)?;
        Ok(unsafe { ram_read_u16(ptr) })
    }

    pub fn read_u32(&self, addr: PhysAddr) -> Result<u32, MemError> {
        let ptr = self.checked_range_ptr(addr, 4)?;
        Ok(unsafe { ram_read_u32(ptr) })
    }

    pub fn read_u64(&self, addr: PhysAddr) -> Result<u64, MemError> {
        let ptr = self.checked_range_ptr(addr, 8)?;
        Ok(unsafe { ram_read_u64(ptr) })
    }

    pub fn probe_write_u8(&self, addr: PhysAddr) -> Result<(), MemError> {
        self.checked_ptr(addr).map(|_| ())
    }

    pub fn write_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
        let ptr = self.checked_ptr(addr)?;
        unsafe {
            ptr.write(value);
        }
        Ok(())
    }

    pub fn write_u16(&mut self, addr: PhysAddr, value: u16) -> Result<(), MemError> {
        let ptr = self.checked_range_ptr(addr, 2)?;
        unsafe {
            ram_write_u16(ptr, value);
        }
        Ok(())
    }

    pub fn write_u32(&mut self, addr: PhysAddr, value: u32) -> Result<(), MemError> {
        let ptr = self.checked_range_ptr(addr, 4)?;
        unsafe {
            ram_write_u32(ptr, value);
        }
        Ok(())
    }

    pub fn write_u64(&mut self, addr: PhysAddr, value: u64) -> Result<(), MemError> {
        let ptr = self.checked_range_ptr(addr, 8)?;
        unsafe {
            ram_write_u64(ptr, value);
        }
        Ok(())
    }

    pub fn write_bytes(&mut self, addr: PhysAddr, bytes: &[u8]) -> Result<(), X86MonitorError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let end = addr
            .0
            .checked_add(bytes.len() as u64)
            .ok_or(X86MonitorError::RamOutOfRange {
                addr,
                len: bytes.len() as u64,
                ram_len: self.len,
            })?;
        if end > self.len {
            return Err(X86MonitorError::RamOutOfRange {
                addr,
                len: bytes.len() as u64,
                ram_len: self.len,
            });
        }
        let ptr = self.checked_range_ptr(addr, bytes.len() as u64)?;
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        }
        Ok(())
    }

    fn checked_ptr(&self, addr: PhysAddr) -> Result<*mut u8, MemError> {
        if addr.0 >= self.len {
            return Err(MemError::OutOfRange { addr });
        }
        let offset = usize::try_from(addr.0).map_err(|_| MemError::OutOfRange { addr })?;
        let absolute = self
            .base
            .checked_add(offset)
            .ok_or(MemError::OutOfRange { addr })?;
        Ok(absolute as *mut u8)
    }

    fn checked_range_ptr(&self, addr: PhysAddr, len: u64) -> Result<*mut u8, MemError> {
        let end = addr
            .0
            .checked_add(len)
            .ok_or(MemError::OutOfRange { addr })?;
        if end > self.len {
            return Err(MemError::OutOfRange { addr });
        }
        self.checked_ptr(addr)
    }
}

pub struct MonitorSoftBus<'a, 'b, D> {
    ram: &'a mut MonitorRam,
    regions: RegionTableV1,
    client: &'a mut MailboxClient<'b>,
    doorbell: &'a mut D,
    stats: X86MonitorBusStats,
    monitor_error: Option<MonitorError>,
    shadow: Option<&'a mut DefaultMonitorShadowRuntime>,
}

// This monitor exposes unbacked ROM as all ones. Fetch-cache tokens for ROM
// point here, while RAM tokens point into the fixed physical direct map.
static MONITOR_UNBACKED_ROM: [u8; 4096] = [0xff; 4096];

impl<'a, 'b, D: Doorbell> MonitorSoftBus<'a, 'b, D> {
    pub fn new(
        ram: &'a mut MonitorRam,
        regions: RegionTableV1,
        client: &'a mut MailboxClient<'b>,
        doorbell: &'a mut D,
    ) -> Self {
        Self {
            ram,
            regions,
            client,
            doorbell,
            stats: X86MonitorBusStats::default(),
            monitor_error: None,
            shadow: None,
        }
    }

    pub fn new_with_shadow(
        ram: &'a mut MonitorRam,
        regions: RegionTableV1,
        client: &'a mut MailboxClient<'b>,
        doorbell: &'a mut D,
        shadow: &'a mut DefaultMonitorShadowRuntime,
    ) -> Self {
        Self {
            ram,
            regions,
            client,
            doorbell,
            stats: X86MonitorBusStats::default(),
            monitor_error: None,
            shadow: Some(shadow),
        }
    }

    pub fn stats(&self) -> X86MonitorBusStats {
        let mut stats = self.stats;
        if let Some(shadow) = self.shadow.as_deref() {
            let native = shadow.stats();
            stats.native_read_attempts = native.native_read_attempts;
            stats.native_read_hits = native.native_read_hits;
            stats.native_read_aborts = native.native_read_aborts;
            stats.native_read_descriptor_aborts = native.native_read_descriptor_aborts;
            stats.native_read_translation_aborts = native.native_read_translation_aborts;
            stats.native_read_access_flag_aborts = native.native_read_access_flag_aborts;
            stats.native_read_permission_aborts = native.native_read_permission_aborts;
        }
        stats
    }

    pub const fn transport_stats(&self) -> MonitorTransportStats {
        self.client.stats()
    }

    pub fn shadow_stats(&self) -> MonitorShadowRuntimeStats {
        self.shadow
            .as_deref()
            .map(|shadow| shadow.stats())
            .unwrap_or_default()
    }

    pub fn take_monitor_error(&mut self) -> Option<MonitorError> {
        self.monitor_error.take()
    }

    pub fn poll_host_command(
        &mut self,
        last_seen: u32,
    ) -> Result<Option<PendingCommand>, MonitorError> {
        self.client.poll_command(last_seen)
    }

    fn classify_region(&self, addr: PhysAddr, len: u32) -> PhysRegionKind {
        match self.regions.classify(addr.0, u64::from(len)) {
            Ok(RegionClassV1::Ram)
                if addr.0 <= self.ram.len && u64::from(len) <= self.ram.len - addr.0 =>
            {
                PhysRegionKind::Ram
            }
            Ok(RegionClassV1::Rom) => PhysRegionKind::Rom,
            Ok(RegionClassV1::Mmio) => PhysRegionKind::Mmio,
            Ok(RegionClassV1::FbDirtyTracked) => PhysRegionKind::FbDirtyTracked,
            Ok(RegionClassV1::Hole) => PhysRegionKind::Hole,
            Ok(RegionClassV1::Ram | RegionClassV1::Mixed) | Err(_) => PhysRegionKind::Mixed,
        }
    }

    fn record_monitor_error(&mut self, port: u16, err: MonitorError) -> MemError {
        self.monitor_error = Some(err);
        MemError::IoUnhandled { port }
    }
}

impl<D: Doorbell> Bus for MonitorSoftBus<'_, '_, D> {
    #[inline(always)]
    fn begin_instruction(&mut self, _instruction_count: u64, tsc: u64) {
        self.stats.last_vtime = tsc;
    }

    fn cached_backing_token(&self, addr: PhysAddr) -> BackingToken {
        let ptr = if self.classify_region(addr, 1) == PhysRegionKind::Rom {
            MONITOR_UNBACKED_ROM
                .as_ptr()
                .wrapping_add((addr.0 & 4095) as usize)
        } else {
            debug_assert!(addr.0 < self.ram.len);
            (self.ram.base as *const u8).wrapping_add(addr.0 as usize)
        };
        BackingToken(ptr as u64)
    }

    #[inline(always)]
    fn cached_backing_phys(&self, token: BackingToken) -> PhysAddr {
        // Only writable RAM tokens reach the tracked physical-write path.
        PhysAddr(token.0 - self.ram.base as u64)
    }

    #[inline(always)]
    fn read_backing_token_u8(&mut self, token: BackingToken) -> u8 {
        // Cache fill approved the complete page. The backing and region map
        // stay fixed for this bus's lifetime; cursor bounds prevent page spill.
        self.stats.phys_reads = self.stats.phys_reads.wrapping_add(1);
        unsafe { (token.0 as *const u8).read() }
    }

    #[inline(always)]
    fn read_backing_token_u16(&mut self, token: BackingToken) -> u16 {
        self.stats.phys_reads = self.stats.phys_reads.wrapping_add(2);
        unsafe { ram_read_u16(token.0 as *const u8) }
    }

    #[inline(always)]
    fn read_backing_token_u32(&mut self, token: BackingToken) -> u32 {
        self.stats.phys_reads = self.stats.phys_reads.wrapping_add(4);
        unsafe { ram_read_u32(token.0 as *const u8) }
    }

    #[inline(always)]
    fn write_backing_token_u8(&mut self, token: BackingToken, value: u8) -> Result<(), MemError> {
        // The common MMU has already tracked the write and checked permissions.
        self.stats.phys_writes = self.stats.phys_writes.wrapping_add(1);
        unsafe { (token.0 as *mut u8).write(value) };
        Ok(())
    }

    #[inline(always)]
    fn write_backing_token_u16(&mut self, token: BackingToken, value: u16) -> Result<(), MemError> {
        self.stats.phys_writes = self.stats.phys_writes.wrapping_add(2);
        unsafe { ram_write_u16(token.0 as *mut u8, value) };
        Ok(())
    }

    #[inline(always)]
    fn write_backing_token_u32(&mut self, token: BackingToken, value: u32) -> Result<(), MemError> {
        self.stats.phys_writes = self.stats.phys_writes.wrapping_add(4);
        unsafe { ram_write_u32(token.0 as *mut u8, value) };
        Ok(())
    }

    fn cached_ram_write_needs_fetch_invalidation(&self) -> bool {
        // Tokens read live RAM, so self-modifying code is visible immediately.
        // Page-table writes still invalidate CPU translations below.
        false
    }

    fn phys_ram_repeat_write_supported(&mut self, addr: PhysAddr, len: u32) -> bool {
        len != 0
            && self.classify_region(addr, len) == PhysRegionKind::Ram
            && len <= 4096 - (addr.0 & 4095) as u32
            && !self.translation_cache_flush_after_phys_write(PhysWrite::new(
                WriteOrigin::Cpu,
                addr,
                1,
            ))
    }

    fn write_phys_ram_repeat(
        &mut self,
        addr: PhysAddr,
        width: u8,
        value: u32,
        units: u32,
    ) -> Result<(), MemError> {
        let len = units
            .checked_mul(u32::from(width))
            .ok_or(MemError::OutOfRange { addr })?;
        if !matches!(width, 1 | 2 | 4) || !self.phys_ram_repeat_write_supported(addr, len) {
            return Err(MemError::OutOfRange { addr });
        }
        let ptr = (self.ram.base as *mut u8).wrapping_add(addr.0 as usize);
        // The shared interpreter bounded this chunk by page, segment and REP
        // restart limits. Tracking ran before commit. Active page-table frames
        // stay scalar because their writes can change the next unit's walk.
        unsafe {
            match width {
                1 => ptr.write_bytes(value as u8, len as usize),
                2 => {
                    for offset in 0..units as usize {
                        ptr.add(offset * 2)
                            .cast::<u16>()
                            .write_unaligned((value as u16).to_le());
                    }
                }
                4 => {
                    for offset in 0..units as usize {
                        ptr.add(offset * 4)
                            .cast::<u32>()
                            .write_unaligned(value.to_le());
                    }
                }
                _ => unreachable!(),
            }
        }
        self.stats.phys_writes = self.stats.phys_writes.wrapping_add(u64::from(len));
        Ok(())
    }

    fn phys_ram_copy_supported(&mut self, src: PhysAddr, dst: PhysAddr, len: u32) -> bool {
        self.phys_ram_repeat_write_supported(dst, len)
            && self.classify_region(src, len) == PhysRegionKind::Ram
            && (src.0.abs_diff(dst.0) >= u64::from(len))
    }

    fn copy_phys_to_ram(
        &mut self,
        src: PhysAddr,
        dst: PhysAddr,
        len: u32,
        _width: u8,
    ) -> Result<(), MemError> {
        if !self.phys_ram_copy_supported(src, dst, len) {
            return Err(MemError::OutOfRange { addr: dst });
        }
        let base = self.ram.base as *mut u8;
        // Both ranges are RAM, in bounds and disjoint; the common MMU has
        // already performed tracked-write notification for the entire chunk.
        unsafe {
            core::ptr::copy_nonoverlapping(
                base.add(src.0 as usize),
                base.add(dst.0 as usize),
                len as usize,
            )
        };
        self.stats.phys_reads = self.stats.phys_reads.wrapping_add(u64::from(len));
        self.stats.phys_writes = self.stats.phys_writes.wrapping_add(u64::from(len));
        Ok(())
    }

    fn read_phys_u8(&mut self, addr: PhysAddr) -> Result<u8, MemError> {
        self.stats.phys_reads = self.stats.phys_reads.wrapping_add(1);
        match self.classify_phys(addr, 1) {
            PhysRegionKind::Ram => self.ram.read_u8(addr),
            PhysRegionKind::Rom | PhysRegionKind::Hole => Ok(0xff),
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked => {
                match self
                    .client
                    .mmio_read(self.doorbell, addr.0, 1, self.stats.last_vtime)
                {
                    Ok(value) => {
                        self.stats.mmio_read = self.stats.mmio_read.wrapping_add(1);
                        self.stats.last_mmio_gpa = addr.0;
                        Ok(value as u8)
                    }
                    Err(err) => {
                        self.monitor_error = Some(err);
                        Err(MemError::Mmio { addr })
                    }
                }
            }
            PhysRegionKind::Mixed => Err(MemError::OutOfRange { addr }),
        }
    }

    fn read_phys_u16(&mut self, addr: PhysAddr) -> Result<u16, MemError> {
        self.stats.phys_reads = self.stats.phys_reads.wrapping_add(2);
        match self.classify_phys(addr, 2) {
            PhysRegionKind::Ram => self.ram.read_u16(addr),
            PhysRegionKind::Rom | PhysRegionKind::Hole => Ok(0xffff),
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked => {
                match self
                    .client
                    .mmio_read(self.doorbell, addr.0, 2, self.stats.last_vtime)
                {
                    Ok(value) => {
                        self.stats.mmio_read = self.stats.mmio_read.wrapping_add(1);
                        self.stats.last_mmio_gpa = addr.0;
                        Ok(value as u16)
                    }
                    Err(err) => {
                        self.monitor_error = Some(err);
                        Err(MemError::Mmio { addr })
                    }
                }
            }
            PhysRegionKind::Mixed => Err(MemError::OutOfRange { addr }),
        }
    }

    fn read_phys_u32(&mut self, addr: PhysAddr) -> Result<u32, MemError> {
        self.stats.phys_reads = self.stats.phys_reads.wrapping_add(4);
        match self.classify_phys(addr, 4) {
            PhysRegionKind::Ram => self.ram.read_u32(addr),
            PhysRegionKind::Rom | PhysRegionKind::Hole => Ok(0xffff_ffff),
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked => {
                match self
                    .client
                    .mmio_read(self.doorbell, addr.0, 4, self.stats.last_vtime)
                {
                    Ok(value) => {
                        self.stats.mmio_read = self.stats.mmio_read.wrapping_add(1);
                        self.stats.last_mmio_gpa = addr.0;
                        Ok(value as u32)
                    }
                    Err(err) => {
                        self.monitor_error = Some(err);
                        Err(MemError::Mmio { addr })
                    }
                }
            }
            PhysRegionKind::Mixed => Err(MemError::OutOfRange { addr }),
        }
    }

    fn read_phys_u64(&mut self, addr: PhysAddr) -> Result<u64, MemError> {
        self.stats.phys_reads = self.stats.phys_reads.wrapping_add(8);
        match self.classify_phys(addr, 8) {
            PhysRegionKind::Ram => self.ram.read_u64(addr),
            PhysRegionKind::Rom | PhysRegionKind::Hole => Ok(0xffff_ffff_ffff_ffff),
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked => Err(MemError::Mmio { addr }),
            PhysRegionKind::Mixed => Err(MemError::OutOfRange { addr }),
        }
    }

    fn probe_phys_write_u8(&mut self, addr: PhysAddr) -> Result<(), MemError> {
        self.stats.phys_write_probes = self.stats.phys_write_probes.wrapping_add(1);
        match self.classify_phys(addr, 1) {
            PhysRegionKind::Ram => self.ram.probe_write_u8(addr),
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked => Ok(()),
            PhysRegionKind::Rom | PhysRegionKind::Hole => Ok(()),
            PhysRegionKind::Mixed => Err(MemError::OutOfRange { addr }),
        }
    }

    fn write_phys_u8(&mut self, addr: PhysAddr, value: u8) -> Result<(), MemError> {
        self.stats.phys_writes = self.stats.phys_writes.wrapping_add(1);
        match self.classify_phys(addr, 1) {
            PhysRegionKind::Ram => self.ram.write_u8(addr, value),
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked => match self.client.mmio_write(
                self.doorbell,
                addr.0,
                1,
                u64::from(value),
                self.stats.last_vtime,
            ) {
                Ok(()) => {
                    self.stats.mmio_write = self.stats.mmio_write.wrapping_add(1);
                    self.stats.last_mmio_gpa = addr.0;
                    Ok(())
                }
                Err(err) => {
                    self.monitor_error = Some(err);
                    Err(MemError::Mmio { addr })
                }
            },
            PhysRegionKind::Rom | PhysRegionKind::Hole => Ok(()),
            PhysRegionKind::Mixed => Err(MemError::OutOfRange { addr }),
        }
    }

    fn write_phys_u16(&mut self, addr: PhysAddr, value: u16) -> Result<(), MemError> {
        self.stats.phys_writes = self.stats.phys_writes.wrapping_add(2);
        match self.classify_phys(addr, 2) {
            PhysRegionKind::Ram => self.ram.write_u16(addr, value),
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked => match self.client.mmio_write(
                self.doorbell,
                addr.0,
                2,
                u64::from(value),
                self.stats.last_vtime,
            ) {
                Ok(()) => {
                    self.stats.mmio_write = self.stats.mmio_write.wrapping_add(1);
                    self.stats.last_mmio_gpa = addr.0;
                    Ok(())
                }
                Err(err) => {
                    self.monitor_error = Some(err);
                    Err(MemError::Mmio { addr })
                }
            },
            PhysRegionKind::Rom | PhysRegionKind::Hole => Ok(()),
            PhysRegionKind::Mixed => Err(MemError::OutOfRange { addr }),
        }
    }

    fn write_phys_u32(&mut self, addr: PhysAddr, value: u32) -> Result<(), MemError> {
        self.stats.phys_writes = self.stats.phys_writes.wrapping_add(4);
        match self.classify_phys(addr, 4) {
            PhysRegionKind::Ram => self.ram.write_u32(addr, value),
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked => match self.client.mmio_write(
                self.doorbell,
                addr.0,
                4,
                u64::from(value),
                self.stats.last_vtime,
            ) {
                Ok(()) => {
                    self.stats.mmio_write = self.stats.mmio_write.wrapping_add(1);
                    self.stats.last_mmio_gpa = addr.0;
                    Ok(())
                }
                Err(err) => {
                    self.monitor_error = Some(err);
                    Err(MemError::Mmio { addr })
                }
            },
            PhysRegionKind::Rom | PhysRegionKind::Hole => Ok(()),
            PhysRegionKind::Mixed => Err(MemError::OutOfRange { addr }),
        }
    }

    fn write_phys_u64(&mut self, addr: PhysAddr, value: u64) -> Result<(), MemError> {
        self.stats.phys_writes = self.stats.phys_writes.wrapping_add(8);
        match self.classify_phys(addr, 8) {
            PhysRegionKind::Ram => self.ram.write_u64(addr, value),
            PhysRegionKind::Mmio | PhysRegionKind::FbDirtyTracked => Err(MemError::Mmio { addr }),
            PhysRegionKind::Rom | PhysRegionKind::Hole => Ok(()),
            PhysRegionKind::Mixed => Err(MemError::OutOfRange { addr }),
        }
    }

    fn classify_phys(&self, addr: PhysAddr, len: u32) -> PhysRegionKind {
        self.classify_region(addr, len)
    }

    fn note_page_table_frame(
        &mut self,
        frame: crate::x86::PhysFrame,
        _role: crate::x86::PageTableFrameRole,
    ) -> Result<bool, MemError> {
        self.stats.page_table_frames = self.stats.page_table_frames.wrapping_add(1);
        Ok(self
            .shadow
            .as_deref_mut()
            .is_some_and(|shadow| shadow.core.track_frame(frame)))
    }

    fn cached_write_translation_allowed(&self, phys: PhysAddr) -> bool {
        // A page table belonging to any resident space must keep tracked stores,
        // including writes made while its owner is inactive.
        self.shadow.as_deref().is_some_and(|shadow| {
            !shadow
                .core
                .frame_has_users(crate::x86::PhysFrame::containing(phys))
        })
    }

    #[inline(always)]
    fn before_cached_ram_write(&mut self, _write: PhysWrite) -> Result<(), MemError> {
        // First discovery of a page-table frame revokes its writable CPU tokens.
        // Such frames are then excluded from writable fills above.
        Ok(())
    }

    fn cached_ram_write_needs_dynamic_translation_flush(&self) -> bool {
        false
    }

    fn before_phys_write(&mut self, write: PhysWrite) -> Result<(), MemError> {
        self.stats.tracked_writes = self.stats.tracked_writes.wrapping_add(1);
        if let Some(shadow) = self.shadow.as_deref_mut() {
            shadow.observe_phys_write(write);
        }
        Ok(())
    }

    fn linear_translation_cache_enabled(&self) -> bool {
        self.shadow.is_some()
    }

    fn native_read_backing(
        &mut self,
        linear: LinearAddr,
        priv_: AccessPriv,
    ) -> Option<(BackingToken, TranslationPageSize)> {
        let (phys, page_size) = self
            .shadow
            .as_deref()?
            .native_hit_translation(linear, priv_)?;
        Some((self.cached_backing_token(phys), page_size))
    }

    fn translation_cache_flush_after_phys_write(&self, write: PhysWrite) -> bool {
        self.shadow
            .as_deref()
            .is_some_and(|shadow| shadow.write_touches_page_table_frame(write))
    }

    fn after_linear_translate(
        &mut self,
        linear: LinearAddr,
        access: AccessKind,
        priv_: AccessPriv,
        cr0: u32,
        cr3: u32,
        cr4: u32,
        eflags: u32,
        translation: Translation,
    ) {
        if let Some(shadow) = self.shadow.as_deref_mut() {
            shadow.after_linear_translate(
                &self.regions,
                linear,
                access,
                priv_,
                cr0,
                cr3,
                cr4,
                eflags,
                translation,
            );
        }
    }

    fn try_native_read_linear_u8(
        &mut self,
        linear: LinearAddr,
        access: AccessKind,
        priv_: AccessPriv,
        cr0: u32,
        cr3: u32,
        cr4: u32,
        eflags: u32,
    ) -> NativeRead<u8> {
        self.shadow
            .as_deref_mut()
            .map_or(NativeRead::Unsupported, |shadow| {
                shadow.try_native_read_linear_u8(linear, access, priv_, cr0, cr3, cr4, eflags)
            })
    }

    fn try_native_read_linear_u16(
        &mut self,
        linear: LinearAddr,
        access: AccessKind,
        priv_: AccessPriv,
        cr0: u32,
        cr3: u32,
        cr4: u32,
        eflags: u32,
    ) -> NativeRead<u16> {
        self.shadow
            .as_deref_mut()
            .map_or(NativeRead::Unsupported, |shadow| {
                shadow.try_native_read_linear_u16(linear, access, priv_, cr0, cr3, cr4, eflags)
            })
    }

    #[inline(always)]
    fn try_native_read_linear_u32(
        &mut self,
        linear: LinearAddr,
        access: AccessKind,
        priv_: AccessPriv,
        cr0: u32,
        cr3: u32,
        cr4: u32,
        eflags: u32,
    ) -> NativeRead<u32> {
        self.shadow
            .as_deref_mut()
            .map_or(NativeRead::Unsupported, |shadow| {
                shadow.try_native_read_linear_u32(linear, access, priv_, cr0, cr3, cr4, eflags)
            })
    }

    fn after_control_register_write(&mut self, index: u8, old: u32, new: u32) {
        if let Some(shadow) = self.shadow.as_deref_mut() {
            shadow.flush_for_control_write(index, old, new);
        }
    }

    fn after_invlpg(&mut self, linear: LinearAddr) {
        if let Some(shadow) = self.shadow.as_deref_mut() {
            shadow.flush_for_invlpg(linear);
        }
    }

    fn io_read_u8(&mut self, port: u16) -> Result<u8, MemError> {
        match self
            .client
            .pio_in(self.doorbell, port, 1, self.stats.last_vtime)
        {
            Ok(read) => {
                self.stats.pio_in = self.stats.pio_in.wrapping_add(1);
                self.stats.last_pio_port_dir = u32::from(port);
                Ok(read.value as u8)
            }
            Err(err) => Err(self.record_monitor_error(port, err)),
        }
    }

    fn io_write_u8(&mut self, port: u16, value: u8) -> Result<(), MemError> {
        match self.client.pio_out(
            self.doorbell,
            port,
            1,
            u32::from(value),
            self.stats.last_vtime,
        ) {
            Ok(()) => {
                self.stats.pio_out = self.stats.pio_out.wrapping_add(1);
                self.stats.last_pio_port_dir = u32::from(port) | (1 << 31);
                Ok(())
            }
            Err(err) => Err(self.record_monitor_error(port, err)),
        }
    }
}

pub const fn x86_monitor_smoke_program() -> [u8; X86_MONITOR_SMOKE_PROGRAM_LEN] {
    let mut out = [0_u8; X86_MONITOR_SMOKE_PROGRAM_LEN];
    let mut at = 0;
    out[at] = 0xba;
    at += 1;
    out[at] = 0xf8;
    at += 1;
    out[at] = 0x03;
    at += 1;
    out[at] = 0x00;
    at += 1;
    out[at] = 0x00;
    at += 1;
    let mut index = 0;
    while index < X86_MONITOR_SMOKE_MARKER.len() {
        out[at] = 0xb0;
        out[at + 1] = X86_MONITOR_SMOKE_MARKER[index];
        out[at + 2] = 0xee;
        at += 3;
        index += 1;
    }
    out[at] = 0xf4;
    out
}

pub fn install_x86_monitor_smoke_program(ram: &mut MonitorRam) -> Result<(), X86MonitorError> {
    let program = x86_monitor_smoke_program();
    let base = PhysAddr(u64::from(X86_MONITOR_SMOKE_BASE));
    let end =
        base.0
            .checked_add(program.len() as u64)
            .ok_or(X86MonitorError::ProgramOutOfRange {
                base,
                len: program.len(),
                ram_len: ram.len(),
            })?;
    if end > ram.len() {
        return Err(X86MonitorError::ProgramOutOfRange {
            base,
            len: program.len(),
            ram_len: ram.len(),
        });
    }
    ram.write_bytes(base, &program)
}

/// Warm A/B, revisit A, modify A's PTE while B runs, revisit both, then
/// rewrite the active self-map. Register results distinguish every mapping.
pub const fn hardmmu_paged_smoke_program() -> [u8; HARDMMU_PAGED_SMOKE_PROGRAM_LEN] {
    [
        0x8b, 0x05, 0x00, 0x40, 0x00, 0x00, 0x8b, 0x1d, 0x00, 0x40, 0x00, 0x00, 0xb9, 0x00, 0x50,
        0x00, 0x00, 0x0f, 0x22, 0xd9, 0x8b, 0x15, 0x00, 0x40, 0x00, 0x00, 0x8b, 0x15, 0x00, 0x40,
        0x00, 0x00, 0xb9, 0x00, 0x20, 0x00, 0x00, 0x0f, 0x22, 0xd9, 0x8b, 0x35, 0x00, 0x40, 0x00,
        0x00, 0xb9, 0x00, 0x50, 0x00, 0x00, 0x0f, 0x22, 0xd9, 0x8b, 0x15, 0x00, 0x40, 0x00, 0x00,
        0xc7, 0x05, 0x10, 0x30, 0x00, 0x00, 0x03, 0x80, 0x00, 0x00, 0xb9, 0x00, 0x20, 0x00, 0x00,
        0x0f, 0x22, 0xd9, 0x8b, 0x3d, 0x00, 0x40, 0x00, 0x00, 0x8b, 0x3d, 0x00, 0x40, 0x00, 0x00,
        0xb9, 0x00, 0x50, 0x00, 0x00, 0x0f, 0x22, 0xd9, 0x8b, 0x15, 0x00, 0x40, 0x00, 0x00, 0xb9,
        0x00, 0x20, 0x00, 0x00, 0x0f, 0x22, 0xd9, 0x8b, 0x3d, 0x00, 0x40, 0x00, 0x00, 0xc7, 0x05,
        0x10, 0x30, 0x00, 0x00, 0x03, 0x40, 0x00, 0x00, 0x8b, 0x2d, 0x00, 0x40, 0x00, 0x00, 0x8b,
        0x2d, 0x00, 0x40, 0x00, 0x00, 0xf4,
    ]
}

pub fn hardmmu_paged_smoke_registers_match(cpu: &Cpu) -> bool {
    cpu.reg(Reg32::Eax) == HARDMMU_PAGED_DATA_VALUE
        && cpu.reg(Reg32::Ebx) == HARDMMU_PAGED_DATA_VALUE
        && cpu.reg(Reg32::Esi) == HARDMMU_PAGED_DATA_VALUE
        && cpu.reg(Reg32::Edx) == HARDMMU_CONTEXT_B_VALUE
        && cpu.reg(Reg32::Edi) == HARDMMU_CONTEXT_C_VALUE
        && cpu.reg(Reg32::Ebp) == HARDMMU_PAGED_DATA_VALUE
}

fn hardmmu_paged_zero_page(ram: &mut MonitorRam, base: u32) -> Result<(), X86MonitorError> {
    let mut offset = 0_u64;
    while offset < PAGE_SIZE_4K {
        ram.write_u8(PhysAddr(u64::from(base) + offset), 0)?;
        offset += 1;
    }
    Ok(())
}

fn hardmmu_paged_write_u32(
    ram: &mut MonitorRam,
    addr: u32,
    value: u32,
) -> Result<(), X86MonitorError> {
    ram.write_bytes(PhysAddr(u64::from(addr)), &value.to_le_bytes())
}

pub fn install_hardmmu_paged_smoke_image(ram: &mut MonitorRam) -> Result<(), X86MonitorError> {
    let program = hardmmu_paged_smoke_program();
    ram.write_bytes(PhysAddr(u64::from(HARDMMU_PAGED_CODE_BASE)), &program)?;
    hardmmu_paged_write_u32(ram, HARDMMU_PAGED_DATA_BASE, HARDMMU_PAGED_DATA_VALUE)?;
    hardmmu_paged_write_u32(ram, 0x7000, HARDMMU_CONTEXT_B_VALUE)?;
    hardmmu_paged_write_u32(ram, 0x8000, HARDMMU_CONTEXT_C_VALUE)?;
    for (pd, pt, data) in [
        (
            HARDMMU_PAGED_PD_BASE,
            HARDMMU_PAGED_PT_BASE,
            HARDMMU_PAGED_DATA_BASE,
        ),
        (0x5000, 0x6000, 0x7000),
    ] {
        hardmmu_paged_zero_page(ram, pd)?;
        hardmmu_paged_zero_page(ram, pt)?;
        hardmmu_paged_write_u32(ram, pd, pt | 3)?;
        hardmmu_paged_write_u32(ram, pt + 4, HARDMMU_PAGED_CODE_BASE | 3)?;
        hardmmu_paged_write_u32(ram, pt + 12, HARDMMU_PAGED_PT_BASE | 3)?;
        hardmmu_paged_write_u32(ram, pt + 16, data | 3)?;
    }
    Ok(())
}

pub fn hardmmu_paged_smoke_cpu() -> Cpu {
    let mut cpu = Cpu::new();
    cpu.enter_protected_flat(HARDMMU_PAGED_CODE_BASE, 0);
    cpu.cr0 = CR0_PE | CR0_PG;
    cpu.cr3 = HARDMMU_PAGED_PD_BASE;
    cpu.cr4 = 0;
    cpu
}

fn install_hardmmu_large_smoke_image(ram: &mut MonitorRam) -> Result<(), X86MonitorError> {
    // Code and both PDE aliases use small pages. Data uses both halves of
    // x86 4 MiB leaves, including an inactive and then an active PDE rewrite.
    const PROGRAM: &[u8] = &[
        0x8b, 0x05, 0x01, 0x01, 0x40, 0x00, 0x8b, 0x1d, 0x01, 0xf1, 0x5f, 0x00, 0x8b, 0x35, 0x01,
        0x01, 0x60, 0x00, 0x8b, 0x3d, 0x01, 0xf1, 0x7f, 0x00, 0xb9, 0x00, 0x50, 0x00, 0x00, 0x0f,
        0x22, 0xd9, 0x8b, 0x15, 0x01, 0x01, 0x40, 0x00, 0x8b, 0x15, 0x01, 0xf1, 0x5f, 0x00, 0xc7,
        0x05, 0x04, 0x20, 0x00, 0x00, 0x83, 0x00, 0xc0, 0x00, 0xb9, 0x00, 0x20, 0x00, 0x00, 0x0f,
        0x22, 0xd9, 0x8b, 0x2d, 0x01, 0x01, 0x40, 0x00, 0x8b, 0x2d, 0x01, 0xf1, 0x5f, 0x00, 0x8b,
        0x0d, 0x00, 0x00, 0xc0, 0x00, 0x8b, 0x0d, 0x00, 0x00, 0xc0, 0x00, 0x0f, 0x01, 0x3d, 0x01,
        0x01, 0x40, 0x00, 0x8b, 0x0d, 0x00, 0x00, 0xc0, 0x00, 0x8b, 0x2d, 0x01, 0x01, 0x40, 0x00,
        0x8b, 0x2d, 0x01, 0xf1, 0x5f, 0x00, 0xc7, 0x05, 0x04, 0x20, 0x00, 0x00, 0x83, 0x00, 0x40,
        0x00, 0x8b, 0x0d, 0x01, 0x01, 0x40, 0x00, 0x8b, 0x0d, 0x01, 0xf1, 0x5f, 0x00, 0xf4,
    ];
    ram.write_bytes(PhysAddr(0x1000), PROGRAM)?;
    // Warm an unrelated 4 KiB mapping before INVLPG, then read it immediately
    // afterward. Pass 3 explicitly evicts the software TLB to exercise the
    // retained ARM mapping; normal retention may hit the software cache first.
    hardmmu_paged_zero_page(ram, 0x8000)?;
    hardmmu_paged_write_u32(ram, 0x8000, 0x7003)?;
    hardmmu_paged_write_u32(ram, 0x7000, 0x12345678)?;
    for (pd, pt, data) in [(0x2000, 0x3000, 0x400000), (0x5000, 0x6000, 0x800000)] {
        hardmmu_paged_zero_page(ram, pd)?;
        hardmmu_paged_zero_page(ram, pt)?;
        hardmmu_paged_write_u32(ram, pd, pt | 3)?;
        hardmmu_paged_write_u32(ram, pd + 4, data | 0x83)?;
        hardmmu_paged_write_u32(ram, pd + 12, 0x8003)?;
        hardmmu_paged_write_u32(ram, pt + 4, 0x1003)?;
        hardmmu_paged_write_u32(ram, pt + 8, 0x2003)?;
    }
    for (base, value) in [
        (0x400000, 0xaabb0000),
        (0x800000, 0xbbcc0000),
        (0xc00000, 0xccdd0000),
    ] {
        for (offset, index) in [(0x101, 1), (0x1ff101, 2), (0x200101, 3), (0x3ff101, 4)] {
            hardmmu_paged_write_u32(ram, base + offset, value | index)?;
        }
    }
    Ok(())
}

pub fn hardmmu_large_paged_smoke_registers_match(cpu: &Cpu) -> bool {
    cpu.reg(Reg32::Eax) == 0xaabb0001
        && cpu.reg(Reg32::Ebx) == 0xaabb0002
        && cpu.reg(Reg32::Esi) == 0xaabb0003
        && cpu.reg(Reg32::Edi) == 0xaabb0004
        && cpu.reg(Reg32::Edx) == 0xbbcc0002
        && cpu.reg(Reg32::Ebp) == 0xccdd0002
        && cpu.reg(Reg32::Ecx) == 0xaabb0002
}

pub fn run_hardmmu_large_paged_smoke<D: Doorbell>(
    client: &mut MailboxClient<'_>,
    doorbell: &mut D,
    ram: &mut MonitorRam,
    regions: RegionTableV1,
    cpu: &mut Cpu,
    shadow: &mut DefaultMonitorShadowRuntime,
    max_steps: u64,
) -> Result<X86MonitorStepReport, X86MonitorError> {
    run_hardmmu_paged_regression(client, doorbell, ram, regions, cpu, shadow, max_steps, true)
}

pub fn run_hardmmu_paged_smoke<D: Doorbell>(
    client: &mut MailboxClient<'_>,
    doorbell: &mut D,
    ram: &mut MonitorRam,
    regions: RegionTableV1,
    cpu: &mut Cpu,
    shadow: &mut DefaultMonitorShadowRuntime,
    max_steps: u64,
) -> Result<X86MonitorStepReport, X86MonitorError> {
    run_hardmmu_paged_regression(
        client, doorbell, ram, regions, cpu, shadow, max_steps, false,
    )
}

fn run_hardmmu_paged_regression<D: Doorbell>(
    client: &mut MailboxClient<'_>,
    doorbell: &mut D,
    ram: &mut MonitorRam,
    regions: RegionTableV1,
    cpu: &mut Cpu,
    shadow: &mut DefaultMonitorShadowRuntime,
    max_steps: u64,
    large: bool,
) -> Result<X86MonitorStepReport, X86MonitorError> {
    assert!(max_steps <= HARDMMU_PAGED_SMOKE_MAX_STEPS);
    // This bounded regression image writes only within its first 36 KiB.
    // Compare complete CPU records and every byte of that RAM after each
    // instruction with native-cache eviction, normal retention, and finally
    // software-cache eviction so native reuse is exercised even with L1 hits.
    let mut reference =
        [(abi::CpuStateV1::ZERO, [0_u8; 0x9000]); HARDMMU_PAGED_SMOKE_MAX_STEPS as usize];
    let mut report = None;
    for pass in 0..4 {
        if large {
            install_hardmmu_large_smoke_image(ram)?;
        } else {
            install_hardmmu_paged_smoke_image(ram)?;
        }
        *cpu = hardmmu_paged_smoke_cpu();
        if large {
            cpu.cr4 = crate::x86::cpu::CR4_PSE;
        }
        shadow.reset(cpu);
        let mut bus = if pass == 0 {
            MonitorSoftBus::new(ram, regions, client, doorbell)
        } else {
            MonitorSoftBus::new_with_shadow(ram, regions, client, doorbell, shadow)
        };
        let mut random = 0x1234_5678_u32;
        let mut steps = 0;
        let mut check_retained_read = false;
        while steps < max_steps {
            if pass == 3 {
                // Deliberate test-only software eviction leaves the resident
                // ARM translations intact; the workload and x86 state agree.
                cpu.invalidate_translation_context();
            }
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            if pass == 1 && random & 3 == 0 {
                // An arbitrary native-cache eviction must leave the CPU's
                // software-cache dependency tracking intact.
                bus.shadow
                    .as_deref_mut()
                    .unwrap()
                    .stage1_mut()
                    .flush_active();
            }
            bus.stats.last_vtime = cpu.tsc();
            let before = bus.shadow.as_deref().map(|runtime| runtime.stats());
            Interpreter::step(cpu, &mut bus).map_err(|error| X86MonitorError::Step {
                steps: steps as u32,
                eip: cpu.eip,
                error,
            })?;
            if large && pass >= 2 {
                let runtime = bus.shadow.as_deref_mut().unwrap();
                let before = before.unwrap();
                if check_retained_read {
                    assert_eq!(cpu.reg(Reg32::Ecx), 0x12345678);
                    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
                    if pass == 3 {
                        // Scoped INVLPG leaves unrelated software entries in
                        // pass 2. Only this deliberately cold software pass
                        // must reach the retained native mapping itself.
                        assert_eq!(
                            runtime.stats().native_read_hits,
                            before.native_read_hits + 1,
                            "unrelated mapping must supply a native read after INVLPG"
                        );
                    }
                    check_retained_read = false;
                }
                if runtime.stats().invlpg_flushes != before.invlpg_flushes {
                    assert!(
                        runtime
                            .descriptor_for(ShadowWindow::Supervisor, LinearAddr(0xc00000))
                            .is_some()
                    );
                    for linear in [0x400101, 0x600101] {
                        assert!(
                            runtime
                                .descriptor_for(ShadowWindow::Supervisor, LinearAddr(linear))
                                .is_none()
                        );
                    }
                    check_retained_read = true;
                }
            }
            let state = cpu_state_v1_from_cpu(cpu, steps + 1);
            let ptr = bus.ram.checked_range_ptr(PhysAddr(0), 0x9000)?;
            // The bounded direct map remains live, and this reference is used
            // only between instructions, while no guest writes can occur.
            let bytes = unsafe { core::slice::from_raw_parts(ptr, 0x9000) };
            let expected = &mut reference[steps as usize];
            if pass == 0 {
                expected.0 = state;
                expected.1.copy_from_slice(bytes);
            } else {
                assert_eq!(
                    state, expected.0,
                    "CPU differs at pass {pass}, step {steps}"
                );
                assert_eq!(
                    bytes, expected.1,
                    "RAM differs at pass {pass}, step {steps}"
                );
            }
            steps += 1;
            if cpu.halted {
                break;
            }
        }
        if large {
            assert!(cpu.halted && hardmmu_large_paged_smoke_registers_match(cpu));
            if pass != 0 {
                assert!(bus.shadow.as_deref().unwrap().stats().upgrades >= 4);
            }
            #[cfg(all(target_arch = "aarch64", target_os = "none"))]
            if pass == 3 {
                assert!(
                    bus.stats().native_read_hits >= 6,
                    "large leaves must supply native reads"
                );
            }
        }
        report = Some(X86MonitorStepReport {
            steps,
            halted: cpu.halted,
            eip: cpu.eip,
            tsc: cpu.tsc(),
            bus: bus.stats(),
        });
    }
    Ok(report.unwrap())
}

pub fn cpu_from_state_v1(state: &crate::monitor::abi::CpuStateV1) -> Cpu {
    let mut cpu = Cpu::new();
    cpu.regs = state.gpr;
    cpu.eip = state.eip;
    cpu.replace_eflags(state.eflags);
    cpu.cr0 = state.cr0;
    cpu.cr2 = state.cr2;
    cpu.cr3 = state.cr3;
    cpu.cr4 = state.cr4;
    cpu.pae_pdptrs = state.pae_pdptrs;
    cpu.dr = state.dr;
    cpu.set_seg_cache(SegmentReg::Es, segment_cache_from_state(state.seg[0]));
    cpu.set_seg_cache(SegmentReg::Cs, segment_cache_from_state(state.seg[1]));
    cpu.set_seg_cache(SegmentReg::Ss, segment_cache_from_state(state.seg[2]));
    cpu.set_seg_cache(SegmentReg::Ds, segment_cache_from_state(state.seg[3]));
    cpu.set_seg_cache(SegmentReg::Fs, segment_cache_from_state(state.seg[4]));
    cpu.set_seg_cache(SegmentReg::Gs, segment_cache_from_state(state.seg[5]));
    cpu.gdtr = table_reg_from_state(state.gdtr);
    cpu.idtr = table_reg_from_state(state.idtr);
    cpu.ldtr = state.ldtr.selector;
    cpu.ldtr_cache = segment_cache_from_state(state.ldtr);
    cpu.tr = state.tr.selector;
    cpu.tr_cache = segment_cache_from_state(state.tr);
    cpu.msr_apic_base = state.msr.apic_base;
    cpu.msr_test_ctl = state.msr.test_ctl;
    cpu.msr_bios_update_trigger = state.msr.bios_update_trigger;
    cpu.msr_bbl_cr_data = state.msr.bbl_cr_data;
    cpu.msr_bbl_cr_addr = state.msr.bbl_cr_addr;
    cpu.msr_bbl_cr_decc = state.msr.bbl_cr_decc;
    cpu.msr_bbl_cr_ctl = state.msr.bbl_cr_ctl;
    cpu.msr_bbl_cr_trig = state.msr.bbl_cr_trig;
    cpu.msr_bbl_cr_busy = state.msr.bbl_cr_busy;
    cpu.msr_bbl_cr_ctl3 = state.msr.bbl_cr_ctl3;
    cpu.msr_misc_enable = state.msr.misc_enable;
    cpu.msr_perfctr = state.msr.perfctr;
    cpu.msr_evntsel = state.msr.evntsel;
    cpu.msr_mtrr_phys = state.msr.mtrr_phys;
    cpu.msr_mtrr_fix64k_00000 = state.msr.mtrr_fix64k_00000;
    cpu.msr_mtrr_fix16k = state.msr.mtrr_fix16k;
    cpu.msr_mtrr_fix4k = state.msr.mtrr_fix4k;
    cpu.msr_mtrr_def_type = state.msr.mtrr_def_type;
    cpu.msr_pat = state.msr.pat;
    cpu.msr_mcg_ctl = state.msr.mcg_ctl;
    cpu.msr_mca_ctl = state.msr.mca_ctl;
    cpu.msr_sysenter_cs = state.msr.sysenter_cs;
    cpu.msr_sysenter_esp = state.msr.sysenter_esp;
    cpu.msr_sysenter_eip = state.msr.sysenter_eip;
    cpu.set_tsc(state.tsc);
    cpu.fpu.control = state.fpu.control;
    cpu.fpu.status = state.fpu.status;
    cpu.fpu.top = state.fpu.top & 7;
    cpu.fpu.mxcsr = state.fpu.mxcsr;
    for (dst, bits) in cpu.fpu.st.iter_mut().zip(state.fpu.st_bits) {
        *dst = f64::from_bits(bits);
    }
    cpu.fpu.mm = state.fpu.mm;
    cpu.fpu.mmx_active = state.fpu.mmx_active != 0;
    cpu.halted = state.halted != 0;
    cpu.restore_interrupt_shadow(state.interrupt_shadow != 0);
    cpu
}

pub fn cpu_state_v1_from_cpu(cpu: &Cpu, instr_count: u64) -> crate::monitor::abi::CpuStateV1 {
    let mut state = crate::monitor::abi::CpuStateV1::ZERO;
    state.gpr = cpu.regs;
    state.eip = cpu.eip;
    state.eflags = cpu.effective_eflags();
    state.cr0 = cpu.cr0;
    state.cr2 = cpu.cr2;
    state.cr3 = cpu.cr3;
    state.cr4 = cpu.cr4;
    state.pae_pdptrs = cpu.pae_pdptrs;
    state.dr = cpu.dr;
    state.cpl = cpu.cpl();
    state.halted = u8::from(cpu.halted);
    state.interrupt_shadow = u8::from(cpu.interrupt_shadow());
    state.seg[0] = segment_state_from_cache(cpu.seg(SegmentReg::Es));
    state.seg[1] = segment_state_from_cache(cpu.seg(SegmentReg::Cs));
    state.seg[2] = segment_state_from_cache(cpu.seg(SegmentReg::Ss));
    state.seg[3] = segment_state_from_cache(cpu.seg(SegmentReg::Ds));
    state.seg[4] = segment_state_from_cache(cpu.seg(SegmentReg::Fs));
    state.seg[5] = segment_state_from_cache(cpu.seg(SegmentReg::Gs));
    state.gdtr = descriptor_table_from_reg(cpu.gdtr);
    state.idtr = descriptor_table_from_reg(cpu.idtr);
    state.ldtr = segment_state_from_cache(cpu.ldtr_cache);
    state.tr = segment_state_from_cache(cpu.tr_cache);
    state.msr.apic_base = cpu.msr_apic_base;
    state.msr.test_ctl = cpu.msr_test_ctl;
    state.msr.bios_update_trigger = cpu.msr_bios_update_trigger;
    state.msr.bbl_cr_data = cpu.msr_bbl_cr_data;
    state.msr.bbl_cr_addr = cpu.msr_bbl_cr_addr;
    state.msr.bbl_cr_decc = cpu.msr_bbl_cr_decc;
    state.msr.bbl_cr_ctl = cpu.msr_bbl_cr_ctl;
    state.msr.bbl_cr_trig = cpu.msr_bbl_cr_trig;
    state.msr.bbl_cr_busy = cpu.msr_bbl_cr_busy;
    state.msr.bbl_cr_ctl3 = cpu.msr_bbl_cr_ctl3;
    state.msr.misc_enable = cpu.msr_misc_enable;
    state.msr.perfctr = cpu.msr_perfctr;
    state.msr.evntsel = cpu.msr_evntsel;
    state.msr.mtrr_phys = cpu.msr_mtrr_phys;
    state.msr.mtrr_fix64k_00000 = cpu.msr_mtrr_fix64k_00000;
    state.msr.mtrr_fix16k = cpu.msr_mtrr_fix16k;
    state.msr.mtrr_fix4k = cpu.msr_mtrr_fix4k;
    state.msr.mtrr_def_type = cpu.msr_mtrr_def_type;
    state.msr.pat = cpu.msr_pat;
    state.msr.mcg_ctl = cpu.msr_mcg_ctl;
    state.msr.mca_ctl = cpu.msr_mca_ctl;
    state.msr.sysenter_cs = cpu.msr_sysenter_cs;
    state.msr.sysenter_esp = cpu.msr_sysenter_esp;
    state.msr.sysenter_eip = cpu.msr_sysenter_eip;
    state.tsc = cpu.tsc();
    state.instr_count = instr_count;
    state.fpu.control = cpu.fpu.control;
    state.fpu.status = cpu.fpu.status;
    state.fpu.top = cpu.fpu.top & 7;
    state.fpu.mmx_active = u8::from(cpu.fpu.mmx_active);
    state.fpu.mxcsr = cpu.fpu.mxcsr;
    for (dst, value) in state.fpu.st_bits.iter_mut().zip(cpu.fpu.st) {
        *dst = value.to_bits();
    }
    state.fpu.mm = cpu.fpu.mm;
    state
}

pub fn x86_monitor_smoke_cpu(initial: &crate::monitor::abi::CpuStateV1) -> Cpu {
    let mut cpu = cpu_from_state_v1(initial);
    cpu.eip = X86_MONITOR_SMOKE_BASE;
    cpu.halted = false;
    cpu.segment_override = None;
    cpu.set_reg(Reg32::Edx, 0);
    cpu.set_reg(Reg32::Eax, 0);
    cpu
}

pub fn run_x86_monitor_smoke<D: Doorbell>(
    client: &mut MailboxClient<'_>,
    doorbell: &mut D,
    ram: &mut MonitorRam,
    regions: RegionTableV1,
    initial: &crate::monitor::abi::CpuStateV1,
    max_steps: u32,
) -> Result<X86MonitorRunReport, X86MonitorError> {
    install_x86_monitor_smoke_program(ram)?;
    let mut cpu = x86_monitor_smoke_cpu(initial);
    run_x86_monitor_cpu(client, doorbell, ram, regions, &mut cpu, max_steps)
}

pub fn run_x86_monitor_cpu<D: Doorbell>(
    client: &mut MailboxClient<'_>,
    doorbell: &mut D,
    ram: &mut MonitorRam,
    regions: RegionTableV1,
    cpu: &mut Cpu,
    max_steps: u32,
) -> Result<X86MonitorRunReport, X86MonitorError> {
    let mut bus = MonitorSoftBus::new(ram, regions, client, doorbell);
    let mut steps = 0_u32;
    while steps < max_steps {
        bus.stats.last_vtime = cpu.tsc();
        let outcome = Interpreter::step(cpu, &mut bus);
        match outcome {
            Ok(StepOutcome::Running | StepOutcome::Attention) => {
                steps = steps.wrapping_add(1);
            }
            Ok(StepOutcome::Halted) => {
                steps = steps.wrapping_add(1);
                return Ok(X86MonitorRunReport {
                    steps,
                    halted: true,
                    eip: cpu.eip,
                    tsc: cpu.tsc(),
                    bus: bus.stats(),
                });
            }
            Err(error) => {
                if let Some(err) = bus.take_monitor_error() {
                    return Err(X86MonitorError::Monitor(err));
                }
                return Err(X86MonitorError::Step {
                    steps,
                    eip: cpu.eip,
                    error,
                });
            }
        }
    }
    Err(X86MonitorError::StepLimit {
        max_steps,
        eip: cpu.eip,
    })
}

pub fn run_x86_monitor_step_window<D: Doorbell>(
    client: &mut MailboxClient<'_>,
    doorbell: &mut D,
    ram: &mut MonitorRam,
    regions: RegionTableV1,
    cpu: &mut Cpu,
    max_steps: u64,
    timer_poll_interval: u64,
) -> Result<X86MonitorStepReport, X86MonitorError> {
    let mut bus = MonitorSoftBus::new(ram, regions, client, doorbell);
    run_x86_monitor_step_loop(&mut bus, cpu, max_steps, timer_poll_interval, None)
}

pub fn run_x86_monitor_step_window_with_shadow<D: Doorbell>(
    client: &mut MailboxClient<'_>,
    doorbell: &mut D,
    ram: &mut MonitorRam,
    regions: RegionTableV1,
    cpu: &mut Cpu,
    shadow: &mut DefaultMonitorShadowRuntime,
    max_steps: u64,
    timer_poll_interval: u64,
) -> Result<X86MonitorStepReport, X86MonitorError> {
    let mut bus = MonitorSoftBus::new_with_shadow(ram, regions, client, doorbell, shadow);
    run_x86_monitor_step_loop(&mut bus, cpu, max_steps, timer_poll_interval, None)
}

pub fn run_x86_monitor_step_window_with_shadow_and_debug<D: Doorbell>(
    client: &mut MailboxClient<'_>,
    doorbell: &mut D,
    ram: &mut MonitorRam,
    regions: RegionTableV1,
    cpu: &mut Cpu,
    shadow: &mut DefaultMonitorShadowRuntime,
    max_steps: u64,
    timer_poll_interval: u64,
    debug: &mut MonitorDebugPublisher,
) -> Result<X86MonitorStepReport, X86MonitorError> {
    let mut bus = MonitorSoftBus::new_with_shadow(ram, regions, client, doorbell, shadow);
    run_x86_monitor_step_loop(&mut bus, cpu, max_steps, timer_poll_interval, Some(debug))
}

fn run_x86_monitor_step_loop<D: Doorbell>(
    bus: &mut MonitorSoftBus<'_, '_, D>,
    cpu: &mut Cpu,
    max_steps: u64,
    timer_poll_interval: u64,
    mut debug: Option<&mut MonitorDebugPublisher>,
) -> Result<X86MonitorStepReport, X86MonitorError> {
    let mut steps = 0_u64;
    let mut last_command_seq = 0_u32;
    while steps < max_steps {
        if steps == 0 || steps % HOST_COMMAND_POLL_INTERVAL == 0 {
            // The host samples progress once per second. Publish the full
            // diagnostic record every million instructions, while retaining
            // the original command/stop polling boundaries below.
            if steps % (HOST_COMMAND_POLL_INTERVAL * 256) == 0 {
                publish_x86_debug(
                    &mut debug,
                    if cpu.halted {
                        DebugPhaseV1::X86Halted
                    } else {
                        DebugPhaseV1::X86Step
                    },
                    cpu,
                    bus,
                    steps,
                );
            }
            if poll_x86_monitor_command(bus, cpu, steps, &mut last_command_seq)?.is_some() {
                publish_x86_debug(&mut debug, DebugPhaseV1::Stopping, cpu, bus, steps);
                return Ok(X86MonitorStepReport {
                    steps,
                    halted: cpu.halted,
                    eip: cpu.eip,
                    tsc: cpu.tsc(),
                    bus: bus.stats(),
                });
            }
        }
        // Preserve the exact polling boundaries while keeping ordinary x86
        // instructions, including their lazy flags, inside the shared engine.
        let mut budget = (max_steps - steps)
            .min(HOST_COMMAND_POLL_INTERVAL - steps % HOST_COMMAND_POLL_INTERVAL);
        if timer_poll_interval != 0 {
            budget = budget.min(timer_poll_interval - steps % timer_poll_interval);
        }
        let batch = Interpreter::run_batch(cpu, &mut *bus, steps, budget);
        let start_step = steps;
        steps += batch.ran;
        match batch.stop {
            BatchStop::Budget | BatchStop::Attention { .. } => {
                maybe_deliver_external_irq(cpu, &mut *bus, steps, timer_poll_interval, false)?;
            }
            BatchStop::Step {
                reason: BatchStepStop::Halted,
                ..
            } => {
                if !maybe_deliver_external_irq(cpu, &mut *bus, steps, timer_poll_interval, true)? {
                    publish_x86_debug(&mut debug, DebugPhaseV1::X86Halted, cpu, bus, steps);
                    return Ok(X86MonitorStepReport {
                        steps,
                        halted: true,
                        eip: cpu.eip,
                        tsc: cpu.tsc(),
                        bus: bus.stats(),
                    });
                }
            }
            BatchStop::Step {
                instruction_step,
                instruction_eip,
                reason: BatchStepStop::Error(error),
                ..
            } => {
                debug_assert!(instruction_step >= start_step);
                deliver_step_error(cpu, &mut *bus, instruction_step, instruction_eip, error)?;
                maybe_deliver_external_irq(cpu, &mut *bus, steps, timer_poll_interval, false)?;
            }
            BatchStop::Step {
                reason: BatchStepStop::Attention,
                ..
            } => unreachable!(),
        }
    }
    publish_x86_debug(&mut debug, DebugPhaseV1::Stopping, cpu, bus, steps);
    Ok(X86MonitorStepReport {
        steps,
        halted: cpu.halted,
        eip: cpu.eip,
        tsc: cpu.tsc(),
        bus: bus.stats(),
    })
}

fn publish_x86_debug<D: Doorbell>(
    debug: &mut Option<&mut MonitorDebugPublisher>,
    phase: DebugPhaseV1,
    cpu: &Cpu,
    bus: &MonitorSoftBus<'_, '_, D>,
    steps: u64,
) {
    if let Some(publisher) = debug.as_deref_mut() {
        publisher.publish(MonitorDebugSnapshot {
            phase,
            steps,
            cpu: Some(MonitorDebugCpuSnapshot::from_cpu(cpu)),
            bus: bus.stats(),
            shadow: bus.shadow_stats(),
            transport: bus.transport_stats(),
        });
    }
}

fn poll_x86_monitor_command<D: Doorbell>(
    bus: &mut MonitorSoftBus<'_, '_, D>,
    _cpu: &Cpu,
    _steps: u64,
    last_command_seq: &mut u32,
) -> Result<Option<HostCommandKindV1>, X86MonitorError> {
    let Some(command) = bus
        .poll_host_command(*last_command_seq)
        .map_err(X86MonitorError::Monitor)?
    else {
        return Ok(None);
    };
    *last_command_seq = command.seq;
    if command.command.kind == HostCommandKindV1::Stop as u32 {
        Ok(Some(HostCommandKindV1::Stop))
    } else {
        Ok(None)
    }
}

fn deliver_step_error<D: Doorbell>(
    cpu: &mut Cpu,
    bus: &mut MonitorSoftBus<'_, '_, D>,
    steps: u64,
    step_eip: u32,
    error: StepError,
) -> Result<(), X86MonitorError> {
    if let Some(err) = bus.take_monitor_error() {
        return Err(X86MonitorError::Monitor(err));
    }

    let result = match error {
        StepError::SoftwareInterrupt {
            vector,
            return_eip,
            fault_eip,
        } => deliver_software_interrupt_event(cpu, bus, vector, return_eip, fault_eip),
        other => {
            if let Some((vector, code, eip)) = other.exception(step_eip) {
                deliver_exception(cpu, bus, vector, code, eip)
            } else {
                return Err(X86MonitorError::Step {
                    steps: steps as u32,
                    eip: cpu.eip,
                    error: other,
                });
            }
        }
    };

    match result {
        Ok(()) => {
            if let Some(err) = bus.take_monitor_error() {
                Err(X86MonitorError::Monitor(err))
            } else {
                bus.stats.exceptions_delivered = bus.stats.exceptions_delivered.wrapping_add(1);
                Ok(())
            }
        }
        Err(error) => Err(X86MonitorError::Step {
            steps: steps as u32,
            eip: cpu.eip,
            error,
        }),
    }
}

fn maybe_deliver_external_irq<D: Doorbell>(
    cpu: &mut Cpu,
    bus: &mut MonitorSoftBus<'_, '_, D>,
    steps: u64,
    timer_poll_interval: u64,
    halted: bool,
) -> Result<bool, X86MonitorError> {
    if !halted && (timer_poll_interval == 0 || steps % timer_poll_interval != 0) {
        return Ok(false);
    }

    let mut poll = bus
        .client
        .timer_poll(bus.doorbell, cpu.tsc(), halted)
        .map_err(X86MonitorError::Monitor)?;
    bus.stats.irq_polls = bus.stats.irq_polls.wrapping_add(1);
    if halted
        && poll.pending_mask & 0xff == 0
        && cpu.maskable_interrupts_enabled()
        && poll.next_timer_vtime > cpu.tsc()
    {
        cpu.set_tsc(poll.next_timer_vtime);
        poll.pending_mask |= 1;
        bus.stats.last_vtime = cpu.tsc();
        bus.stats.hlt_wakes = bus.stats.hlt_wakes.wrapping_add(1);
    }
    if poll.pending_mask & 0xff == 0 || !cpu.maskable_interrupts_enabled() {
        return Ok(false);
    }
    let vector = bus
        .client
        .inta(bus.doorbell, cpu.tsc())
        .map_err(X86MonitorError::Monitor)?;
    cpu.halted = false;
    deliver_external_interrupt(cpu, bus, vector, cpu.eip).map_err(|error| {
        X86MonitorError::Step {
            steps: steps as u32,
            eip: cpu.eip,
            error,
        }
    })?;
    bus.stats.irqs_delivered = bus.stats.irqs_delivered.wrapping_add(1);
    Ok(true)
}

fn segment_cache_from_state(seg: SegmentStateV1) -> SegmentCache {
    SegmentCache::new(seg.selector, seg.base, seg.limit, seg.attr)
}

fn segment_state_from_cache(seg: SegmentCache) -> SegmentStateV1 {
    SegmentStateV1 {
        selector: seg.selector,
        attr: seg.architectural_flags(),
        limit: seg.limit,
        base: seg.base,
    }
}

fn table_reg_from_state(table: crate::monitor::abi::DescriptorTableV1) -> TableReg {
    TableReg {
        base: table.base as u32,
        limit: table.limit,
    }
}

fn descriptor_table_from_reg(table: TableReg) -> crate::monitor::abi::DescriptorTableV1 {
    crate::monitor::abi::DescriptorTableV1 {
        base: u64::from(table.base),
        limit: table.limit,
        reserved: [0; 3],
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ByteRing<const N: usize> {
    bytes: [u8; N],
    head: usize,
    len: usize,
}

impl<const N: usize> ByteRing<N> {
    pub const fn new() -> Self {
        Self {
            bytes: [0; N],
            head: 0,
            len: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn capacity(&self) -> usize {
        N
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, byte: u8) -> Result<(), MonitorError> {
        if self.len == N {
            return Err(MonitorError::RingFull);
        }
        let tail = self.tail();
        self.bytes[tail] = byte;
        self.len += 1;
        Ok(())
    }

    pub fn extend(&mut self, bytes: &[u8]) -> Result<(), MonitorError> {
        let mut index = 0;
        while index < bytes.len() {
            self.push(bytes[index])?;
            index += 1;
        }
        Ok(())
    }

    pub fn pop(&mut self) -> Result<u8, MonitorError> {
        if self.len == 0 {
            return Err(MonitorError::RingEmpty);
        }
        let byte = self.bytes[self.head];
        self.head = wrap_index::<N>(self.head + 1);
        self.len -= 1;
        Ok(byte)
    }

    pub fn drain_packet(&mut self) -> Result<SerialFlushPacket, MonitorError> {
        if self.len == 0 {
            return Err(MonitorError::RingEmpty);
        }
        let mut packet = SerialFlushPacket::EMPTY;
        while packet.len() < SERIAL_FLUSH_PACKET_CAPACITY && self.len != 0 {
            let next = self.pop()?;
            packet.bytes[packet.len()] = next;
            packet.len += 1;
        }
        Ok(packet)
    }

    fn tail(&self) -> usize {
        wrap_index::<N>(self.head + self.len)
    }
}

impl<const N: usize> Default for ByteRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

pub trait Doorbell {
    fn ring(&mut self, mailbox: &mut [u8]) -> Result<(), MonitorError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioDoorbell {
    addr: *mut u32,
    magic: u32,
}

impl MmioDoorbell {
    pub const fn new(addr: *mut u32) -> Self {
        Self {
            addr,
            magic: DOORBELL_MAGIC,
        }
    }

    pub const fn with_magic(addr: *mut u32, magic: u32) -> Self {
        Self { addr, magic }
    }
}

#[cfg(target_arch = "aarch64")]
impl Doorbell for MmioDoorbell {
    fn ring(&mut self, _mailbox: &mut [u8]) -> Result<(), MonitorError> {
        let addr = self.addr as usize;
        let value = self.magic;
        unsafe {
            core::arch::asm!(
                "str {value:w}, [{addr}]",
                addr = in(reg) addr,
                value = in(reg) value,
                options(nostack, preserves_flags)
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct MailboxClient<'a> {
    mailbox: &'a mut [u8],
    next_seq: u32,
    stats: MonitorTransportStats,
}

impl<'a> MailboxClient<'a> {
    pub fn new(mailbox: &'a mut [u8]) -> Result<Self, MonitorError> {
        validate_mailbox_header(mailbox)?;
        Ok(Self {
            mailbox,
            next_seq: 1,
            stats: MonitorTransportStats::default(),
        })
    }

    pub const fn stats(&self) -> MonitorTransportStats {
        self.stats
    }

    pub const fn next_seq(&self) -> u32 {
        self.next_seq
    }

    pub fn hello<D: Doorbell>(
        &mut self,
        doorbell: &mut D,
        feature_bits: u64,
        build_id_hash: u64,
    ) -> Result<(), MonitorError> {
        let seq = self.allocate_seq();
        let request = MonitorRequestV1::hello(seq, feature_bits, build_id_hash);
        let response = self.transact(doorbell, request, DEFAULT_RESPONSE_SPIN_LIMIT)?;
        let _ = response;
        self.stats.hello = self.stats.hello.wrapping_add(1);
        Ok(())
    }

    pub fn pio_in<D: Doorbell>(
        &mut self,
        doorbell: &mut D,
        port: u16,
        width: u8,
        vtime: u64,
    ) -> Result<PioRead, MonitorError> {
        let seq = self.allocate_seq();
        let request = MonitorRequestV1::pio_in(seq, port, width, vtime);
        let response = self.transact(doorbell, request, DEFAULT_RESPONSE_SPIN_LIMIT)?;
        self.stats.pio_in = self.stats.pio_in.wrapping_add(1);
        Ok(PioRead {
            value: response.value as u32,
        })
    }

    pub fn pio_out<D: Doorbell>(
        &mut self,
        doorbell: &mut D,
        port: u16,
        width: u8,
        value: u32,
        vtime: u64,
    ) -> Result<(), MonitorError> {
        let seq = self.allocate_seq();
        let request = MonitorRequestV1::pio_out(seq, port, width, value, vtime);
        let response = self.transact(doorbell, request, DEFAULT_RESPONSE_SPIN_LIMIT)?;
        let _ = response;
        self.stats.pio_out = self.stats.pio_out.wrapping_add(1);
        Ok(())
    }

    pub fn mmio_read<D: Doorbell>(
        &mut self,
        doorbell: &mut D,
        gpa: u64,
        width: u8,
        vtime: u64,
    ) -> Result<u64, MonitorError> {
        let seq = self.allocate_seq();
        let mut request =
            MonitorRequestV1::mmio(seq, MonitorRequestKindV1::MmioRead, gpa, width, 0);
        request.d = vtime;
        let response = self.transact(doorbell, request, DEFAULT_RESPONSE_SPIN_LIMIT)?;
        Ok(response.value)
    }

    pub fn mmio_write<D: Doorbell>(
        &mut self,
        doorbell: &mut D,
        gpa: u64,
        width: u8,
        value: u64,
        vtime: u64,
    ) -> Result<(), MonitorError> {
        let seq = self.allocate_seq();
        let mut request =
            MonitorRequestV1::mmio(seq, MonitorRequestKindV1::MmioWrite, gpa, width, value);
        request.d = vtime;
        let response = self.transact(doorbell, request, DEFAULT_RESPONSE_SPIN_LIMIT)?;
        let _ = response;
        Ok(())
    }

    pub fn timer_poll<D: Doorbell>(
        &mut self,
        doorbell: &mut D,
        vtime: u64,
        halted: bool,
    ) -> Result<TimerPollResult, MonitorError> {
        let seq = self.allocate_seq();
        let request = MonitorRequestV1::timer_poll(seq, vtime, halted);
        let response = self.transact(doorbell, request, DEFAULT_RESPONSE_SPIN_LIMIT)?;
        self.stats.timer_poll = self.stats.timer_poll.wrapping_add(1);
        Ok(TimerPollResult {
            pending_mask: response.value,
            next_timer_vtime: response.a,
        })
    }

    pub fn inta<D: Doorbell>(&mut self, doorbell: &mut D, vtime: u64) -> Result<u8, MonitorError> {
        let seq = self.allocate_seq();
        let request = MonitorRequestV1 {
            kind: MonitorRequestKindV1::Inta as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq,
            flags: 0,
            a: vtime,
            b: 0,
            c: 0,
            d: 0,
        };
        let response = self.transact(doorbell, request, DEFAULT_RESPONSE_SPIN_LIMIT)?;
        self.stats.inta = self.stats.inta.wrapping_add(1);
        Ok(response.value as u8)
    }

    pub fn reload_boot<D: Doorbell>(&mut self, doorbell: &mut D) -> Result<(), MonitorError> {
        let request = MonitorRequestV1 {
            kind: MonitorRequestKindV1::ReloadBoot as u32,
            seq: self.allocate_seq(),
            ..MonitorRequestV1::NONE
        };
        self.transact(doorbell, request, DEFAULT_RESPONSE_SPIN_LIMIT)?;
        Ok(())
    }

    pub fn halt<D: Doorbell>(&mut self, doorbell: &mut D, vtime: u64) -> Result<u64, MonitorError> {
        let seq = self.allocate_seq();
        let request = MonitorRequestV1 {
            kind: MonitorRequestKindV1::Halt as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq,
            flags: 0,
            a: vtime,
            b: 0,
            c: 0,
            d: 0,
        };
        let response = self.transact(doorbell, request, DEFAULT_RESPONSE_SPIN_LIMIT)?;
        self.stats.halt = self.stats.halt.wrapping_add(1);
        Ok(response.value)
    }

    pub fn serial_flush<D: Doorbell>(
        &mut self,
        doorbell: &mut D,
        packet: SerialFlushPacket,
    ) -> Result<u64, MonitorError> {
        if packet.is_empty() {
            return Ok(0);
        }
        let seq = self.allocate_seq();
        let request = packet.request(seq);
        let response = self.transact(doorbell, request, DEFAULT_RESPONSE_SPIN_LIMIT)?;
        self.stats.serial_flush = self.stats.serial_flush.wrapping_add(1);
        Ok(response.value)
    }

    pub fn trace_full<D: Doorbell>(&mut self, doorbell: &mut D) -> Result<(), MonitorError> {
        let seq = self.allocate_seq();
        let request = MonitorRequestV1 {
            kind: MonitorRequestKindV1::TraceFull as u32,
            size: core::mem::size_of::<MonitorRequestV1>() as u32,
            seq,
            flags: 0,
            a: 0,
            b: 0,
            c: 0,
            d: 0,
        };
        let response = self.transact(doorbell, request, DEFAULT_RESPONSE_SPIN_LIMIT)?;
        let _ = response;
        self.stats.trace_full = self.stats.trace_full.wrapping_add(1);
        Ok(())
    }

    pub fn stop_ack<D: Doorbell>(
        &mut self,
        doorbell: &mut D,
        packet: MonitorStopAckPacket,
    ) -> Result<(), MonitorError> {
        let seq = self.allocate_seq();
        let request = packet.request(seq);
        let response = self.transact(doorbell, request, DEFAULT_RESPONSE_SPIN_LIMIT)?;
        let _ = response;
        Ok(())
    }

    pub fn hardmmu_stats<D: Doorbell>(
        &mut self,
        doorbell: &mut D,
        packet: MonitorHardMmuStatsPacket,
    ) -> Result<(), MonitorError> {
        let seq = self.allocate_seq();
        let request = packet.request(seq);
        let response = self.transact(doorbell, request, DEFAULT_RESPONSE_SPIN_LIMIT)?;
        let _ = response;
        self.stats.hardmmu_stats = self.stats.hardmmu_stats.wrapping_add(1);
        Ok(())
    }

    pub fn poll_command(&mut self, last_seen: u32) -> Result<Option<PendingCommand>, MonitorError> {
        // Command polling starts after stage-1 setup. Both the monitor's
        // shared mapping and the host backing are coherent Normal memory;
        // observing a new sequence needs ordering, not cache invalidation.
        fence(Ordering::Acquire);
        let command_seq = read_u32(self.mailbox, MAILBOX_COMMAND_SEQ_OFFSET)?;
        if command_seq == 0 || command_seq == last_seen {
            return Ok(None);
        }
        fence(Ordering::Acquire);
        let command = read_command(self.mailbox)?;
        if command.seq != command_seq {
            return Err(MonitorError::CommandSequenceMismatch {
                expected: command_seq,
                got: command.seq,
            });
        }
        if command.size != core::mem::size_of::<HostCommandV1>() as u32 {
            return Err(MonitorError::CommandSizeMismatch { size: command.size });
        }
        Ok(Some(PendingCommand {
            seq: command_seq,
            command,
        }))
    }

    pub fn transact<D: Doorbell>(
        &mut self,
        doorbell: &mut D,
        request: MonitorRequestV1,
        spin_limit: u32,
    ) -> Result<HostResponseV1, MonitorError> {
        write_request(self.mailbox, &request)?;
        compiler_fence(Ordering::Release);
        write_u32_volatile(self.mailbox, MAILBOX_REQ_SEQ_OFFSET, request.seq)?;
        fence(Ordering::Release);
        clean_request_to_host(self.mailbox);
        doorbell.ring(self.mailbox)?;
        self.stats.doorbells = self.stats.doorbells.wrapping_add(1);
        let response = match wait_response(self.mailbox, request.seq, spin_limit) {
            Ok(response) => response,
            Err(MonitorError::ResponseTimeout { seq }) => {
                self.stats.response_timeouts = self.stats.response_timeouts.wrapping_add(1);
                return Err(MonitorError::ResponseTimeout { seq });
            }
            Err(err) => return Err(err),
        };
        self.stats.requests = self.stats.requests.wrapping_add(1);
        self.stats.last_seq = request.seq;
        self.stats.last_kind = request.kind;
        self.stats.last_status = response.status;
        classify_response_for_request(response, request)?;
        Ok(response)
    }

    fn allocate_seq(&mut self) -> u32 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        seq
    }
}

pub fn validate_mailbox_header(mailbox: &[u8]) -> Result<(), MonitorError> {
    ensure_mailbox_len(mailbox)?;
    let version = read_u16(mailbox, MAILBOX_ABI_VERSION_OFFSET)?;
    if version != ABI_VERSION {
        return Err(MonitorError::BadMailboxVersion { version });
    }
    let size = read_u16(mailbox, MAILBOX_HEADER_SIZE_OFFSET)?;
    if size as usize != core::mem::size_of::<crate::monitor::abi::MailboxV1>() {
        return Err(MonitorError::BadMailboxSize { size });
    }
    Ok(())
}

pub fn init_mailbox_header(mailbox: &mut [u8]) -> Result<(), MonitorError> {
    ensure_mailbox_len(mailbox)?;
    write_u16(mailbox, MAILBOX_ABI_VERSION_OFFSET, ABI_VERSION)?;
    write_u16(
        mailbox,
        MAILBOX_HEADER_SIZE_OFFSET,
        core::mem::size_of::<crate::monitor::abi::MailboxV1>() as u16,
    )?;
    write_u32(mailbox, MAILBOX_REQ_SEQ_OFFSET, 0)?;
    write_u32(mailbox, MAILBOX_RESP_SEQ_OFFSET, 0)?;
    write_u32(mailbox, MAILBOX_COMMAND_SEQ_OFFSET, 0)?;
    write_request(mailbox, &MonitorRequestV1::NONE)?;
    write_response(mailbox, &HostResponseV1::ok(0, 0))?;
    write_command(mailbox, &HostCommandV1::NONE)
}

pub fn wait_response(
    mailbox: &[u8],
    seq: u32,
    spin_limit: u32,
) -> Result<HostResponseV1, MonitorError> {
    let mut spins = 0;
    loop {
        fence(Ordering::Acquire);
        invalidate_response_from_host(mailbox);
        let resp_seq = read_u32(mailbox, MAILBOX_RESP_SEQ_OFFSET)?;
        if resp_seq == seq {
            fence(Ordering::Acquire);
            invalidate_response_from_host(mailbox);
            let response = read_response(mailbox)?;
            if response.seq != seq {
                return Err(MonitorError::ResponseSequenceMismatch {
                    expected: seq,
                    got: response.seq,
                });
            }
            if response.size != core::mem::size_of::<HostResponseV1>() as u32 {
                return Err(MonitorError::ResponseSizeMismatch {
                    size: response.size,
                });
            }
            return Ok(response);
        }
        if spins >= spin_limit {
            return Err(MonitorError::ResponseTimeout { seq });
        }
        spins += 1;
        core::hint::spin_loop();
    }
}

pub fn classify_response_for_request(
    response: HostResponseV1,
    request: MonitorRequestV1,
) -> Result<(), MonitorError> {
    match response.status {
        value if value == ResponseStatusV1::Ok as u32 => Ok(()),
        value if value == ResponseStatusV1::Unsupported as u32 => Err(MonitorError::Unsupported {
            seq: response.seq,
            kind: request.kind,
            a: request.a,
            b: request.b,
            c: request.c,
        }),
        value if value == ResponseStatusV1::Fault as u32 => Err(MonitorError::Fault {
            seq: response.seq,
            value: response.value,
        }),
        value if value == ResponseStatusV1::BadPacket as u32 => {
            Err(MonitorError::BadPacket { seq: response.seq })
        }
        _ => Err(MonitorError::BadPacket { seq: response.seq }),
    }
}

pub fn classify_response(response: HostResponseV1) -> Result<(), MonitorError> {
    classify_response_for_request(response, MonitorRequestV1::NONE)
}

#[cfg(test)]
fn write_host_response_for_test(
    mailbox: &mut [u8],
    response: &HostResponseV1,
) -> Result<(), MonitorError> {
    write_response(mailbox, response)?;
    compiler_fence(Ordering::Release);
    write_u32_volatile(mailbox, MAILBOX_RESP_SEQ_OFFSET, response.seq)
}

#[cfg(test)]
fn write_host_command_for_test(
    mailbox: &mut [u8],
    command_seq: u32,
    command: &HostCommandV1,
) -> Result<(), MonitorError> {
    write_command(mailbox, command)?;
    compiler_fence(Ordering::Release);
    write_u32_volatile(mailbox, MAILBOX_COMMAND_SEQ_OFFSET, command_seq)
}

#[cfg(test)]
fn read_monitor_request_for_test(mailbox: &[u8]) -> Result<MonitorRequestV1, MonitorError> {
    read_request(mailbox)
}

fn write_request(mailbox: &mut [u8], request: &MonitorRequestV1) -> Result<(), MonitorError> {
    let request_bytes = mailbox_window_mut(mailbox, MAILBOX_REQUEST_OFFSET)?;
    write_u32(request_bytes, REQUEST_KIND_OFFSET, request.kind)?;
    write_u32(request_bytes, REQUEST_SIZE_OFFSET, request.size)?;
    write_u32(request_bytes, REQUEST_SEQ_OFFSET, request.seq)?;
    write_u32(request_bytes, REQUEST_FLAGS_OFFSET, request.flags)?;
    write_u64(request_bytes, REQUEST_A_OFFSET, request.a)?;
    write_u64(request_bytes, REQUEST_B_OFFSET, request.b)?;
    write_u64(request_bytes, REQUEST_C_OFFSET, request.c)?;
    write_u64(request_bytes, REQUEST_D_OFFSET, request.d)
}

#[cfg(test)]
fn read_request(mailbox: &[u8]) -> Result<MonitorRequestV1, MonitorError> {
    let request = mailbox_window(mailbox, MAILBOX_REQUEST_OFFSET)?;
    Ok(MonitorRequestV1 {
        kind: read_u32(request, REQUEST_KIND_OFFSET)?,
        size: read_u32(request, REQUEST_SIZE_OFFSET)?,
        seq: read_u32(request, REQUEST_SEQ_OFFSET)?,
        flags: read_u32(request, REQUEST_FLAGS_OFFSET)?,
        a: read_u64(request, REQUEST_A_OFFSET)?,
        b: read_u64(request, REQUEST_B_OFFSET)?,
        c: read_u64(request, REQUEST_C_OFFSET)?,
        d: read_u64(request, REQUEST_D_OFFSET)?,
    })
}

fn write_response(mailbox: &mut [u8], response: &HostResponseV1) -> Result<(), MonitorError> {
    let response_bytes = mailbox_window_mut(mailbox, MAILBOX_RESPONSE_OFFSET)?;
    write_u32(response_bytes, RESPONSE_STATUS_OFFSET, response.status)?;
    write_u32(response_bytes, RESPONSE_SIZE_OFFSET, response.size)?;
    write_u32(response_bytes, RESPONSE_SEQ_OFFSET, response.seq)?;
    write_u32(response_bytes, RESPONSE_RESERVED_OFFSET, response.reserved)?;
    write_u64(response_bytes, RESPONSE_VALUE_OFFSET, response.value)?;
    write_u64(response_bytes, RESPONSE_A_OFFSET, response.a)?;
    write_u64(response_bytes, RESPONSE_B_OFFSET, response.b)?;
    write_u64(response_bytes, RESPONSE_C_OFFSET, response.c)
}

fn read_response(mailbox: &[u8]) -> Result<HostResponseV1, MonitorError> {
    let response = mailbox_window(mailbox, MAILBOX_RESPONSE_OFFSET)?;
    Ok(HostResponseV1 {
        status: read_u32(response, RESPONSE_STATUS_OFFSET)?,
        size: read_u32(response, RESPONSE_SIZE_OFFSET)?,
        seq: read_u32(response, RESPONSE_SEQ_OFFSET)?,
        reserved: read_u32(response, RESPONSE_RESERVED_OFFSET)?,
        value: read_u64(response, RESPONSE_VALUE_OFFSET)?,
        a: read_u64(response, RESPONSE_A_OFFSET)?,
        b: read_u64(response, RESPONSE_B_OFFSET)?,
        c: read_u64(response, RESPONSE_C_OFFSET)?,
    })
}

fn write_command(mailbox: &mut [u8], command: &HostCommandV1) -> Result<(), MonitorError> {
    let command_bytes = mailbox_window_mut(mailbox, MAILBOX_COMMAND_OFFSET)?;
    write_u32(command_bytes, HOST_COMMAND_KIND_OFFSET, command.kind)?;
    write_u32(command_bytes, HOST_COMMAND_SIZE_OFFSET, command.size)?;
    write_u32(command_bytes, HOST_COMMAND_SEQ_OFFSET, command.seq)?;
    write_u32(command_bytes, HOST_COMMAND_FLAGS_OFFSET, command.flags)?;
    write_u64(command_bytes, HOST_COMMAND_A_OFFSET, command.a)?;
    write_u64(command_bytes, HOST_COMMAND_B_OFFSET, command.b)?;
    write_u64(command_bytes, HOST_COMMAND_C_OFFSET, command.c)?;
    write_u64(command_bytes, HOST_COMMAND_D_OFFSET, command.d)
}

fn read_command(mailbox: &[u8]) -> Result<HostCommandV1, MonitorError> {
    let command = mailbox_window(mailbox, MAILBOX_COMMAND_OFFSET)?;
    Ok(HostCommandV1 {
        kind: read_u32(command, HOST_COMMAND_KIND_OFFSET)?,
        size: read_u32(command, HOST_COMMAND_SIZE_OFFSET)?,
        seq: read_u32(command, HOST_COMMAND_SEQ_OFFSET)?,
        flags: read_u32(command, HOST_COMMAND_FLAGS_OFFSET)?,
        a: read_u64(command, HOST_COMMAND_A_OFFSET)?,
        b: read_u64(command, HOST_COMMAND_B_OFFSET)?,
        c: read_u64(command, HOST_COMMAND_C_OFFSET)?,
        d: read_u64(command, HOST_COMMAND_D_OFFSET)?,
    })
}

fn mailbox_window(mailbox: &[u8], offset: usize) -> Result<&[u8], MonitorError> {
    ensure_mailbox_len(mailbox)?;
    mailbox
        .get(offset..)
        .ok_or(MonitorError::MailboxTooSmall { len: mailbox.len() })
}

fn mailbox_window_mut(mailbox: &mut [u8], offset: usize) -> Result<&mut [u8], MonitorError> {
    ensure_mailbox_len(mailbox)?;
    let len = mailbox.len();
    mailbox
        .get_mut(offset..)
        .ok_or(MonitorError::MailboxTooSmall { len })
}

fn ensure_mailbox_len(mailbox: &[u8]) -> Result<(), MonitorError> {
    if mailbox.len() < core::mem::size_of::<crate::monitor::abi::MailboxV1>() {
        return Err(MonitorError::MailboxTooSmall { len: mailbox.len() });
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, MonitorError> {
    let data = bytes
        .get(offset..offset + 2)
        .ok_or(MonitorError::MailboxTooSmall { len: bytes.len() })?;
    let mut value = [0_u8; 2];
    read_volatile_bytes(data, &mut value);
    Ok(u16::from_le_bytes(value))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, MonitorError> {
    let data = bytes
        .get(offset..offset + 4)
        .ok_or(MonitorError::MailboxTooSmall { len: bytes.len() })?;
    let mut value = [0_u8; 4];
    read_volatile_bytes(data, &mut value);
    Ok(u32::from_le_bytes(value))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, MonitorError> {
    let data = bytes
        .get(offset..offset + 8)
        .ok_or(MonitorError::MailboxTooSmall { len: bytes.len() })?;
    let mut value = [0_u8; 8];
    read_volatile_bytes(data, &mut value);
    Ok(u64::from_le_bytes(value))
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) -> Result<(), MonitorError> {
    let len = bytes.len();
    let data = bytes
        .get_mut(offset..offset + 2)
        .ok_or(MonitorError::MailboxTooSmall { len })?;
    write_volatile_bytes(data, &value.to_le_bytes());
    Ok(())
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), MonitorError> {
    let len = bytes.len();
    let data = bytes
        .get_mut(offset..offset + 4)
        .ok_or(MonitorError::MailboxTooSmall { len })?;
    write_volatile_bytes(data, &value.to_le_bytes());
    Ok(())
}

fn write_u32_volatile(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), MonitorError> {
    write_u32(bytes, offset, value)
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) -> Result<(), MonitorError> {
    let len = bytes.len();
    let data = bytes
        .get_mut(offset..offset + 8)
        .ok_or(MonitorError::MailboxTooSmall { len })?;
    write_volatile_bytes(data, &value.to_le_bytes());
    Ok(())
}

fn read_volatile_bytes(src: &[u8], dst: &mut [u8]) {
    let mut index = 0;
    while index < dst.len() {
        unsafe {
            dst[index] = src.as_ptr().add(index).read_volatile();
        }
        index += 1;
    }
}

fn write_volatile_bytes(dst: &mut [u8], src: &[u8]) {
    let mut index = 0;
    while index < src.len() {
        unsafe {
            dst.as_mut_ptr().add(index).write_volatile(src[index]);
        }
        index += 1;
    }
}

fn checked_ipa_add(base: u64, offset: u64) -> Result<u64, MonitorError> {
    base.checked_add(offset)
        .ok_or(MonitorError::Abi(AbiError::RegionOverflow))
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn clean_request_to_host(mailbox: &[u8]) {
    clean_to_poc(
        mailbox.as_ptr(),
        MAILBOX_REQUEST_OFFSET + core::mem::size_of::<MonitorRequestV1>(),
    );
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn clean_request_to_host(_: &[u8]) {}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn invalidate_response_from_host(mailbox: &[u8]) {
    invalidate_to_poc(
        mailbox.as_ptr().wrapping_add(MAILBOX_RESP_SEQ_OFFSET),
        MAILBOX_RESPONSE_OFFSET + core::mem::size_of::<HostResponseV1>() - MAILBOX_RESP_SEQ_OFFSET,
    );
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn invalidate_response_from_host(_: &[u8]) {}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn clean_to_poc(ptr: *const u8, len: usize) {
    cache_range(ptr, len, CacheOp::Clean);
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn clean_to_poc(_: *const u8, _: usize) {}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn invalidate_to_poc(ptr: *const u8, len: usize) {
    cache_range(ptr, len, CacheOp::Invalidate);
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[derive(Clone, Copy)]
enum CacheOp {
    Clean,
    Invalidate,
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn cache_range(ptr: *const u8, len: usize, op: CacheOp) {
    // The early smoke monitor runs before stage-1 cacheable mappings exist.
    if len == 0 {
        return;
    }
    let line = dcache_line_bytes();
    let start = (ptr as usize) & !(line - 1);
    let end = (ptr as usize).saturating_add(len).saturating_add(line - 1) & !(line - 1);
    let mut addr = start;
    while addr < end {
        unsafe {
            match op {
                CacheOp::Clean => {
                    core::arch::asm!("dc cvac, {addr}", addr = in(reg) addr, options(nostack, preserves_flags));
                }
                CacheOp::Invalidate => {
                    core::arch::asm!("dc ivac, {addr}", addr = in(reg) addr, options(nostack, preserves_flags));
                }
            }
        }
        addr = addr.saturating_add(line);
    }
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn dcache_line_bytes() -> usize {
    let ctr: usize;
    unsafe {
        core::arch::asm!("mrs {ctr}, ctr_el0", ctr = out(reg) ctr, options(nostack, preserves_flags));
    }
    let dminline = (ctr >> 16) & 0xf;
    4_usize << dminline
}

const fn wrap_index<const N: usize>(value: usize) -> usize {
    if N == 0 { 0 } else { value % N }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::abi::{
        BOOT_BLOCK_MAILBOX_OFFSET_FIELD_OFFSET, CpuStateV1, HostCommandKindV1, IpaLayoutV1,
        MailboxV1, PAGE_SIZE_4K, RegionEntryV1, RegionTableV1,
    };
    use crate::x86::cpu::{CR0_WP, CR4_PSE};
    use crate::x86::{PageTableFrameRole, PhysFrame, PhysWrite, WalkFrameSet, WriteOrigin};
    use std::vec;

    struct EchoDoorbell {
        rings: u64,
        expected_kind: Option<u32>,
        response_value: u64,
    }

    impl EchoDoorbell {
        fn expecting(kind: MonitorRequestKindV1, value: u64) -> Self {
            Self {
                rings: 0,
                expected_kind: Some(kind as u32),
                response_value: value,
            }
        }
    }

    impl Doorbell for EchoDoorbell {
        fn ring(&mut self, mailbox: &mut [u8]) -> Result<(), MonitorError> {
            self.rings += 1;
            let request = read_monitor_request_for_test(mailbox)?;
            if let Some(kind) = self.expected_kind {
                assert_eq!(request.kind, kind);
            }
            write_host_response_for_test(
                mailbox,
                &HostResponseV1::ok(request.seq, self.response_value),
            )
        }
    }

    fn stage1_entry(table: &[u64; STAGE1_TABLE_ENTRIES], addr: u64) -> u64 {
        table[l2_index(addr).unwrap()]
    }

    fn stage1_page_entry(table: &[u64; STAGE1_TABLE_ENTRIES], addr: u64) -> u64 {
        table[l3_index(addr).unwrap()]
    }

    #[test]
    fn x86_monitor_error_report_encodes_step_and_memory_failures() {
        assert_eq!(
            x86_monitor_error_report(X86MonitorError::Step {
                steps: 42,
                eip: 0xc001_0000,
                error: StepError::UnsupportedOpcode {
                    eip: 0xc001_0000,
                    opcode: 0x0f,
                },
            }),
            X86MonitorErrorReport {
                kind: 0x3020,
                eip: 0xc001_0000,
                arg0: 0xc001_0000,
                arg1: 0x0f,
                arg2: 0,
            }
        );

        assert_eq!(
            x86_monitor_error_report(X86MonitorError::Mem(MemError::IoUnhandled { port: 0x3f8 })),
            X86MonitorErrorReport {
                kind: 0x2003,
                eip: 0,
                arg0: 0x3f8,
                arg1: 0,
                arg2: 0,
            }
        );

        assert_eq!(
            x86_monitor_error_report(X86MonitorError::Monitor(MonitorError::Unsupported {
                seq: 77,
                kind: MonitorRequestKindV1::PioOut as u32,
                a: 0x3f8,
                b: 1,
                c: 0x55,
            })),
            X86MonitorErrorReport {
                kind: 0x1006,
                eip: 77,
                arg0: MonitorRequestKindV1::PioOut as u32,
                arg1: 0x3f8,
                arg2: 1,
            }
        );
    }

    #[test]
    fn native_thunk_abort_status_values_match_aarch64_vector_encoding() {
        assert_eq!(NATIVE_THUNK_STATUS_TRANSLATION_ABORT, 1);
        assert_eq!(NATIVE_THUNK_STATUS_ACCESS_FLAG_ABORT, 2);
        assert_eq!(NATIVE_THUNK_STATUS_PERMISSION_ABORT, 3);
    }

    #[test]
    fn stage1_tables_identity_map_ram_internal_slots_and_doorbell() {
        let mut regions = RegionTableV1::new();
        regions.push(RegionEntryV1::ram(0, 0x000a_0000)).unwrap();
        regions
            .push(RegionEntryV1::mmio(0x000a_0000, 0x0002_0000))
            .unwrap();
        regions
            .push(RegionEntryV1::hole(0x000c_0000, 0x0004_0000))
            .unwrap();
        regions
            .push(RegionEntryV1::ram(0x0010_0000, 0x07f0_0000))
            .unwrap();

        let layout = IpaLayoutV1::new(0x0800_0000, 0x0100_0000, 0x0020_0000, 0x0040_0000);
        let mut tables = MonitorStage1Tables::new();
        build_monitor_stage1_tables(&mut tables, layout, regions).unwrap();

        assert_eq!(tables.byte_len(), PAGE_SIZE_4K as usize * 6);
        assert_eq!(tables.l1[0], 0);
        assert_ne!(tables.l1[l1_index(layout.ram_base).unwrap()] & 0b11, 0);
        assert_ne!(tables.l1[l1_index(layout.monitor_base).unwrap()] & 0b11, 0);
        assert_ne!(tables.l1[l1_index(layout.shared_base).unwrap()] & 0b11, 0);
        assert_ne!(tables.l1[l1_index(layout.doorbell_ipa).unwrap()] & 0b11, 0);

        assert_eq!(stage1_page_entry(&tables.low_l3, 0) & 0b11, 0b11);
        assert_eq!(
            stage1_page_entry(&tables.low_l3, 0) & 0x0000_ffff_ffff_f000,
            layout.ram_base
        );
        assert_eq!(
            stage1_entry(&tables.low_l2, 0x0020_0000) & 0x0000_ffff_ffff_f000,
            layout.ram_base + 0x0020_0000
        );
        assert_eq!(stage1_page_entry(&tables.low_l3, 0x0009_f000) & 0b11, 0b11);
        assert_eq!(stage1_page_entry(&tables.low_l3, 0x000a_0000), 0);
        assert_eq!(stage1_page_entry(&tables.low_l3, 0x0010_0000) & 0b11, 0b11);
        assert_eq!(stage1_entry(&tables.low_l2, 0x0020_0000) & 0b11, 0b01);

        let monitor_entry = stage1_entry(&tables.monitor_l2, layout.monitor_base);
        assert_eq!(monitor_entry & 0b11, 0b01);
        assert_eq!((monitor_entry >> 2) & 0x7, 0);

        let shared_entry = stage1_entry(&tables.shared_l2, layout.shared_base);
        assert_eq!(shared_entry & 0b11, 0b01);
        assert_eq!((shared_entry >> 2) & 0x7, 0);
        assert_ne!(shared_entry & ((1_u64 << 53) | (1_u64 << 54)), 0);

        let trace_entry = stage1_entry(&tables.shared_l2, layout.trace_base);
        assert_eq!(trace_entry & 0b11, 0b01);
        assert_eq!((trace_entry >> 2) & 0x7, 0);

        let doorbell_entry = stage1_entry(&tables.doorbell_l2, layout.doorbell_ipa);
        assert_eq!(doorbell_entry & 0b11, 0b01);
        assert_eq!((doorbell_entry >> 2) & 0x7, 1);
        assert_ne!(doorbell_entry & ((1_u64 << 53) | (1_u64 << 54)), 0);
    }

    #[test]
    fn shadow_stage1_tables_attach_and_install_dual_window_leaves() {
        let tables = MonitorStage1Tables::new();
        let mut shadow = MonitorShadowStage1Tables::<8>::new();

        attach_shadow_stage1_tables(&tables, &mut shadow).unwrap();

        assert_ne!(
            shadow.contexts[0].root[l1_index(SHADOW_SUP_BASE).unwrap()] & 0b11,
            0
        );
        assert_ne!(
            shadow.contexts[0].root[l1_index(SHADOW_USR_BASE).unwrap()] & 0b11,
            0
        );

        shadow
            .install_leaf(
                ShadowWindow::Supervisor,
                LinearAddr(0x0000_1234),
                PhysAddr(0x0050_1234),
            )
            .unwrap();
        let sup_entry = shadow
            .descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x0000_1234))
            .unwrap();
        assert_eq!(sup_entry & 0b11, 0b11);
        assert_eq!(
            sup_entry & 0x0000_ffff_ffff_f000,
            abi::X86_RAM_IPA_BASE + 0x0050_1000
        );
        assert_ne!(sup_entry & ((1_u64 << 53) | (1_u64 << 54)), 0);
        assert_ne!(sup_entry & (1 << 11), 0);
        assert_ne!(sup_entry & (1 << 7), 0);

        shadow
            .install_leaf(
                ShadowWindow::User,
                LinearAddr(0xc001_2000),
                PhysAddr(0x0060_2000),
            )
            .unwrap();
        let user_entry = shadow
            .descriptor_for(ShadowWindow::User, LinearAddr(0xc001_2000))
            .unwrap();
        assert_eq!(user_entry & 0b11, 0b11);
        assert_eq!(
            user_entry & 0x0000_ffff_ffff_f000,
            abi::X86_RAM_IPA_BASE + 0x0060_2000
        );
        assert_ne!(user_entry & (1 << 11), 0);
        assert_ne!(user_entry & (1 << 7), 0);
    }

    #[test]
    fn context_flush_breaks_all_parent_windows_and_preserves_other_asids() {
        let mut shadow = MonitorShadowStage1Tables::<16>::new();
        for active in 0..2 {
            shadow.active = active;
            for window in [ShadowWindow::Supervisor, ShadowWindow::User] {
                for slot in 0..4 {
                    let linear = LinearAddr((slot << 30) | 0x3fe0_1000);
                    shadow
                        .install_leaf(window, linear, PhysAddr(0x4000))
                        .unwrap();
                }
            }
        }
        shadow.active = 0;
        shadow.flush_active();
        for active in 0..2 {
            shadow.active = active;
            for window in [ShadowWindow::Supervisor, ShadowWindow::User] {
                for slot in 0..4 {
                    let linear = LinearAddr((slot << 30) | 0x3fe0_1000);
                    assert_eq!(shadow.descriptor_for(window, linear).is_some(), active == 1);
                }
            }
        }
        // Reuse every freed table for different addresses without reviving
        // either window's previous leaves or disturbing the resident ASID.
        shadow.active = 0;
        for window in [ShadowWindow::Supervisor, ShadowWindow::User] {
            for slot in 0..4 {
                let old = LinearAddr((slot << 30) | 0x3fe0_1000);
                let new = LinearAddr(slot << 30);
                shadow.install_leaf(window, new, PhysAddr(0x8000)).unwrap();
                assert!(shadow.descriptor_for(window, old).is_none());
                assert!(shadow.descriptor_for(window, new).is_some());
            }
        }
        shadow.active = 1;
        assert!(
            shadow
                .descriptor_for(ShadowWindow::User, LinearAddr(0xffe0_1000))
                .is_some()
        );
    }

    #[test]
    fn recycled_stage1_table_does_not_revive_old_window_mappings() {
        let mut shadow = MonitorShadowStage1Tables::<1>::new();
        shadow
            .install_leaf(
                ShadowWindow::Supervisor,
                LinearAddr(0x1000),
                PhysAddr(0x8000),
            )
            .unwrap();
        shadow.clear();
        assert_eq!(
            shadow.descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x1000)),
            None
        );
        shadow
            .install_leaf(ShadowWindow::User, LinearAddr(0x2000), PhysAddr(0xa000))
            .unwrap();
        assert_eq!(
            shadow.descriptor_for(ShadowWindow::User, LinearAddr(0x1000)),
            None
        );
        assert_eq!(
            shadow.descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x2000)),
            None
        );
        let entry = shadow
            .descriptor_for(ShadowWindow::User, LinearAddr(0x2000))
            .unwrap();
        assert_eq!(
            entry & 0x0000_ffff_ffff_f000,
            abi::X86_RAM_IPA_BASE + 0xa000
        );
        assert_ne!(entry & (1 << 7), 0);
    }

    fn walked_translation(
        phys: u64,
        page_size: TranslationPageSize,
        access: AccessKind,
    ) -> Translation {
        let mut frames = WalkFrameSet::new();
        assert!(frames.push(PhysFrame(1), PageTableFrameRole::Directory));
        assert!(frames.push(PhysFrame(2), PageTableFrameRole::Table));
        Translation::new(
            PhysAddr(phys),
            page_size,
            frames,
            true,
            matches!(access, AccessKind::Write),
            true,
            matches!(access, AccessKind::Write),
        )
    }

    #[test]
    fn monitor_shadow_runtime_installs_ram_fragments_from_supported_walks() {
        let mut stage1 = MonitorShadowStage1Tables::<8>::new();
        let mut runtime: MonitorShadowRuntime<16, 8> =
            MonitorShadowRuntime::new(&mut stage1 as *mut _);
        let regions = all_ram_regions(0x8000);
        let cr0 = CR0_PE | CR0_PG | CR0_WP;

        runtime.after_linear_translate(
            &regions,
            LinearAddr(0x0000_4000),
            AccessKind::Read,
            AccessPriv::Supervisor,
            cr0,
            0x1000,
            CR4_PSE,
            2,
            walked_translation(0x5000, TranslationPageSize::Size4K, AccessKind::Read),
        );

        assert_eq!(runtime.stats().eligible_4k, 1);
        assert_eq!(runtime.stats().read_fills, 1);
        assert_eq!(runtime.stats().stage1_installs, 1);
        assert!(
            stage1
                .descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x0000_4000))
                .is_some()
        );
        runtime.after_linear_translate(
            &regions,
            LinearAddr(0x0000_4100),
            AccessKind::Read,
            AccessPriv::Supervisor,
            cr0,
            0x2000,
            CR4_PAE,
            2,
            walked_translation(0x5100, TranslationPageSize::Size4K, AccessKind::Read),
        );

        assert_eq!(runtime.stats().eligible_4k, 2);
        assert_eq!(runtime.stats().read_fills, 2);
        assert_eq!(runtime.stats().stage1_installs, 2);
        assert!(
            stage1
                .descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x0000_4100))
                .is_some()
        );

        runtime.after_linear_translate(
            &regions,
            LinearAddr(0x0000_5000),
            AccessKind::Write,
            AccessPriv::User,
            cr0,
            0x1000,
            CR4_PSE,
            2,
            walked_translation(0x9000, TranslationPageSize::Size4K, AccessKind::Write),
        );
        runtime.after_linear_translate(
            &regions,
            LinearAddr(0x0000_6000),
            AccessKind::Read,
            AccessPriv::Supervisor,
            cr0,
            0x1000,
            CR4_PSE,
            2,
            walked_translation(0x6000, TranslationPageSize::Size4M, AccessKind::Read),
        );
        runtime.after_linear_translate(
            &regions,
            LinearAddr(0x0000_7000),
            AccessKind::Read,
            AccessPriv::Supervisor,
            cr0,
            0x1000,
            CR4_PSE,
            EFLAGS_VM,
            walked_translation(0x7000, TranslationPageSize::Size4K, AccessKind::Read),
        );

        assert_eq!(runtime.stats().soft_non_ram, 1);
        assert_eq!(runtime.stats().soft_large_or_direct, 0);
        assert_eq!(runtime.stats().soft_unsupported_mode, 1);
        assert!(
            stage1
                .descriptor_for(ShadowWindow::User, LinearAddr(0x0000_5000))
                .is_none()
        );
    }

    #[test]
    fn shadow_survives_fpu_control_changes_but_not_paging_permission_changes() {
        let mut stage1 = MonitorShadowStage1Tables::<8>::new();
        let mut runtime: MonitorShadowRuntime<16, 8> = MonitorShadowRuntime::new(&mut stage1);
        let cr0 = CR0_PE | CR0_PG | CR0_WP;
        let linear = LinearAddr(0x4000);
        runtime.after_linear_translate(
            &all_ram_regions(0x8000),
            linear,
            AccessKind::Read,
            AccessPriv::Supervisor,
            cr0,
            0x1000,
            0,
            2,
            walked_translation(0x5000, TranslationPageSize::Size4K, AccessKind::Read),
        );
        let entry = runtime
            .descriptor_for(ShadowWindow::Supervisor, linear)
            .unwrap();
        runtime.flush_for_control_write(0, cr0, cr0 | crate::x86::cpu::CR0_TS);
        runtime.flush_for_control_write(4, 0, crate::x86::cpu::CR4_OSFXSR);
        assert_eq!(
            runtime.descriptor_for(ShadowWindow::Supervisor, linear),
            Some(entry)
        );
        runtime.flush_for_control_write(0, cr0, cr0 & !CR0_WP);
        assert_eq!(
            runtime.descriptor_for(ShadowWindow::Supervisor, linear),
            None
        );
    }

    #[test]
    fn pae_control_reloads_cannot_reuse_translations_from_old_pdptrs() {
        let mut stage1 = MonitorShadowStage1Tables::<8>::new();
        let mut runtime: MonitorShadowRuntime<16, 8> = MonitorShadowRuntime::new(&mut stage1);
        let cr0 = CR0_PE | CR0_PG;
        let linear = LinearAddr(0x4000);
        for (index, value) in [(0, cr0), (3, 0x1000), (4, CR4_PAE)] {
            runtime.after_linear_translate(
                &all_ram_regions(0x8000),
                linear,
                AccessKind::Read,
                AccessPriv::Supervisor,
                cr0,
                0x1000,
                CR4_PAE,
                2,
                walked_translation(0x5000, TranslationPageSize::Size4K, AccessKind::Read),
            );
            assert!(
                runtime
                    .descriptor_for(ShadowWindow::Supervisor, linear)
                    .is_some()
            );
            runtime.flush_for_control_write(index, value, value);
            assert_eq!(
                runtime.descriptor_for(ShadowWindow::Supervisor, linear),
                None
            );
        }
    }

    #[test]
    fn asid_flush_and_table_recycling_leave_other_spaces_intact() {
        let mut stage1 = MonitorShadowStage1Tables::<2>::new();
        stage1
            .install_leaf(
                ShadowWindow::Supervisor,
                LinearAddr(0x1000),
                PhysAddr(0x8000),
            )
            .unwrap();
        stage1.activate(1);
        stage1
            .install_leaf(
                ShadowWindow::Supervisor,
                LinearAddr(0x1000),
                PhysAddr(0x9000),
            )
            .unwrap();
        stage1.activate(0);
        assert_eq!(
            stage1
                .descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x1000))
                .unwrap()
                & 0x0000_ffff_ffff_f000,
            abi::X86_RAM_IPA_BASE + 0x8000
        );
        stage1.flush_active();
        stage1
            .install_leaf(ShadowWindow::User, LinearAddr(0x2000), PhysAddr(0xa000))
            .unwrap();
        assert_eq!(
            stage1.descriptor_for(ShadowWindow::User, LinearAddr(0x1000)),
            None
        );
        stage1.activate(1);
        assert_eq!(
            stage1
                .descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x1000))
                .unwrap()
                & 0x0000_ffff_ffff_f000,
            abi::X86_RAM_IPA_BASE + 0x9000
        );
    }

    #[test]
    fn monitor_shadow_runtime_invalidates_written_entry_and_retains_its_neighbor() {
        let mut stage1 = MonitorShadowStage1Tables::<8>::new();
        let mut runtime: MonitorShadowRuntime<16, 8> =
            MonitorShadowRuntime::new(&mut stage1 as *mut _);
        let regions = all_ram_regions(0x8000);
        let mut cpu = Cpu::new();
        cpu.cr0 = CR0_PE | CR0_PG;
        cpu.cr3 = 0x1000;
        runtime.reset(&cpu);

        runtime.after_linear_translate(
            &regions,
            LinearAddr(0x0000_4000),
            AccessKind::Read,
            AccessPriv::Supervisor,
            CR0_PE | CR0_PG,
            0x1000,
            0,
            2,
            walked_translation(0x5000, TranslationPageSize::Size4K, AccessKind::Read),
        );
        assert!(
            stage1
                .descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x0000_4000))
                .is_some()
        );

        runtime.observe_phys_write(PhysWrite::new(WriteOrigin::Cpu, PhysAddr(0x2000), 4));

        assert_eq!(runtime.stats().page_table_write_invalidations, 1);
        assert!(runtime.write_touches_page_table_frame(PhysWrite::new(
            WriteOrigin::Cpu,
            PhysAddr(0x2000),
            4
        )));
        assert!(
            runtime
                .descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x4000))
                .is_some()
        );
        assert_eq!(runtime.stats().page_table_write_flushes, 1);
        assert_eq!(runtime.stats().stage1_flushes, 0);
        runtime.observe_phys_write(PhysWrite::new(WriteOrigin::Cpu, PhysAddr(0x2010), 4));
        assert!(
            runtime
                .descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x4000))
                .is_none()
        );
        assert_eq!(runtime.stats().page_table_write_flushes, 2);
        assert_eq!(runtime.stats().stage1_flushes, 1);
    }

    #[test]
    fn shadow_entry_writes_cover_aliases_split_words_and_inactive_windows() {
        for pae in [false, true] {
            let mut tables = MonitorShadowStage1Tables::<16>::new();
            let mut runtime = MonitorShadowRuntime::<16, 16>::new(&mut tables);
            let cr0 = CR0_PE | CR0_PG;
            let cr4 = if pae { CR4_PAE } else { 0 };
            for cr3 in [0x1000, 0x3000] {
                for priv_ in [AccessPriv::Supervisor, AccessPriv::User] {
                    for linear in [0x400000, 0x401000, 0x402000, 0x600000, 0x602000] {
                        let mut frames = WalkFrameSet::new();
                        if pae {
                            frames.push(PhysFrame(u64::from(cr3 >> 12)), PageTableFrameRole::Pdpt);
                        }
                        frames.push(
                            PhysFrame(if pae { 4 } else { u64::from(cr3 >> 12) }),
                            PageTableFrameRole::Directory,
                        );
                        frames.push(PhysFrame(2), PageTableFrameRole::Table);
                        let translation = Translation::new(
                            PhysAddr(0x8000),
                            TranslationPageSize::Size4K,
                            frames,
                            true,
                            false,
                            false,
                            false,
                        );
                        runtime.after_linear_translate_ram(
                            true,
                            false,
                            LinearAddr(linear),
                            AccessKind::Read,
                            priv_,
                            cr0,
                            cr3,
                            cr4,
                            2,
                            translation,
                        );
                    }
                }
            }
            // The write straddles two leaf words, shared by both address spaces.
            let width = if pae { 8 } else { 4 };
            runtime.observe_phys_write(PhysWrite::new(
                WriteOrigin::Device,
                PhysAddr(0x2000 + width - 1),
                2,
            ));
            for cr3 in [0x1000, 0x3000] {
                runtime.select_context(ContextKey::new(cr0, cr3, cr4));
                for window in [ShadowWindow::Supervisor, ShadowWindow::User] {
                    for addr in [0x400000, 0x401000] {
                        assert!(runtime.descriptor_for(window, LinearAddr(addr)).is_none());
                    }
                    assert_eq!(
                        runtime
                            .descriptor_for(window, LinearAddr(0x600000))
                            .is_some(),
                        !pae
                    );
                    for addr in [0x402000, 0x602000] {
                        assert!(runtime.descriptor_for(window, LinearAddr(addr)).is_some());
                    }
                }
                assert!(runtime.core.frame_has_users(PhysFrame(2)));
                runtime.flush_for_invlpg(LinearAddr(0x402123));
                for window in [ShadowWindow::Supervisor, ShadowWindow::User] {
                    assert!(
                        runtime
                            .descriptor_for(window, LinearAddr(0x402000))
                            .is_none()
                    );
                    assert!(
                        runtime
                            .descriptor_for(window, LinearAddr(0x602000))
                            .is_some()
                    );
                }
            }
            // Replacing the relevant upper word revokes the remaining leaves,
            // including fragments in the upper half of a legacy 4 MiB range.
            for cr3 in [0x1000, 0x3000] {
                let pde = if pae { 0x4000 + 3 * 8 } else { cr3 as u64 + 4 };
                runtime.observe_phys_write(PhysWrite::new(WriteOrigin::Cpu, PhysAddr(pde), 4));
            }
            for cr3 in [0x1000, 0x3000] {
                runtime.select_context(ContextKey::new(cr0, cr3, cr4));
                for window in [ShadowWindow::Supervisor, ShadowWindow::User] {
                    assert!(
                        runtime
                            .descriptor_for(window, LinearAddr(0x602000))
                            .is_none()
                    );
                }
            }
            assert!(!runtime.core.frame_has_users(PhysFrame(2)));
        }
    }

    #[test]
    fn native_read_helpers_select_data_windows_and_reject_split_reads() {
        assert_eq!(
            native_read_window(AccessKind::Read, AccessPriv::Supervisor),
            Some(ShadowWindow::Supervisor)
        );
        assert_eq!(
            native_read_window(AccessKind::Read, AccessPriv::User),
            Some(ShadowWindow::User)
        );
        assert_eq!(
            native_read_window(AccessKind::Read, AccessPriv::ImplicitSupervisor),
            Some(ShadowWindow::Supervisor)
        );
        assert_eq!(
            native_read_window(AccessKind::Execute, AccessPriv::Supervisor),
            None
        );
        assert_eq!(
            native_read_window(AccessKind::Write, AccessPriv::Supervisor),
            None
        );

        assert!(native_read_in_single_page(LinearAddr(0x0fff), 1));
        assert!(!native_read_in_single_page(LinearAddr(0x0fff), 2));
        assert!(native_read_in_single_page(LinearAddr(0x0ffc), 4));
        assert!(!native_read_in_single_page(LinearAddr(0x0ffd), 4));
        assert!(!native_read_in_single_page(LinearAddr(0xffff_fffe), 4));
    }

    #[test]
    fn stage1_l2_mapping_rejects_unaligned_ranges() {
        let mut table = [0_u64; STAGE1_TABLE_ENTRIES];
        assert_eq!(
            map_l2_range(
                &mut table,
                0x0020_0000,
                0x0030_0000,
                normal_xn_block_descriptor
            ),
            Err(MonitorError::Stage1MisalignedMapping {
                base: 0x0020_0000,
                len: 0x0030_0000
            })
        );
    }

    #[test]
    fn stop_ack_packet_encodes_final_cpu_state_and_io_stats() {
        let packet = MonitorStopAckPacket {
            steps: 99,
            eip: 0x0010_0000,
            eflags: 0x0000_0202,
            phys_writes: 0x8000_0011,
            mmio_writes: 0x0012_3000,
            pio_outs: 0x1234_5000,
            irq_polls: 0x0000_0030,
        };
        let request = packet.request(3);

        assert_eq!(request.kind, MonitorRequestKindV1::StopAck as u32);
        assert_eq!(request.seq, 3);
        assert_eq!(request.a, 99);
        assert_eq!(request.b, 0x0000_0202_0010_0000);
        assert_eq!(request.c, 0x0012_3000_8000_0011);
        assert_eq!(request.d, 0x0000_0030_1234_5000);
    }

    #[test]
    fn hardmmu_stats_packets_encode_cpu_bus_and_shadow_counters() {
        let mut cpu = Cpu::new();
        cpu.cr0 = 0x8000_0011;
        cpu.cr3 = 0x0012_3000;
        cpu.cr4 = CR4_PSE | CR4_PAE;
        cpu.eflags = 0x0000_0202;
        cpu.cache_stats.fetch_translation_hits = 37;
        cpu.cache_stats.fetch_translation_fills = 38;
        cpu.cache_stats.fetch_translation_invalidations = 39;
        cpu.cache_stats.linear_translation_hits = 40;
        cpu.cache_stats.linear_translation_fills = 41;
        cpu.cache_stats.linear_translation_invalidations = 42;
        let report = X86MonitorStepReport {
            steps: 0x1_0000_0001,
            halted: false,
            eip: 0x0010_0000,
            tsc: 0,
            bus: X86MonitorBusStats {
                phys_reads: 9,
                tracked_writes: 10,
                native_read_attempts: 1,
                native_read_hits: 2,
                native_read_aborts: 3,
                native_read_descriptor_aborts: 29,
                native_read_translation_aborts: 31,
                native_read_access_flag_aborts: 32,
                native_read_permission_aborts: 33,
                page_table_frames: 4,
                ..X86MonitorBusStats::default()
            },
        };
        let shadow = MonitorShadowRuntimeStats {
            translations: 11,
            eligible_4k: 12,
            read_fills: 13,
            write_fills: 14,
            upgrades: 15,
            unchanged_fills: 37,
            soft_non_ram: 16,
            soft_large_or_direct: 17,
            soft_unsupported_mode: 18,
            soft_pt_protected: 19,
            stage1_installs: 20,
            stage1_flushes: 21,
            stage1_leaf_invalidations: 39,
            page_table_write_flushes: 22,
            page_table_write_invalidations: 40,
            page_walk_write_ignores: 38,
            control_flushes: 23,
            invlpg_flushes: 24,
            native_read_attempts: 25,
            native_read_hits: 26,
            native_read_aborts: 27,
            native_read_descriptor_aborts: 30,
            native_read_translation_aborts: 34,
            native_read_access_flag_aborts: 35,
            native_read_permission_aborts: 36,
            shadow_errors: 28,
            context_hits: 43,
            context_misses: 44,
            context_evictions: 45,
        };

        let cpu_packet = MonitorHardMmuStatsPacket::from_cpu_report_and_shadow_part(
            &cpu,
            Some(&report),
            shadow,
            0,
        )
        .unwrap()
        .request(7);
        assert_eq!(cpu_packet.kind, MonitorRequestKindV1::HardMmuStats as u32);
        assert_eq!(cpu_packet.a, 0);
        assert_eq!(cpu_packet.b, 0x0012_3000_8000_0011);
        assert_eq!(cpu_packet.c, 0x0000_0202_0000_0030);
        assert_eq!(cpu_packet.d, 0x1_0000_0001);

        let bus_packet = MonitorHardMmuStatsPacket::from_cpu_report_and_shadow_part(
            &cpu,
            Some(&report),
            shadow,
            1,
        )
        .unwrap()
        .request(8);
        assert_eq!(bus_packet.b, 0x0000_0002_0000_0001);
        assert_eq!(bus_packet.c, 0x0000_0004_0000_0003);
        assert_eq!(bus_packet.d, 0x0000_000a_0000_0009);

        let fill_packet = MonitorHardMmuStatsPacket::from_cpu_report_and_shadow_part(
            &cpu,
            Some(&report),
            shadow,
            2,
        )
        .unwrap()
        .request(9);
        assert_eq!(fill_packet.b, 0x0000_000c_0000_000b);
        assert_eq!(fill_packet.c, 0x0000_000e_0000_000d);
        assert_eq!(fill_packet.d, 0x0000_0014_0000_000f);

        let native_packet = MonitorHardMmuStatsPacket::from_cpu_report_and_shadow_part(
            &cpu,
            Some(&report),
            shadow,
            4,
        )
        .unwrap()
        .request(10);
        assert_eq!(native_packet.b, 0x0000_0017_0000_0016);
        assert_eq!(native_packet.c, 0x0000_0019_0000_0018);
        assert_eq!(native_packet.d, 0x0000_001b_0000_001a);
        let abort_packet = MonitorHardMmuStatsPacket::from_cpu_report_and_shadow_part(
            &cpu,
            Some(&report),
            shadow,
            5,
        )
        .unwrap()
        .request(11);
        assert_eq!(abort_packet.b, 0x0000_001e_0000_001d);
        assert_eq!(abort_packet.c, 0x0000_0022_0000_001f);
        assert_eq!(abort_packet.d, 0x0000_0023_0000_0020);
        let perm_packet = MonitorHardMmuStatsPacket::from_cpu_report_and_shadow_part(
            &cpu,
            Some(&report),
            shadow,
            6,
        )
        .unwrap()
        .request(12);
        assert_eq!(perm_packet.b, 0x0000_0024_0000_0021);
        assert_eq!(perm_packet.c, 0x0000_002c_0000_002b);
        assert_eq!(perm_packet.d, 45);
        let cache_packet = MonitorHardMmuStatsPacket::from_cpu_report_and_shadow_part(
            &cpu,
            Some(&report),
            shadow,
            7,
        )
        .unwrap()
        .request(13);
        assert_eq!(cache_packet.b, 0x0000_0026_0000_0025);
        assert_eq!(cache_packet.c, 0x0000_0028_0000_0027);
        assert_eq!(cache_packet.d, 0x0000_002a_0000_0029);
        assert!(
            MonitorHardMmuStatsPacket::from_cpu_report_and_shadow_part(
                &cpu,
                Some(&report),
                shadow,
                8
            )
            .is_none()
        );
    }

    #[cfg(not(target_arch = "aarch64"))]
    #[test]
    fn hardmmu_paged_smoke_installs_stage1_leaf_via_non_pae_walk() {
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                let mut ram_bytes = [0_u8; 0xa000];
                let mut ram = MonitorRam::from_slice(&mut ram_bytes);
                let mut mailbox = mailbox_bytes();
                let mut client = MailboxClient::new(&mut mailbox).unwrap();
                let mut doorbell = EchoDoorbell::expecting(MonitorRequestKindV1::None, 0);
                doorbell.expected_kind = None;
                let regions = all_ram_regions(ram.len());
                let mut stage1 = MonitorShadowStage1Tables::<MONITOR_SHADOW_L3_TABLES>::new();
                let mut shadow = DefaultMonitorShadowRuntime::new(&mut stage1);
                let mut cpu = Cpu::new();

                let report = run_hardmmu_paged_smoke(
                    &mut client,
                    &mut doorbell,
                    &mut ram,
                    regions,
                    &mut cpu,
                    &mut shadow,
                    HARDMMU_PAGED_SMOKE_MAX_STEPS,
                )
                .unwrap();

                assert!(report.halted);
                assert!(hardmmu_paged_smoke_registers_match(&cpu));
                assert!(shadow.stats().context_hits >= 5);
                assert_eq!(cpu.cr3, HARDMMU_PAGED_PD_BASE);
                assert_eq!(cpu.cr4, 0);
                assert!(shadow.stats().stage1_installs > 0);
                assert!(shadow.stats().eligible_4k > 0);
                assert_eq!(shadow.stats().soft_unsupported_mode, 0);
                assert_eq!(shadow.stats().native_read_descriptor_aborts, 0);
                assert_eq!(shadow.stats().shadow_errors, 0);
                assert_eq!(report.bus.native_read_hits, 0);
                assert_eq!(report.bus.native_read_descriptor_aborts, 0);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    struct PioRecordingDoorbell {
        rings: u64,
        serial: [u8; 64],
        serial_len: usize,
    }

    impl PioRecordingDoorbell {
        const fn new() -> Self {
            Self {
                rings: 0,
                serial: [0; 64],
                serial_len: 0,
            }
        }

        fn serial_output(&self) -> &[u8] {
            &self.serial[..self.serial_len]
        }
    }

    impl Doorbell for PioRecordingDoorbell {
        fn ring(&mut self, mailbox: &mut [u8]) -> Result<(), MonitorError> {
            self.rings = self.rings.wrapping_add(1);
            let request = read_monitor_request_for_test(mailbox)?;
            let response = if request.kind == MonitorRequestKindV1::PioOut as u32 {
                assert_eq!(request.a, 0x03f8);
                assert_eq!(request.b, 1);
                assert!(self.serial_len < self.serial.len());
                self.serial[self.serial_len] = request.c as u8;
                self.serial_len += 1;
                HostResponseV1::ok(request.seq, 0)
            } else if request.kind == MonitorRequestKindV1::PioIn as u32 {
                HostResponseV1::ok(request.seq, 0)
            } else {
                HostResponseV1::unsupported(request.seq)
            };
            write_host_response_for_test(mailbox, &response)
        }
    }

    fn mailbox_bytes() -> [u8; core::mem::size_of::<MailboxV1>()] {
        let mut bytes = [0_u8; core::mem::size_of::<MailboxV1>()];
        init_mailbox_header(&mut bytes).unwrap();
        bytes
    }

    fn all_ram_regions(size: u64) -> RegionTableV1 {
        let mut regions = RegionTableV1::new();
        regions.push(RegionEntryV1::ram(0, size)).unwrap();
        regions
    }

    #[test]
    fn boot_block_offsets_produce_absolute_monitor_ipas() {
        assert_eq!(BOOT_BLOCK_MAILBOX_OFFSET_FIELD_OFFSET, 496);
        let layout = IpaLayoutV1::new(128 * 1024 * 1024, 2 * 1024 * 1024, 64 * 1024, 64 * 1024);
        let mut boot = BootBlockV1::new(layout);
        boot.regions
            .push(RegionEntryV1::ram(0, layout.ram_size))
            .unwrap();
        boot.initial_cpu_state_offset = PAGE_SIZE_4K;
        boot.mailbox_offset = PAGE_SIZE_4K * 2;
        boot.set_debug_state_offset(PAGE_SIZE_4K * 3);
        boot.set_panic_block_offset(PAGE_SIZE_4K * 4);
        boot.trace_offset = 0x1000;
        boot.timer_poll_interval = 12;
        boot.step_limit = 34;
        boot.trace_mode = 2;
        boot.feature_bits = 0x55aa;

        let parsed = MonitorBoot::from_boot_block(0x1400_0000, &boot, 34).unwrap();

        assert_eq!(parsed.cpu_state_ipa, layout.shared_base + PAGE_SIZE_4K);
        assert_eq!(parsed.mailbox_ipa, layout.shared_base + PAGE_SIZE_4K * 2);
        assert_eq!(
            parsed.debug_state_ipa,
            layout.shared_base + PAGE_SIZE_4K * 3
        );
        assert_eq!(
            parsed.panic_block_ipa,
            layout.shared_base + PAGE_SIZE_4K * 4
        );
        assert_eq!(parsed.trace_ipa, layout.trace_base + 0x1000);
        assert_eq!(parsed.ram_base, abi::X86_RAM_IPA_BASE);
        assert_eq!(parsed.ram_size, layout.ram_size);
        assert_eq!(parsed.regions.count, 1);
        assert_eq!(parsed.timer_poll_interval, 12);
        assert_eq!(parsed.step_limit, 34);
        assert_eq!(parsed.trace_mode, 2);
        assert_eq!(parsed.feature_bits, 0x55aa);
    }

    #[test]
    fn debug_publisher_writes_seqlocked_state_block() {
        let mut block = DebugStateBlockV1::new();
        let mut publisher = unsafe { MonitorDebugPublisher::new(&mut block) };
        publisher.publish(MonitorDebugSnapshot::boot_phase(DebugPhaseV1::Hello));

        assert_eq!(block.magic, crate::monitor::abi::debug::DEBUG_MAGIC);
        assert_eq!(block.version, crate::monitor::abi::debug::DEBUG_ABI_VERSION);
        assert_eq!(block.phase, DebugPhaseV1::Hello as u32);
        assert_eq!(block.heartbeat, 1);
        assert_eq!(block.seq & 1, 0);
        assert!(block.seq >= 2);
    }

    #[test]
    fn monitor_ram_maps_guest_page_zero_through_a_nonzero_pointer() {
        let mut bytes = [0; PAGE_SIZE_4K as usize];
        let mut ram =
            unsafe { MonitorRam::from_raw_parts(bytes.as_mut_ptr() as u64, PAGE_SIZE_4K).unwrap() };
        ram.write_u32(PhysAddr(0), 0x1234_5678).unwrap();
        assert_eq!(ram.read_u32(PhysAddr(0)).unwrap(), 0x1234_5678);
    }

    #[test]
    fn monitor_ram_supports_unaligned_little_endian_scalars() {
        let mut ram_bytes = [0_u8; 16];
        let mut ram = MonitorRam::from_slice(&mut ram_bytes);

        ram.write_u16(PhysAddr(1), 0x1122).unwrap();
        ram.write_u32(PhysAddr(3), 0xa1b2_c3d4).unwrap();
        ram.write_u64(PhysAddr(7), 0x0102_0304_0506_0708).unwrap();

        assert_eq!(ram.read_u16(PhysAddr(1)).unwrap(), 0x1122);
        assert_eq!(ram.read_u32(PhysAddr(3)).unwrap(), 0xa1b2_c3d4);
        assert_eq!(ram.read_u64(PhysAddr(7)).unwrap(), 0x0102_0304_0506_0708);
        drop(ram);
        assert_eq!(&ram_bytes[1..3], &[0x22, 0x11]);
        assert_eq!(&ram_bytes[3..7], &[0xd4, 0xc3, 0xb2, 0xa1]);
    }

    #[test]
    fn x86_monitor_smoke_runs_shared_interpreter_and_emits_com1_marker() {
        let mut ram_bytes = [0_u8; 0xa000];
        let mut ram = MonitorRam::from_slice(&mut ram_bytes);
        let mut mailbox = mailbox_bytes();
        let mut client = MailboxClient::new(&mut mailbox).unwrap();
        let mut doorbell = PioRecordingDoorbell::new();
        let initial = CpuStateV1::protected_flat_linux_entry(0x0010_0000, 0x0009_0000);
        let regions = all_ram_regions(ram.len());

        let report = run_x86_monitor_smoke(
            &mut client,
            &mut doorbell,
            &mut ram,
            regions,
            &initial,
            X86_MONITOR_SMOKE_MAX_STEPS,
        )
        .unwrap();

        assert!(report.halted);
        assert_eq!(report.bus.pio_out, X86_MONITOR_SMOKE_MARKER.len() as u64);
        assert_eq!(doorbell.serial_output(), X86_MONITOR_SMOKE_MARKER);
        assert_eq!(doorbell.rings, X86_MONITOR_SMOKE_MARKER.len() as u64);
        assert_eq!(
            report.steps,
            1 + (X86_MONITOR_SMOKE_MARKER.len() as u32 * 2) + 1
        );
    }

    #[test]
    fn cpu_state_v1_conversion_preserves_scalar_and_segment_state() {
        let mut initial = CpuStateV1::protected_flat_linux_entry(0x0010_0000, 0x0009_0000);
        initial.pae_pdptrs = [0x2001, 0x3001, 0, 0];
        initial.dr[7] = 0x0000_0400;
        initial.msr.sysenter_cs = 0x10;
        initial.msr.sysenter_esp = 0xcafe_babe;
        initial.msr.sysenter_eip = 0xc001_f00d;
        initial.msr.perfctr = [0x1111_2222_3333_4444, 0x5555_6666_7777_8888];
        initial.fpu.status = 0x3800;
        initial.fpu.top = 7;
        initial.fpu.st_bits[0] = 1.25f64.to_bits();
        initial.fpu.mm[0] = 0x0123_4567_89ab_cdef;
        initial.fpu.mmx_active = 1;

        let cpu = cpu_from_state_v1(&initial);
        let roundtrip = cpu_state_v1_from_cpu(&cpu, 17);

        assert_eq!(cpu.eip, initial.eip);
        assert_eq!(cpu.reg(Reg32::Esi), 0x0009_0000);
        assert_eq!(roundtrip.eip, initial.eip);
        assert_eq!(roundtrip.gpr, initial.gpr);
        assert_eq!(roundtrip.seg, initial.seg);
        assert_eq!(roundtrip.cr0, initial.cr0);
        assert_eq!(roundtrip.pae_pdptrs, initial.pae_pdptrs);
        assert_eq!(roundtrip.dr, initial.dr);
        assert_eq!(roundtrip.msr.sysenter_cs, 0x10);
        assert_eq!(roundtrip.msr.sysenter_esp, 0xcafe_babe);
        assert_eq!(roundtrip.msr.sysenter_eip, 0xc001_f00d);
        assert_eq!(roundtrip.msr.perfctr, initial.msr.perfctr);
        assert_eq!(roundtrip.fpu.status, initial.fpu.status);
        assert_eq!(roundtrip.fpu.top, initial.fpu.top);
        assert_eq!(roundtrip.fpu.st_bits[0], initial.fpu.st_bits[0]);
        assert_eq!(roundtrip.fpu.mm[0], initial.fpu.mm[0]);
        assert_eq!(roundtrip.fpu.mmx_active, initial.fpu.mmx_active);
        assert_eq!(roundtrip.instr_count, 17);
    }

    #[test]
    fn mailbox_header_validation_is_exact() {
        let mut mailbox = mailbox_bytes();

        validate_mailbox_header(&mailbox).unwrap();
        mailbox[MAILBOX_ABI_VERSION_OFFSET] = 0xff;
        assert_eq!(
            validate_mailbox_header(&mailbox),
            Err(MonitorError::BadMailboxVersion { version: 0x00ff })
        );
        init_mailbox_header(&mut mailbox).unwrap();
        mailbox[MAILBOX_HEADER_SIZE_OFFSET] = 0;
        assert_eq!(
            validate_mailbox_header(&mailbox),
            Err(MonitorError::BadMailboxSize { size: 0 })
        );
    }

    #[test]
    fn hello_transaction_publishes_request_and_counts_doorbell() {
        let mut mailbox = mailbox_bytes();
        let mut client = MailboxClient::new(&mut mailbox).unwrap();
        let mut doorbell = EchoDoorbell::expecting(MonitorRequestKindV1::Hello, 0);

        client.hello(&mut doorbell, 0x22, 0x33).unwrap();

        assert_eq!(doorbell.rings, 1);
        assert_eq!(client.next_seq(), 2);
        assert_eq!(client.stats().hello, 1);
        assert_eq!(client.stats().requests, 1);
    }

    #[test]
    fn pio_transactions_encode_width_port_value_and_vtime() {
        let mut mailbox = mailbox_bytes();
        let mut client = MailboxClient::new(&mut mailbox).unwrap();
        let mut out_doorbell = EchoDoorbell::expecting(MonitorRequestKindV1::PioOut, 0);

        client
            .pio_out(&mut out_doorbell, 0x3f8, 1, b'X' as u32, 99)
            .unwrap();
        let request = read_monitor_request_for_test(client.mailbox).unwrap();
        assert_eq!(request.kind, MonitorRequestKindV1::PioOut as u32);
        assert_eq!(request.a, 0x3f8);
        assert_eq!(request.b, 1);
        assert_eq!(request.c, b'X' as u64);
        assert_eq!(request.d, 99);

        let mut in_doorbell = EchoDoorbell::expecting(MonitorRequestKindV1::PioIn, 0x60);
        let read = client.pio_in(&mut in_doorbell, 0x3fd, 1, 100).unwrap();
        assert_eq!(read.value, 0x60);
        assert_eq!(client.stats().pio_out, 1);
        assert_eq!(client.stats().pio_in, 1);
        assert_eq!(client.stats().last_seq, 2);
    }

    #[test]
    fn mmio_transactions_encode_gpa_width_value_and_vtime() {
        let mut mailbox = mailbox_bytes();
        let mut client = MailboxClient::new(&mut mailbox).unwrap();
        let mut write_doorbell = EchoDoorbell::expecting(MonitorRequestKindV1::MmioWrite, 0);

        client
            .mmio_write(&mut write_doorbell, 0x000a_0000, 1, b'V' as u64, 77)
            .unwrap();
        let request = read_monitor_request_for_test(client.mailbox).unwrap();
        assert_eq!(request.kind, MonitorRequestKindV1::MmioWrite as u32);
        assert_eq!(request.a, 0x000a_0000);
        assert_eq!(request.b, 1);
        assert_eq!(request.c, b'V' as u64);
        assert_eq!(request.d, 77);

        let mut read_doorbell = EchoDoorbell::expecting(MonitorRequestKindV1::MmioRead, 0x44);
        let value = client
            .mmio_read(&mut read_doorbell, 0x000a_0000, 1, 78)
            .unwrap();
        let request = read_monitor_request_for_test(client.mailbox).unwrap();
        assert_eq!(value, 0x44);
        assert_eq!(request.kind, MonitorRequestKindV1::MmioRead as u32);
        assert_eq!(request.d, 78);
    }

    #[test]
    fn monitor_soft_bus_routes_mmio_region_via_mailbox() {
        struct MmioDoorbellForTest {
            writes: u64,
            last_gpa: u64,
            last_value: u64,
        }

        impl Doorbell for MmioDoorbellForTest {
            fn ring(&mut self, mailbox: &mut [u8]) -> Result<(), MonitorError> {
                let request = read_monitor_request_for_test(mailbox)?;
                let response = if request.kind == MonitorRequestKindV1::MmioWrite as u32 {
                    self.writes += 1;
                    self.last_gpa = request.a;
                    self.last_value = request.c;
                    HostResponseV1::ok(request.seq, 0)
                } else if request.kind == MonitorRequestKindV1::MmioRead as u32 {
                    HostResponseV1::ok(request.seq, 0x5a)
                } else {
                    HostResponseV1::unsupported(request.seq)
                };
                write_host_response_for_test(mailbox, &response)
            }
        }

        let mut ram_bytes = [0_u8; 0x1000];
        let mut ram = MonitorRam::from_slice(&mut ram_bytes);
        let mut regions = RegionTableV1::new();
        regions.push(RegionEntryV1::ram(0, 0x1000)).unwrap();
        regions
            .push(RegionEntryV1::mmio(0x000a_0000, 0x1000))
            .unwrap();
        let mut mailbox = mailbox_bytes();
        let mut client = MailboxClient::new(&mut mailbox).unwrap();
        let mut doorbell = MmioDoorbellForTest {
            writes: 0,
            last_gpa: 0,
            last_value: 0,
        };
        let mut bus = MonitorSoftBus::new(&mut ram, regions, &mut client, &mut doorbell);

        assert_eq!(bus.read_phys_u8(PhysAddr(0x2000)).unwrap(), 0xff);
        bus.probe_phys_write_u8(PhysAddr(0x2000)).unwrap();
        bus.write_phys_u8(PhysAddr(0x2000), 0xaa).unwrap();
        assert_eq!(bus.read_phys_u8(PhysAddr(0x000a_0000)).unwrap(), 0x5a);
        bus.probe_phys_write_u8(PhysAddr(0x000a_0001)).unwrap();
        bus.write_phys_u8(PhysAddr(0x000a_0001), b'Q').unwrap();
        let stats = bus.stats();
        drop(bus);

        assert_eq!(doorbell.writes, 1);
        assert_eq!(doorbell.last_gpa, 0x000a_0001);
        assert_eq!(doorbell.last_value, b'Q' as u64);
        assert_eq!(stats.mmio_read, 1);
        assert_eq!(stats.mmio_write, 1);
    }

    #[test]
    fn monitor_soft_bus_routes_wide_ram_and_mmio_accesses() {
        struct WideMmioDoorbell {
            writes: u64,
            last_gpa: u64,
            last_width: u64,
            last_value: u64,
        }

        impl Doorbell for WideMmioDoorbell {
            fn ring(&mut self, mailbox: &mut [u8]) -> Result<(), MonitorError> {
                let request = read_monitor_request_for_test(mailbox)?;
                let response = if request.kind == MonitorRequestKindV1::MmioWrite as u32 {
                    self.writes += 1;
                    self.last_gpa = request.a;
                    self.last_width = request.b;
                    self.last_value = request.c;
                    HostResponseV1::ok(request.seq, 0)
                } else if request.kind == MonitorRequestKindV1::MmioRead as u32 {
                    HostResponseV1::ok(request.seq, 0x1122_3344)
                } else {
                    HostResponseV1::unsupported(request.seq)
                };
                write_host_response_for_test(mailbox, &response)
            }
        }

        let mut ram_bytes = [0_u8; 0x1000];
        let mut ram = MonitorRam::from_slice(&mut ram_bytes);
        let mut regions = RegionTableV1::new();
        regions.push(RegionEntryV1::ram(0, 0x1000)).unwrap();
        regions
            .push(RegionEntryV1::mmio(0x000a_0000, 0x1000))
            .unwrap();
        let mut mailbox = mailbox_bytes();
        let mut client = MailboxClient::new(&mut mailbox).unwrap();
        let mut doorbell = WideMmioDoorbell {
            writes: 0,
            last_gpa: 0,
            last_width: 0,
            last_value: 0,
        };
        let mut bus = MonitorSoftBus::new(&mut ram, regions, &mut client, &mut doorbell);

        bus.write_phys_u32(PhysAddr(4), 0x5566_7788).unwrap();
        assert_eq!(bus.read_phys_u32(PhysAddr(4)).unwrap(), 0x5566_7788);
        assert_eq!(
            bus.read_phys_u32(PhysAddr(0x000a_0000)).unwrap(),
            0x1122_3344
        );
        bus.write_phys_u16(PhysAddr(0x000a_0002), 0xabcd).unwrap();
        let stats = bus.stats();
        drop(bus);

        assert_eq!(doorbell.writes, 1);
        assert_eq!(doorbell.last_gpa, 0x000a_0002);
        assert_eq!(doorbell.last_width, 2);
        assert_eq!(doorbell.last_value, 0xabcd);
        assert_eq!(stats.phys_reads, 8);
        assert_eq!(stats.phys_writes, 6);
        assert_eq!(stats.mmio_read, 1);
        assert_eq!(stats.mmio_write, 1);
    }

    #[test]
    fn monitor_backing_tokens_preserve_live_ram_and_unbacked_rom() {
        let mut bytes = [0_u8; 4096];
        bytes[..7].copy_from_slice(&[0xb8, 1, 0, 0, 0, 0xeb, 0xf9]);
        let mut ram = MonitorRam::from_slice(&mut bytes);
        let mut regions = all_ram_regions(8192);
        regions
            .push(RegionEntryV1 {
                base: 0xf0000,
                len: 4096,
                kind: RegionKindV1::Rom as u32,
                ..RegionEntryV1::EMPTY
            })
            .unwrap();
        let mut mailbox = mailbox_bytes();
        let mut client = MailboxClient::new(&mut mailbox).unwrap();
        let mut doorbell = EchoDoorbell::expecting(MonitorRequestKindV1::PioOut, 0);
        let mut bus = MonitorSoftBus::new(&mut ram, regions, &mut client, &mut doorbell);
        // A region record cannot grant backing past the actual allocation.
        assert_eq!(
            bus.classify_phys(PhysAddr(4096), 4096),
            PhysRegionKind::Mixed
        );
        assert!(bus.read_phys_u8(PhysAddr(4096)).is_err());
        for (offset, width) in [(4095, 1), (4094, 2), (4092, 4)] {
            let token = bus.cached_backing_token(PhysAddr(0xf0000 + offset));
            let value = match width {
                1 => u32::from(bus.read_backing_token_u8(token)),
                2 => u32::from(bus.read_backing_token_u16(token)),
                _ => bus.read_backing_token_u32(token),
            };
            assert_eq!(value, u32::MAX >> (32 - width * 8));
        }
        let mut cpu = Cpu::new();
        cpu.enter_protected_flat(0, 0);
        Interpreter::step(&mut cpu, &mut bus).unwrap();
        assert_eq!(cpu.reg(Reg32::Eax), 1);
        let token = bus.cached_backing_token(PhysAddr(1));
        bus.write_backing_token_u32(token, 0x7856_3412).unwrap();
        assert_eq!(bus.cached_backing_phys(token), PhysAddr(1));
        Interpreter::step(&mut cpu, &mut bus).unwrap();
        Interpreter::step(&mut cpu, &mut bus).unwrap();
        assert_eq!(cpu.reg(Reg32::Eax), 0x7856_3412);
        assert_eq!(cpu.cache_stats.fetch_translation_fills, 1);
        assert_eq!(doorbell.rings, 0);
    }

    #[test]
    fn monitor_rep_ram_chunks_preserve_patterns_and_reject_partial_regions() {
        let mut bytes = [0_u8; 4096];
        let mut ram = MonitorRam::from_slice(&mut bytes);
        let mut mailbox = mailbox_bytes();
        let mut client = MailboxClient::new(&mut mailbox).unwrap();
        let mut doorbell = EchoDoorbell::expecting(MonitorRequestKindV1::PioOut, 0);
        let mut bus =
            MonitorSoftBus::new(&mut ram, all_ram_regions(4096), &mut client, &mut doorbell);
        for width in [1, 2, 4] {
            let value: u32 = 0x7856_3412;
            bus.write_phys_ram_repeat(PhysAddr(129), width, value, 7)
                .unwrap();
            let len = u32::from(width) * 7;
            bus.copy_phys_to_ram(PhysAddr(129), PhysAddr(513), len, width)
                .unwrap();
            for offset in 0..len {
                assert_eq!(
                    bus.read_phys_u8(PhysAddr(513 + u64::from(offset))).unwrap(),
                    value.to_le_bytes()[offset as usize % width as usize]
                );
            }
        }
        assert!(!bus.phys_ram_copy_supported(PhysAddr(129), PhysAddr(130), 28));
        assert!(!bus.phys_ram_repeat_write_supported(PhysAddr(4095), 2));
        assert!(
            bus.write_phys_ram_repeat(PhysAddr(4095), 2, 0xffff, 1)
                .is_err()
        );
        assert_eq!(bus.read_phys_u8(PhysAddr(4095)).unwrap(), 0);
        assert_eq!(doorbell.rings, 0);
    }

    #[test]
    fn monitor_write_cache_revokes_new_page_tables_and_tracks_inactive_owners() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let mut bytes = vec![0_u8; 0x9000];
                let mut ram = MonitorRam::from_slice(&mut bytes);
                for (addr, value) in [
                    (0x1000, 0x2003),
                    (0x1004, 0x5003),
                    (0x2010, 0x5003),
                    (0x3000, 0x8003),
                    (0x8010, 0x5003),
                    (0x5000, 0x6003),
                    (0x6000, 0x1122_3344),
                    (0x7000, 0x5566_7788),
                ] {
                    ram.write_u32(PhysAddr(addr), value).unwrap();
                }
                let mut cpu = Cpu::new();
                cpu.enter_protected_flat(0, 0);
                cpu.cr0 |= CR0_PG;
                cpu.cr3 = 0x1000;
                let mut stage1 = MonitorShadowStage1Tables::<MONITOR_SHADOW_L3_TABLES>::new();
                let mut shadow = DefaultMonitorShadowRuntime::new(&mut stage1);
                shadow.reset(&cpu);
                let mut mailbox = mailbox_bytes();
                let mut client = MailboxClient::new(&mut mailbox).unwrap();
                let mut doorbell = EchoDoorbell::expecting(MonitorRequestKindV1::PioOut, 0);
                let mut bus = MonitorSoftBus::new_with_shadow(
                    &mut ram,
                    all_ram_regions(0x9000),
                    &mut client,
                    &mut doorbell,
                    &mut shadow,
                );
                // The alias is ordinary data until a walk discovers frame 5 as a PT.
                crate::x86::mmu::write_linear_u32(&mut cpu, &mut bus, LinearAddr(0x4010), 0)
                    .unwrap();
                assert!(
                    cpu.cached_write_translation(LinearAddr(0x4000), false)
                        .is_some()
                );
                assert_eq!(
                    crate::x86::mmu::read_linear_u32_at_cpl(
                        &mut cpu,
                        &mut bus,
                        LinearAddr(0x400000),
                        AccessKind::Read,
                        0
                    )
                    .unwrap(),
                    0x1122_3344
                );
                assert!(
                    cpu.cached_write_translation(LinearAddr(0x4000), false)
                        .is_none()
                );
                cpu.cr3 = 0x3000;
                cpu.invalidate_translation_context();
                bus.after_control_register_write(3, 0x1000, 0x3000);
                crate::x86::mmu::write_linear_u32(&mut cpu, &mut bus, LinearAddr(0x4000), 0x7003)
                    .unwrap();
                assert!(
                    cpu.cached_write_translation(LinearAddr(0x4000), false)
                        .is_none()
                );
                // A bulk store to an inactive owner's PT must also dirty that owner.
                assert!(
                    crate::x86::mmu::write_probed_phys_repeat(
                        &mut cpu,
                        &mut bus,
                        PhysAddr(0x5000),
                        4,
                        0x7003,
                        2
                    )
                    .unwrap()
                );
                cpu.cr3 = 0x1000;
                cpu.invalidate_translation_context();
                bus.after_control_register_write(3, 0x3000, 0x1000);
                assert_eq!(
                    crate::x86::mmu::read_linear_u32_at_cpl(
                        &mut cpu,
                        &mut bus,
                        LinearAddr(0x400000),
                        AccessKind::Read,
                        0
                    )
                    .unwrap(),
                    0x5566_7788
                );
                assert!(!bus.phys_ram_repeat_write_supported(PhysAddr(0x5000), 8));
                assert_eq!(
                    bus.shadow
                        .as_deref_mut()
                        .unwrap()
                        .descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x400000))
                        .unwrap()
                        & 0x0000_ffff_ffff_f000,
                    abi::X86_RAM_IPA_BASE + 0x7000
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn large_paged_image_preserves_cpu_and_written_ram_across_cache_eviction() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let mut bytes = vec![0_u8; 0x1000000];
                let mut ram = MonitorRam::from_slice(&mut bytes);
                let mut stage1 = MonitorShadowStage1Tables::<MONITOR_SHADOW_L3_TABLES>::new();
                let mut shadow = DefaultMonitorShadowRuntime::new(&mut stage1);
                let mut mailbox = mailbox_bytes();
                let mut client = MailboxClient::new(&mut mailbox).unwrap();
                let mut doorbell = EchoDoorbell::expecting(MonitorRequestKindV1::PioOut, 0);
                let mut cpu = Cpu::new();
                let report = run_hardmmu_large_paged_smoke(
                    &mut client,
                    &mut doorbell,
                    &mut ram,
                    all_ram_regions(0x1000000),
                    &mut cpu,
                    &mut shadow,
                    HARDMMU_PAGED_SMOKE_MAX_STEPS,
                )
                .unwrap();
                assert_eq!(report.steps, 23);
                assert!(report.halted && hardmmu_large_paged_smoke_registers_match(&cpu));
                assert!(shadow.stats().upgrades >= 4);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn invlpg_range_covers_both_large_halves_and_windows_without_losing_other_maps() {
        for base in [0x0040_0000, 0xffc0_0000] {
            let mut tables = MonitorShadowStage1Tables::<8>::new();
            for window in [ShadowWindow::Supervisor, ShadowWindow::User] {
                tables
                    .install_block(window, LinearAddr(base), PhysAddr(0x400000))
                    .unwrap();
                tables
                    .install_leaf(window, LinearAddr(base + 0x201000), PhysAddr(0x7000))
                    .unwrap();
                tables
                    .install_leaf(window, LinearAddr(0x1000), PhysAddr(0x8000))
                    .unwrap();
            }
            tables.activate(1);
            tables
                .install_leaf(ShadowWindow::Supervisor, LinearAddr(base), PhysAddr(0x9000))
                .unwrap();
            tables.activate(0);
            assert_eq!(tables.invalidate_linear(LinearAddr(base + 0x3ff123)), 4);
            for window in [ShadowWindow::Supervisor, ShadowWindow::User] {
                assert!(tables.descriptor_for(window, LinearAddr(base)).is_none());
                assert!(
                    tables
                        .descriptor_for(window, LinearAddr(base + 0x201000))
                        .is_none()
                );
                assert!(tables.descriptor_for(window, LinearAddr(0x1000)).is_some());
            }
            // Repeated invalidation and allocation must not leak old L3 slots.
            for _ in 0..16 {
                tables
                    .install_leaf(
                        ShadowWindow::User,
                        LinearAddr(base + 0x201000),
                        PhysAddr(0xa000),
                    )
                    .unwrap();
                assert_eq!(tables.invalidate_linear(LinearAddr(base)), 1);
                assert_eq!(tables.invalidate_linear(LinearAddr(base)), 0);
            }
            tables.activate(1);
            assert_eq!(
                tables
                    .descriptor_for(ShadowWindow::Supervisor, LinearAddr(base))
                    .unwrap()
                    & 0x0000_ffff_ffff_f000,
                abi::X86_RAM_IPA_BASE + 0x9000
            );
        }
    }

    #[test]
    fn invlpg_keeps_dependencies_and_applies_pending_writes_before_retention() {
        let mut tables = MonitorShadowStage1Tables::<8>::new();
        let mut runtime = MonitorShadowRuntime::<16, 8>::new(&mut tables);
        let mut cpu = Cpu::new();
        cpu.cr0 = CR0_PE | CR0_PG;
        cpu.cr3 = 0x1000;
        runtime.reset(&cpu);
        let fill = |runtime: &mut MonitorShadowRuntime<16, 8>, linear| {
            runtime.after_linear_translate_ram(
                true,
                false,
                LinearAddr(linear),
                AccessKind::Read,
                AccessPriv::Supervisor,
                cpu.cr0,
                cpu.cr3,
                0,
                2,
                walked_translation(0x5000, TranslationPageSize::Size4K, AccessKind::Read),
            );
        };
        fill(&mut runtime, 0x400000);
        fill(&mut runtime, 0x800000);
        runtime.flush_for_invlpg(LinearAddr(0x400123));
        assert!(
            runtime
                .descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x400000))
                .is_none()
        );
        assert!(
            runtime
                .descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x800000))
                .is_some()
        );
        runtime.observe_phys_write(PhysWrite::new(WriteOrigin::Device, PhysAddr(0x2000), 4));
        assert!(
            runtime
                .descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x800000))
                .is_none()
        );
        fill(&mut runtime, 0x400000);
        fill(&mut runtime, 0x800000);
        runtime.observe_phys_write(PhysWrite::new(WriteOrigin::Cpu, PhysAddr(0x2000), 4));
        runtime.flush_for_invlpg(LinearAddr(0x400123));
        assert!(
            runtime
                .descriptor_for(ShadowWindow::Supervisor, LinearAddr(0x800000))
                .is_none()
        );
        assert_eq!(runtime.stats().page_table_write_flushes, 2);
    }

    #[test]
    fn shadow_blocks_replace_pages_and_flush_only_the_selected_context() {
        let mut stage1 = MonitorShadowStage1Tables::<4>::new();
        let sup = ShadowWindow::Supervisor;
        let user = ShadowWindow::User;
        stage1
            .install_leaf(sup, LinearAddr(0x402000), PhysAddr(0x3000))
            .unwrap();
        stage1
            .install_block(sup, LinearAddr(0x401000), PhysAddr(0x601000))
            .unwrap();
        for offset in [0, 0x2000, 0x1ff000] {
            let descriptor = stage1
                .descriptor_for(sup, LinearAddr(0x400000 + offset))
                .unwrap();
            assert_eq!(
                descriptor & 0x0000_ffff_ffff_f000,
                abi::X86_RAM_IPA_BASE + 0x600000 + u64::from(offset)
            );
            assert_eq!(
                descriptor & ((1 << 7) | (1 << 11) | (1 << 53) | (1 << 54)),
                (1 << 7) | (1 << 11) | (1 << 53) | (1 << 54)
            );
        }
        assert!(stage1.descriptor_for(sup, LinearAddr(0x600000)).is_none());
        assert!(stage1.descriptor_for(user, LinearAddr(0x402000)).is_none());
        assert_eq!(stage1.contexts[0].supervisor_l2[0][2] & 3, 1);
        stage1.active = 1;
        stage1
            .install_block(user, LinearAddr(0x401000), PhysAddr(0x801000))
            .unwrap();
        stage1.active = 0;
        stage1
            .install_leaf(sup, LinearAddr(0x402000), PhysAddr(0x5000))
            .unwrap();
        assert!(stage1.descriptor_for(sup, LinearAddr(0x401000)).is_none());
        assert_eq!(
            stage1.descriptor_for(sup, LinearAddr(0x402000)).unwrap() & 0x0000_ffff_ffff_f000,
            abi::X86_RAM_IPA_BASE + 0x5000
        );
        stage1
            .install_block(sup, LinearAddr(0x402000), PhysAddr(0xa02000))
            .unwrap();
        // Replacements reuse the same parent slot, rather than leaking capacity.
        assert_eq!(stage1.links.iter().filter(|&&link| link != 0).count(), 2);
        stage1.flush_active();
        assert!(stage1.descriptor_for(sup, LinearAddr(0x402000)).is_none());
        stage1.active = 1;
        assert_eq!(
            stage1.descriptor_for(user, LinearAddr(0x5ff000)).unwrap() & 0x0000_ffff_ffff_f000,
            abi::X86_RAM_IPA_BASE + 0x9ff000
        );
    }

    #[test]
    fn large_read_blocks_require_whole_ram_spans_and_revoke_on_inactive_pde_writes() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                use crate::x86::{mem, mmu};
                for pae in [false, true] {
                    for full_span in [false, true] {
                        let mut bytes = vec![0_u8; 0x800000];
                        let mut ram = MonitorRam::from_slice(&mut bytes);
                        let (base, pde) = if pae {
                            (0x200000, 0x2008)
                        } else {
                            (0x400000, 0x1004)
                        };
                        ram.write_u32(PhysAddr(pde), 0x400085).unwrap();
                        ram.write_u32(PhysAddr(0x406101), 0x1122_3344).unwrap();
                        ram.write_u32(PhysAddr(0x6101), 0x5566_7788).unwrap();
                        let mut cpu = Cpu::new();
                        cpu.enter_protected_flat(0, 0);
                        cpu.cr0 |= CR0_PG | CR0_WP;
                        cpu.cr3 = 0x1000;
                        cpu.cr4 = if pae { CR4_PAE } else { CR4_PSE };
                        cpu.pae_pdptrs[0] = 0x2001;
                        let mut stage1 =
                            MonitorShadowStage1Tables::<MONITOR_SHADOW_L3_TABLES>::new();
                        let mut shadow = DefaultMonitorShadowRuntime::new(&mut stage1);
                        shadow.reset(&cpu);
                        let mut mailbox = mailbox_bytes();
                        let mut client = MailboxClient::new(&mut mailbox).unwrap();
                        let mut doorbell = EchoDoorbell::expecting(MonitorRequestKindV1::PioOut, 0);
                        // With a shorter declared region, the physical backing
                        // still exists but part of the 2 MiB span is a hole.
                        let regions = all_ram_regions(if full_span { 0x800000 } else { 0x580000 });
                        let mut bus = MonitorSoftBus::new_with_shadow(
                            &mut ram,
                            regions,
                            &mut client,
                            &mut doorbell,
                            &mut shadow,
                        );
                        let linear = LinearAddr(base + 0x6101);
                        assert_eq!(
                            mmu::read_linear_u32_at_cpl(
                                &mut cpu,
                                &mut bus,
                                linear,
                                AccessKind::Read,
                                3
                            )
                            .unwrap(),
                            0x1122_3344
                        );
                        let runtime = bus.shadow.as_deref_mut().unwrap();
                        assert_eq!(runtime.stats().upgrades, u64::from(full_span));
                        assert_eq!(
                            runtime
                                .descriptor_for(ShadowWindow::User, LinearAddr(base + 0x1ff000))
                                .is_some(),
                            full_span
                        );
                        assert_eq!(bus.ram.read_u32(PhysAddr(pde)).unwrap() & 0x60, 0x20);
                        assert!(matches!(
                            mmu::write_linear_u32_at_cpl(&mut cpu, &mut bus, linear, 0, 3),
                            Err(mmu::Fault::Page(mmu::PageFault { code: 7, .. }))
                        ));
                        assert_eq!(bus.ram.read_u32(PhysAddr(pde)).unwrap() & 0x60, 0x20);
                        // Rewrite the old context's PDE while another context
                        // is selected; returning must not resurrect its block.
                        cpu.cr3 = 0x5000;
                        cpu.invalidate_translation_context();
                        bus.after_control_register_write(3, 0x1000, 0x5000);
                        mem::write_phys_u32(&mut bus, PhysAddr(pde), 0x81).unwrap();
                        cpu.cr3 = 0x1000;
                        cpu.invalidate_translation_context();
                        bus.after_control_register_write(3, 0x5000, 0x1000);
                        assert!(
                            bus.shadow
                                .as_deref_mut()
                                .unwrap()
                                .descriptor_for(ShadowWindow::User, linear)
                                .is_none()
                        );
                        assert!(matches!(
                            mmu::read_linear_u32_at_cpl(
                                &mut cpu,
                                &mut bus,
                                linear,
                                AccessKind::Read,
                                3
                            ),
                            Err(mmu::Fault::Page(mmu::PageFault { code: 5, .. }))
                        ));
                        assert_eq!(cpu.cr2, linear.0);
                        assert_eq!(
                            mmu::read_linear_u32_at_cpl(
                                &mut cpu,
                                &mut bus,
                                linear,
                                AccessKind::Read,
                                0
                            )
                            .unwrap(),
                            0x5566_7788
                        );
                    }
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn large_page_fragments_preserve_permissions_accessed_bits_and_pde_invalidation() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                use crate::x86::{mem, mmu};
                for pae in [false, true] {
                    let mut bytes = vec![0_u8; 0x9000];
                    let mut ram = MonitorRam::from_slice(&mut bytes);
                    let (base, pde) = if pae {
                        (0x200000, 0x2008)
                    } else {
                        (0x400000, 0x1004)
                    };
                    ram.write_u32(PhysAddr(pde), 0x85).unwrap(); // user, read-only, large page
                    ram.write_u32(PhysAddr(0x6101), 0x1122_3344).unwrap();
                    ram.write_u32(PhysAddr(0x7101), 0x5566_7788).unwrap();
                    let mut cpu = Cpu::new();
                    cpu.enter_protected_flat(0, 0);
                    cpu.cr0 |= CR0_PG | CR0_WP;
                    cpu.cr3 = 0x1000;
                    cpu.cr4 = if pae { CR4_PAE } else { CR4_PSE };
                    cpu.pae_pdptrs[0] = 0x2001;
                    let mut stage1 = MonitorShadowStage1Tables::<MONITOR_SHADOW_L3_TABLES>::new();
                    let mut shadow = DefaultMonitorShadowRuntime::new(&mut stage1);
                    shadow.reset(&cpu);
                    let mut mailbox = mailbox_bytes();
                    let mut client = MailboxClient::new(&mut mailbox).unwrap();
                    let mut doorbell = EchoDoorbell::expecting(MonitorRequestKindV1::PioOut, 0);
                    let mut bus = MonitorSoftBus::new_with_shadow(
                        &mut ram,
                        all_ram_regions(0x9000),
                        &mut client,
                        &mut doorbell,
                        &mut shadow,
                    );
                    for (offset, expected) in [(0x6101, 0x1122_3344), (0x7101, 0x5566_7788)] {
                        let linear = LinearAddr(base + offset);
                        assert_eq!(
                            mmu::read_linear_u32_at_cpl(
                                &mut cpu,
                                &mut bus,
                                linear,
                                AccessKind::Read,
                                3
                            )
                            .unwrap(),
                            expected
                        );
                        let entry = bus
                            .shadow
                            .as_deref_mut()
                            .unwrap()
                            .descriptor_for(ShadowWindow::User, linear)
                            .unwrap();
                        assert_eq!(
                            entry & 0x0000_ffff_ffff_f000,
                            abi::X86_RAM_IPA_BASE + u64::from(offset & !4095)
                        );
                        assert_ne!(entry & (1 << 7), 0); // still read-only
                        assert_eq!(bus.ram.read_u32(PhysAddr(pde)).unwrap() & 0x60, 0x20);
                        assert!(matches!(
                            mmu::write_linear_u32_at_cpl(&mut cpu, &mut bus, linear, 0, 3),
                            Err(mmu::Fault::Page(mmu::PageFault { code: 7, .. }))
                        ));
                        assert_eq!(bus.ram.read_u32(PhysAddr(pde)).unwrap() & 0x60, 0x20);
                    }
                    // A tracked physical PDE rewrite revokes every fragment,
                    // including the other 4 KiB page in the same large leaf.
                    mem::write_phys_u32(&mut bus, PhysAddr(pde), 0x81).unwrap();
                    cpu.invalidate_translation_context();
                    for offset in [0x6101, 0x7101] {
                        let linear = LinearAddr(base + offset);
                        assert!(
                            bus.shadow
                                .as_deref_mut()
                                .unwrap()
                                .descriptor_for(ShadowWindow::User, linear)
                                .is_none()
                        );
                        assert!(matches!(
                            mmu::read_linear_u32_at_cpl(
                                &mut cpu,
                                &mut bus,
                                linear,
                                AccessKind::Read,
                                3
                            ),
                            Err(mmu::Fault::Page(mmu::PageFault { code: 5, .. }))
                        ));
                    }
                    assert_eq!(cpu.cr2, base + 0x7101);
                    assert_eq!(
                        mmu::read_linear_u32_at_cpl(
                            &mut cpu,
                            &mut bus,
                            LinearAddr(base + 0x6101),
                            AccessKind::Read,
                            0
                        )
                        .unwrap(),
                        0x1122_3344
                    );
                    // The large leaf extends beyond RAM; that does not license
                    // a shadow mapping for the unbacked physical fragment.
                    assert_eq!(
                        mmu::read_linear_u32_at_cpl(
                            &mut cpu,
                            &mut bus,
                            LinearAddr(base + 0xa000),
                            AccessKind::Read,
                            0
                        )
                        .unwrap(),
                        u32::MAX
                    );
                    assert!(
                        bus.shadow
                            .as_deref_mut()
                            .unwrap()
                            .descriptor_for(ShadowWindow::Supervisor, LinearAddr(base + 0xa000))
                            .is_none()
                    );
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn monitor_batches_preserve_pio_and_timer_times_after_wrmsr() {
        #[derive(Default)]
        struct RecordingDoorbell(std::vec::Vec<MonitorRequestV1>);
        impl Doorbell for RecordingDoorbell {
            fn ring(&mut self, mailbox: &mut [u8]) -> Result<(), MonitorError> {
                let request = read_monitor_request_for_test(mailbox)?;
                self.0.push(request);
                write_host_response_for_test(mailbox, &HostResponseV1::ok(request.seq, 0))
            }
        }
        let mut results = vec![];
        for batched in [false, true] {
            let mut bytes = [0_u8; 4096];
            let code = [
                0xb9, 0x10, 0, 0, 0, // mov ecx, MSR_TSC
                0xba, 0, 0, 0, 0, // mov edx, 0
                0xb8, 0x00, 0x34, 0x12, 0, // mov eax, 0x123400
                0x0f, 0x30, // wrmsr
                0xba, 0xf8, 3, 0, 0, // mov edx, 0x3f8
                0x40, 0x40, 0x40, // inc eax, keeping flags lazy in a batch
                0xee, // out dx, al
                0xeb, 0xfa, // repeat inc/out
            ];
            bytes[..code.len()].copy_from_slice(&code);
            let mut ram = MonitorRam::from_slice(&mut bytes);
            let mut mailbox = mailbox_bytes();
            let mut client = MailboxClient::new(&mut mailbox).unwrap();
            let mut doorbell = RecordingDoorbell::default();
            let mut bus =
                MonitorSoftBus::new(&mut ram, all_ram_regions(4096), &mut client, &mut doorbell);
            let mut cpu = Cpu::new();
            cpu.enter_protected_flat(0, 0);
            cpu.set_tsc(0xffff_ffff_ffff_fffd);
            let steps = 12_003;
            if batched {
                assert_eq!(
                    run_x86_monitor_step_loop(&mut bus, &mut cpu, steps, 997, None)
                        .unwrap()
                        .steps,
                    steps
                );
            } else {
                for step in 0..steps {
                    assert_eq!(
                        Interpreter::step(&mut cpu, &mut bus).unwrap(),
                        StepOutcome::Running
                    );
                    maybe_deliver_external_irq(&mut cpu, &mut bus, step + 1, 997, false).unwrap();
                }
            }
            results.push((cpu_state_v1_from_cpu(&cpu, steps), doorbell.0));
        }
        assert_eq!(results[0], results[1]);
        assert!(
            results[0]
                .1
                .iter()
                .any(|request| request.kind == MonitorRequestKindV1::TimerPoll as u32)
        );
        assert!(results[0].1.iter().any(|request| request.kind
            == MonitorRequestKindV1::PioOut as u32
            && request.d > 0x123400));
    }

    #[test]
    fn serial_flush_packet_uses_three_little_endian_chunks() {
        let mut mailbox = mailbox_bytes();
        let mut client = MailboxClient::new(&mut mailbox).unwrap();
        let mut doorbell = EchoDoorbell::expecting(MonitorRequestKindV1::SerialFlush, 21);
        let packet = SerialFlushPacket::from_bytes(b"R86_KVM_PIO_SMOKE_OK\n").unwrap();

        let value = client.serial_flush(&mut doorbell, packet).unwrap();
        let request = read_monitor_request_for_test(client.mailbox).unwrap();

        assert_eq!(value, 21);
        assert_eq!(request.kind, MonitorRequestKindV1::SerialFlush as u32);
        assert_eq!(request.a, 21);
        assert_eq!(request.b.to_le_bytes(), *b"R86_KVM_");
        assert_eq!(request.c.to_le_bytes(), *b"PIO_SMOK");
        assert_eq!(&request.d.to_le_bytes()[..5], b"E_OK\n");
    }

    #[test]
    fn byte_ring_drains_serial_flush_sized_packets_without_alloc() {
        let mut ring = ByteRing::<32>::new();
        ring.extend(b"abcdefghijklmnopqrstuvwxyz").unwrap();

        let first = ring.drain_packet().unwrap();
        let second = ring.drain_packet().unwrap();

        assert_eq!(first.len(), SERIAL_FLUSH_PACKET_CAPACITY);
        assert_eq!(first.bytes(), b"abcdefghijklmnopqrstuvwx");
        assert_eq!(second.bytes(), b"yz");
        assert!(ring.is_empty());
    }

    #[test]
    fn response_errors_are_mapped_before_callers_continue() {
        let mut mailbox = mailbox_bytes();
        let seq = 7;
        write_host_response_for_test(&mut mailbox, &HostResponseV1::unsupported(seq)).unwrap();
        let response = wait_response(&mailbox, seq, 0).unwrap();

        assert_eq!(
            classify_response(response),
            Err(MonitorError::Unsupported {
                seq,
                kind: 0,
                a: 0,
                b: 0,
                c: 0,
            })
        );

        let request = MonitorRequestV1::pio_in(seq, 0x3f8, 1, 0);
        assert_eq!(
            classify_response_for_request(response, request),
            Err(MonitorError::Unsupported {
                seq,
                kind: MonitorRequestKindV1::PioIn as u32,
                a: 0x3f8,
                b: 1,
                c: 0,
            })
        );
    }

    #[test]
    fn command_poll_validates_sequence_and_size() {
        let mut mailbox = mailbox_bytes();
        let mut client = MailboxClient::new(&mut mailbox).unwrap();
        let command = HostCommandV1 {
            kind: HostCommandKindV1::Stop as u32,
            size: core::mem::size_of::<HostCommandV1>() as u32,
            seq: 3,
            flags: 0,
            a: 10,
            b: 0,
            c: 0,
            d: 0,
        };
        write_host_command_for_test(client.mailbox, 3, &command).unwrap();

        let pending = client.poll_command(0).unwrap().unwrap();

        assert_eq!(pending.seq, 3);
        assert_eq!(pending.command.kind, HostCommandKindV1::Stop as u32);
        assert_eq!(client.poll_command(3).unwrap(), None);
    }

    #[test]
    fn x86_monitor_step_loop_stops_on_host_command() {
        let mut ram_bytes = [0_u8; 0xa000];
        let mut ram = MonitorRam::from_slice(&mut ram_bytes);
        let mut mailbox = mailbox_bytes();
        let mut client = MailboxClient::new(&mut mailbox).unwrap();
        let mut doorbell = PioRecordingDoorbell::new();
        let command = HostCommandV1 {
            kind: HostCommandKindV1::Stop as u32,
            size: core::mem::size_of::<HostCommandV1>() as u32,
            seq: 1,
            flags: 0,
            a: 0,
            b: 0,
            c: 0,
            d: 0,
        };
        write_host_command_for_test(client.mailbox, 1, &command).unwrap();
        let mut cpu = cpu_from_state_v1(&CpuStateV1::protected_flat_linux_entry(
            0x0010_0000,
            0x0009_0000,
        ));
        let regions = all_ram_regions(ram.len());

        let report = run_x86_monitor_step_window(
            &mut client,
            &mut doorbell,
            &mut ram,
            regions,
            &mut cpu,
            u64::from(X86_MONITOR_SMOKE_MAX_STEPS),
            0,
        )
        .unwrap();

        assert_eq!(report.steps, 0);
        assert!(!report.halted);
        assert_eq!(report.bus.pio_out, 0);
        assert_eq!(doorbell.rings, 0);
    }

    #[test]
    fn transact_times_out_when_host_does_not_publish_response() {
        struct SilentDoorbell;
        impl Doorbell for SilentDoorbell {
            fn ring(&mut self, _mailbox: &mut [u8]) -> Result<(), MonitorError> {
                Ok(())
            }
        }

        let mut mailbox = mailbox_bytes();
        let mut client = MailboxClient::new(&mut mailbox).unwrap();
        let mut doorbell = SilentDoorbell;
        let request = MonitorRequestV1::hello(1, 0, 0);

        let err = client.transact(&mut doorbell, request, 2).unwrap_err();

        assert_eq!(err, MonitorError::ResponseTimeout { seq: 1 });
    }

    #[test]
    fn serial_packet_rejects_oversized_buffers() {
        let bytes = vec![0xaa; SERIAL_FLUSH_PACKET_CAPACITY + 1];
        assert_eq!(
            SerialFlushPacket::from_bytes(&bytes),
            Err(MonitorError::SerialPacketTooLong { len: 25 })
        );
    }
}
