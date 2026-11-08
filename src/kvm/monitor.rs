use std::fs;
use std::io::Write;
use std::path::Path;
use std::ptr;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::monitor::abi::{
    ABI_VERSION, BOOT_BLOCK_MAILBOX_OFFSET_FIELD_OFFSET, CpuStateV1, DOORBELL_IPA,
    MAILBOX_RESP_SEQ_OFFSET, MAILBOX_RESPONSE_OFFSET, MonitorRequestKindV1, MonitorRequestV1,
    RESPONSE_SEQ_OFFSET, RESPONSE_SIZE_OFFSET, RESPONSE_STATUS_OFFSET, RESPONSE_VALUE_OFFSET,
    debug::{DEBUG_ABI_VERSION, DEBUG_MAGIC, DebugStateBlockV1, PANIC_MAGIC, PanicBlockV1},
};
use crate::monitor::{
    NULL_HARDMMU_DEFAULT_STEPS, NULL_HARDMMU_ERROR_MARKER, NULL_HARDMMU_STEP_MARKER,
    RUST_MONITOR_SMOKE_MARKER, X86_MONITOR_SMOKE_MARKER,
};

use crate::kvm::backing::SlotBacking;
use crate::kvm::boot::{
    DirectBootConfig, PreparedHardMmuBoot, SHARED_CPU_STATE_OFFSET, SHARED_MAILBOX_OFFSET,
};
use crate::kvm::doorbell::DOORBELL_MAGIC;
use crate::kvm::elf::{MonitorElfLoadReport, load_monitor_elf};
use crate::kvm::error::{KvmError, Result};
use crate::kvm::kvm::{Kvm, RunMap};
use crate::kvm::memslot::{MemslotKind, MemslotPlan, PlannedMemslot};
use crate::kvm::probe::{PlannedHardMmu, ProbeConfig, choose_ipa_bits};
use crate::kvm::service::{
    MONITOR_REQUEST_KIND_SLOTS, MonitorHardMmuStats, MonitorHostRequestStats, MonitorHostService,
    MonitorStopAck,
};
use crate::kvm::snapshot::{
    HardMmuSnapshot, collect_x86_ram, load_hardmmu_snapshot, save_hardmmu_snapshot,
};
use crate::kvm::sys;

pub const HELLO_CODE_OFFSET: u64 = 0;
pub const MONITOR_STACK_TOP_GUARD: u64 = 0x10;
pub const HELLO_PROGRAM_LEN: usize = 96;
pub const PIO_SMOKE_PROGRAM_LEN: usize = 136;
pub const PIO_SMOKE_MARKER: &[u8] = b"R86_KVM_PIO_SMOKE_OK\n";
pub const MAILBOX_REQ_SEQ_OFFSET: usize = 4;
pub const MAILBOX_REQUEST_OFFSET: usize = 16;
pub const REQUEST_KIND_OFFSET: usize = 0;
pub const REQUEST_SIZE_OFFSET: usize = 4;
pub const REQUEST_SEQ_OFFSET: usize = 8;
pub const REQUEST_FLAGS_OFFSET: usize = 12;
pub const REQUEST_A_OFFSET: usize = 16;
pub const REQUEST_B_OFFSET: usize = 24;
pub const REQUEST_C_OFFSET: usize = 32;
pub const REQUEST_D_OFFSET: usize = 40;
const HELLO_RUN_TIMEOUT: Duration = Duration::from_secs(2);
const NULL_MONITOR_RUN_TIMEOUT: Duration = Duration::from_secs(30);
const TINYCORE_VGA_DECOMPRESS_MARKER: &[u8] = b"Decompressing Linux";
const TINYCORE_DECOMPRESS_MARKER_MIN_STEPS: u64 = 100_000_000;
const VGA_TEXT_BYTES: usize = 80 * 25 * 2;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TinyCoreHelloSmokeReport {
    pub api_version: i32,
    pub arm_ipa_bits_cap: i32,
    pub chosen_ipa_bits: u8,
    pub vcpu_mmap_size: i32,
    pub vcpu_target: u32,
    pub entry_ipa: u64,
    pub stack_ipa: u64,
    pub boot_block_ipa: u64,
    pub mailbox_ipa: u64,
    pub kernel_entry: u32,
    pub kernel_payload_len: usize,
    pub initrd_len: usize,
    pub exit_reason: u32,
    pub mmio_ipa: u64,
    pub mmio_len: u32,
    pub mmio_is_write: bool,
    pub mmio_value: u32,
    pub mailbox_req_seq: u32,
    pub request_kind: u32,
    pub request_size: u32,
    pub request_seq: u32,
    pub request_flags: u32,
    pub request_a: u64,
    pub request_b: u64,
    pub request_c: u64,
    pub request_d: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TinyCorePioSmokeReport {
    pub api_version: i32,
    pub arm_ipa_bits_cap: i32,
    pub chosen_ipa_bits: u8,
    pub vcpu_mmap_size: i32,
    pub vcpu_target: u32,
    pub entry_ipa: u64,
    pub stack_ipa: u64,
    pub boot_block_ipa: u64,
    pub mailbox_ipa: u64,
    pub kernel_entry: u32,
    pub kernel_payload_len: usize,
    pub initrd_len: usize,
    pub mmio_exits: u64,
    pub requests_serviced: u64,
    pub halt_seq: u32,
    pub serial_output: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TinyCoreRustMonitorSmokeReport {
    pub api_version: i32,
    pub arm_ipa_bits_cap: i32,
    pub chosen_ipa_bits: u8,
    pub vcpu_mmap_size: i32,
    pub vcpu_target: u32,
    pub entry_ipa: u64,
    pub stack_ipa: u64,
    pub boot_block_ipa: u64,
    pub mailbox_ipa: u64,
    pub kernel_entry: u32,
    pub kernel_payload_len: usize,
    pub initrd_len: usize,
    pub monitor_elf_len: usize,
    pub elf: MonitorElfLoadReport,
    pub mmio_exits: u64,
    pub requests_serviced: u64,
    pub halt_seq: u32,
    pub serial_output: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TinyCoreX86MonitorSmokeReport {
    pub api_version: i32,
    pub arm_ipa_bits_cap: i32,
    pub chosen_ipa_bits: u8,
    pub vcpu_mmap_size: i32,
    pub vcpu_target: u32,
    pub entry_ipa: u64,
    pub stack_ipa: u64,
    pub boot_block_ipa: u64,
    pub mailbox_ipa: u64,
    pub kernel_entry: u32,
    pub kernel_payload_len: usize,
    pub initrd_len: usize,
    pub monitor_elf_len: usize,
    pub elf: MonitorElfLoadReport,
    pub mmio_exits: u64,
    pub requests_serviced: u64,
    pub halt_seq: u32,
    pub serial_output: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TinyCoreNullMonitorSmokeReport {
    pub api_version: i32,
    pub arm_ipa_bits_cap: i32,
    pub chosen_ipa_bits: u8,
    pub vcpu_mmap_size: i32,
    pub vcpu_target: u32,
    pub entry_ipa: u64,
    pub stack_ipa: u64,
    pub boot_block_ipa: u64,
    pub mailbox_ipa: u64,
    pub kernel_entry: u32,
    pub kernel_payload_len: usize,
    pub initrd_len: usize,
    pub monitor_elf_len: usize,
    pub elf: MonitorElfLoadReport,
    pub step_limit: u64,
    pub timer_poll_interval: u64,
    pub elapsed_ms: u128,
    pub stop_steps_per_sec: u64,
    pub mmio_exits: u64,
    pub requests_serviced: u64,
    pub halt_seq: u32,
    pub stop_ack: Option<MonitorStopAck>,
    pub hardmmu_stats: Option<MonitorHardMmuStats>,
    pub debug_state: Option<MonitorDebugStateSnapshot>,
    pub panic_block: Option<MonitorPanicBlockSnapshot>,
    pub request_stats: MonitorHostRequestStats,
    pub serial_output: Vec<u8>,
    pub serial_success_marker_seen: bool,
    pub serial_success_stop_seq: u32,
    pub vga_decompress_marker: bool,
    pub vga_text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MonitorDebugStateSnapshot {
    pub heartbeat: u64,
    pub phase: u32,
    pub panic_flag: u32,
    pub steps: u64,
    pub vtime: u64,
    pub eip: u32,
    pub eflags: u32,
    pub cr0: u32,
    pub cr2: u32,
    pub cr3: u32,
    pub cr4: u32,
    pub cpl_halted_if: u32,
    pub last_mailbox_kind: u32,
    pub last_mailbox_seq: u64,
    pub last_pio_port_dir: u32,
    pub last_mmio_gpa: u64,
    pub stage1_installs: u64,
    pub stage1_leaf_invalidations: u64,
    pub shadow_flushes: u64,
    pub shadow_flush_by_reason: [u64; 4],
    pub shadow_translations: u64,
    pub shadow_eligible_4k: u64,
    pub shadow_read_fills: u64,
    pub shadow_write_fills: u64,
    pub shadow_upgrades: u64,
    pub shadow_unchanged_fills: u64,
    pub shadow_soft_counts: [u64; 4],
    pub shadow_page_table_write_invalidations: u64,
    pub shadow_page_walk_write_ignores: u64,
    pub shadow_errors: u64,
    pub native_reads: u64,
    pub native_read_hits: u64,
    pub native_read_aborts: u64,
    pub fetch_cache: [u64; 3],
    pub linear_cache: [u64; 3],
    pub irq_injections: u64,
    pub hlt_wakes: u64,
    pub timer_polls: u64,
    pub serial_bytes: u64,
    pub eip_histogram: [u32; 64],
}

impl MonitorDebugStateSnapshot {
    fn phase_name(self) -> &'static str {
        match self.phase {
            1 => "stage1",
            2 => "hello",
            3 => "x86-step",
            4 => "x86-halted",
            5 => "stopping",
            6 => "panic",
            _ => "unknown",
        }
    }

    fn halted(self) -> bool {
        (self.cpl_halted_if & (1 << 8)) != 0
    }

    fn top_eip_bucket(self) -> (usize, u32) {
        let mut best_index = 0;
        let mut best_value = 0;
        for (index, value) in self.eip_histogram.iter().copied().enumerate() {
            if value > best_value {
                best_index = index;
                best_value = value;
            }
        }
        (best_index, best_value)
    }

    fn delta_since(self, previous: Option<Self>) -> MonitorDebugDelta {
        if let Some(previous) = previous {
            MonitorDebugDelta {
                steps: self.steps.saturating_sub(previous.steps),
                native_reads: self.native_reads.saturating_sub(previous.native_reads),
                native_read_hits: self
                    .native_read_hits
                    .saturating_sub(previous.native_read_hits),
                stage1_installs: self
                    .stage1_installs
                    .saturating_sub(previous.stage1_installs),
                shadow_flushes: self.shadow_flushes.saturating_sub(previous.shadow_flushes),
                page_table_flushes: self.shadow_flush_by_reason[2]
                    .saturating_sub(previous.shadow_flush_by_reason[2]),
                page_table_invalidations: self
                    .shadow_page_table_write_invalidations
                    .saturating_sub(previous.shadow_page_table_write_invalidations),
                stage1_leaf_invalidations: self
                    .stage1_leaf_invalidations
                    .saturating_sub(previous.stage1_leaf_invalidations),
                page_walk_writes: self
                    .shadow_page_walk_write_ignores
                    .saturating_sub(previous.shadow_page_walk_write_ignores),
                linear_cache_fills: self.linear_cache[1].saturating_sub(previous.linear_cache[1]),
                linear_cache_invalidations: self.linear_cache[2]
                    .saturating_sub(previous.linear_cache[2]),
            }
        } else {
            MonitorDebugDelta::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct MonitorDebugDelta {
    steps: u64,
    native_reads: u64,
    native_read_hits: u64,
    stage1_installs: u64,
    shadow_flushes: u64,
    page_table_flushes: u64,
    page_table_invalidations: u64,
    stage1_leaf_invalidations: u64,
    page_walk_writes: u64,
    linear_cache_fills: u64,
    linear_cache_invalidations: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MonitorPanicBlockSnapshot {
    pub panic_code: u32,
    pub in_panic_depth: u32,
    pub esr: u64,
    pub far: u64,
    pub elr: u64,
    pub spsr: u64,
    pub sp: u64,
    pub xregs: [u64; 31],
    pub x86_eip: u32,
    pub x86_eflags: u32,
    pub x86_cr: [u32; 5],
    pub steps: u64,
    pub last_phase: u32,
    pub last_mailbox_kind: u32,
    pub trace_write_index: u64,
    pub breadcrumb: [u64; 3],
}

#[derive(Clone, Copy)]
struct MonitorDebugReader {
    debug_ptr: usize,
    panic_ptr: usize,
}

impl MonitorDebugReader {
    fn from_backing(
        plan: &MemslotPlan,
        backing: &[SlotBacking],
        boot_block: crate::monitor::abi::BootBlockV1,
    ) -> Result<Self> {
        let shared = find_slot(plan, MemslotKind::Shared)?;
        let shared_backing = backing
            .iter()
            .find(|backing| backing.slot == shared.slot)
            .ok_or(KvmError::MemslotPlan("shared backing not found"))?;
        let debug_offset = usize::try_from(boot_block.debug_state_offset())
            .map_err(|_| KvmError::MemslotPlan("debug state offset too large"))?;
        let panic_offset = usize::try_from(boot_block.panic_block_offset())
            .map_err(|_| KvmError::MemslotPlan("panic block offset too large"))?;
        let debug_end = debug_offset
            .checked_add(core::mem::size_of::<DebugStateBlockV1>())
            .ok_or(KvmError::MemslotPlan("debug state end overflows"))?;
        let panic_end = panic_offset
            .checked_add(core::mem::size_of::<PanicBlockV1>())
            .ok_or(KvmError::MemslotPlan("panic block end overflows"))?;
        if debug_end > shared_backing.memory.len() || panic_end > shared_backing.memory.len() {
            return Err(KvmError::MemslotPlan("debug pages exceed shared backing"));
        }
        let base = shared_backing.memory.as_ptr() as usize;
        Ok(Self {
            debug_ptr: base + debug_offset,
            panic_ptr: base + panic_offset,
        })
    }

    fn read_debug_state(self) -> Option<MonitorDebugStateSnapshot> {
        let ptr = self.debug_ptr as *const DebugStateBlockV1;
        for _ in 0..8 {
            let seq_before = unsafe { ptr::read_volatile(ptr::addr_of!((*ptr).seq)) };
            if seq_before & 1 != 0 {
                thread::yield_now();
                continue;
            }
            std::sync::atomic::fence(Ordering::Acquire);
            let magic = unsafe { ptr::read_volatile(ptr::addr_of!((*ptr).magic)) };
            let version = unsafe { ptr::read_volatile(ptr::addr_of!((*ptr).version)) };
            let size = unsafe { ptr::read_volatile(ptr::addr_of!((*ptr).size)) };
            if magic != DEBUG_MAGIC
                || version != DEBUG_ABI_VERSION
                || size as usize != core::mem::size_of::<DebugStateBlockV1>()
            {
                return None;
            }
            let snapshot = unsafe {
                MonitorDebugStateSnapshot {
                    heartbeat: ptr::read_volatile(ptr::addr_of!((*ptr).heartbeat)),
                    phase: ptr::read_volatile(ptr::addr_of!((*ptr).phase)),
                    panic_flag: ptr::read_volatile(ptr::addr_of!((*ptr).panic_flag)),
                    steps: ptr::read_volatile(ptr::addr_of!((*ptr).steps)),
                    vtime: ptr::read_volatile(ptr::addr_of!((*ptr).vtime)),
                    eip: ptr::read_volatile(ptr::addr_of!((*ptr).eip)),
                    eflags: ptr::read_volatile(ptr::addr_of!((*ptr).eflags)),
                    cr0: ptr::read_volatile(ptr::addr_of!((*ptr).cr0)),
                    cr2: ptr::read_volatile(ptr::addr_of!((*ptr).cr2)),
                    cr3: ptr::read_volatile(ptr::addr_of!((*ptr).cr3)),
                    cr4: ptr::read_volatile(ptr::addr_of!((*ptr).cr4)),
                    cpl_halted_if: ptr::read_volatile(ptr::addr_of!((*ptr).cpl_halted_if)),
                    last_mailbox_kind: ptr::read_volatile(ptr::addr_of!((*ptr).last_mailbox_kind)),
                    last_mailbox_seq: ptr::read_volatile(ptr::addr_of!((*ptr).last_mailbox_seq)),
                    last_pio_port_dir: ptr::read_volatile(ptr::addr_of!((*ptr).last_pio_port_dir)),
                    last_mmio_gpa: ptr::read_volatile(ptr::addr_of!((*ptr).last_mmio_gpa)),
                    stage1_installs: ptr::read_volatile(ptr::addr_of!((*ptr).stage1_installs)),
                    stage1_leaf_invalidations: ptr::read_volatile(ptr::addr_of!(
                        (*ptr).stage1_leaf_invalidations
                    )),
                    shadow_flushes: ptr::read_volatile(ptr::addr_of!((*ptr).shadow_flushes)),
                    shadow_flush_by_reason: ptr::read_volatile(ptr::addr_of!(
                        (*ptr).shadow_flush_by_reason
                    )),
                    shadow_translations: ptr::read_volatile(ptr::addr_of!(
                        (*ptr).shadow_translations
                    )),
                    shadow_eligible_4k: ptr::read_volatile(ptr::addr_of!(
                        (*ptr).shadow_eligible_4k
                    )),
                    shadow_read_fills: ptr::read_volatile(ptr::addr_of!((*ptr).shadow_read_fills)),
                    shadow_write_fills: ptr::read_volatile(ptr::addr_of!(
                        (*ptr).shadow_write_fills
                    )),
                    shadow_upgrades: ptr::read_volatile(ptr::addr_of!((*ptr).shadow_upgrades)),
                    shadow_unchanged_fills: ptr::read_volatile(ptr::addr_of!(
                        (*ptr).shadow_unchanged_fills
                    )),
                    shadow_soft_counts: ptr::read_volatile(ptr::addr_of!(
                        (*ptr).shadow_soft_counts
                    )),
                    shadow_page_table_write_invalidations: ptr::read_volatile(ptr::addr_of!(
                        (*ptr).shadow_page_table_write_invalidations
                    )),
                    shadow_page_walk_write_ignores: ptr::read_volatile(ptr::addr_of!(
                        (*ptr).shadow_page_walk_write_ignores
                    )),
                    shadow_errors: ptr::read_volatile(ptr::addr_of!((*ptr).shadow_errors)),
                    native_reads: ptr::read_volatile(ptr::addr_of!((*ptr).native_reads)),
                    native_read_hits: ptr::read_volatile(ptr::addr_of!((*ptr).native_read_hits)),
                    native_read_aborts: ptr::read_volatile(ptr::addr_of!(
                        (*ptr).native_read_aborts
                    )),
                    fetch_cache: ptr::read_volatile(ptr::addr_of!((*ptr).fetch_cache)),
                    linear_cache: ptr::read_volatile(ptr::addr_of!((*ptr).linear_cache)),
                    irq_injections: ptr::read_volatile(ptr::addr_of!((*ptr).irq_injections)),
                    hlt_wakes: ptr::read_volatile(ptr::addr_of!((*ptr).hlt_wakes)),
                    timer_polls: ptr::read_volatile(ptr::addr_of!((*ptr).timer_polls)),
                    serial_bytes: ptr::read_volatile(ptr::addr_of!((*ptr).serial_bytes)),
                    eip_histogram: ptr::read_volatile(ptr::addr_of!((*ptr).eip_histogram)),
                }
            };
            std::sync::atomic::fence(Ordering::Acquire);
            let seq_after = unsafe { ptr::read_volatile(ptr::addr_of!((*ptr).seq)) };
            if seq_before == seq_after && seq_after & 1 == 0 && snapshot.heartbeat != 0 {
                return Some(snapshot);
            }
        }
        None
    }

    fn read_panic_block(self) -> Option<MonitorPanicBlockSnapshot> {
        let ptr = self.panic_ptr as *const PanicBlockV1;
        let magic = unsafe { ptr::read_volatile(ptr::addr_of!((*ptr).magic)) };
        let version = unsafe { ptr::read_volatile(ptr::addr_of!((*ptr).version)) };
        let size = unsafe { ptr::read_volatile(ptr::addr_of!((*ptr).size)) };
        if magic != PANIC_MAGIC
            || version != DEBUG_ABI_VERSION
            || size as usize != core::mem::size_of::<PanicBlockV1>()
        {
            return None;
        }
        Some(unsafe {
            MonitorPanicBlockSnapshot {
                panic_code: ptr::read_volatile(ptr::addr_of!((*ptr).panic_code)),
                in_panic_depth: ptr::read_volatile(ptr::addr_of!((*ptr).in_panic_depth)),
                esr: ptr::read_volatile(ptr::addr_of!((*ptr).esr)),
                far: ptr::read_volatile(ptr::addr_of!((*ptr).far)),
                elr: ptr::read_volatile(ptr::addr_of!((*ptr).elr)),
                spsr: ptr::read_volatile(ptr::addr_of!((*ptr).spsr)),
                sp: ptr::read_volatile(ptr::addr_of!((*ptr).sp)),
                xregs: ptr::read_volatile(ptr::addr_of!((*ptr).xregs)),
                x86_eip: ptr::read_volatile(ptr::addr_of!((*ptr).x86_eip)),
                x86_eflags: ptr::read_volatile(ptr::addr_of!((*ptr).x86_eflags)),
                x86_cr: ptr::read_volatile(ptr::addr_of!((*ptr).x86_cr)),
                steps: ptr::read_volatile(ptr::addr_of!((*ptr).steps)),
                last_phase: ptr::read_volatile(ptr::addr_of!((*ptr).last_phase)),
                last_mailbox_kind: ptr::read_volatile(ptr::addr_of!((*ptr).last_mailbox_kind)),
                trace_write_index: ptr::read_volatile(ptr::addr_of!((*ptr).trace_write_index)),
                breadcrumb: ptr::read_volatile(ptr::addr_of!((*ptr).breadcrumb)),
            }
        })
    }
}

struct MonitorRunObserver {
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    reason: Arc<Mutex<Option<String>>>,
    handle: Option<JoinHandle<()>>,
}

impl MonitorRunObserver {
    fn start(reader: MonitorDebugReader, boot: &DirectBootConfig) -> Option<Self> {
        if !boot.progress
            && boot.timeout_secs.is_none()
            && boot.stall_timeout_secs.is_none()
            && !boot.json_progress
        {
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let reason = Arc::new(Mutex::new(None));
        let progress = boot.progress || boot.json_progress;
        let json = boot.json_progress;
        let timeout = boot.timeout_secs.map(Duration::from_secs);
        let stall_timeout = boot.stall_timeout_secs.map(Duration::from_secs);
        let observer_stop = Arc::clone(&stop);
        let observer_cancel = Arc::clone(&cancel);
        let observer_reason = Arc::clone(&reason);
        let handle = thread::spawn(move || {
            let started = Instant::now();
            let mut last_snapshot = None;
            let mut last_heartbeat_at = started;
            while !observer_stop.load(Ordering::Acquire) {
                thread::park_timeout(Duration::from_secs(1));
                if observer_stop.load(Ordering::Acquire) {
                    break;
                }
                let now = Instant::now();
                let snapshot = reader.read_debug_state();
                if let Some(snapshot) = snapshot {
                    let delta = snapshot.delta_since(last_snapshot);
                    let (top_eip_bucket, top_eip_samples) = snapshot.top_eip_bucket();
                    if last_snapshot
                        .map(|prev: MonitorDebugStateSnapshot| prev.heartbeat != snapshot.heartbeat)
                        .unwrap_or(true)
                    {
                        last_heartbeat_at = now;
                    }
                    if progress {
                        if json {
                            eprintln!(
                                "{{\"event\":\"r86_progress\",\"heartbeat\":{},\"phase\":{},\"steps\":{},\"delta_steps\":{},\"eip\":{},\"vtime\":{},\"halted\":{},\"native_reads\":{},\"delta_native_reads\":{},\"native_read_hits\":{},\"delta_native_read_hits\":{},\"stage1_installs\":{},\"delta_stage1_installs\":{},\"shadow_flushes\":{},\"delta_shadow_flushes\":{},\"page_table_flushes\":{},\"delta_page_table_flushes\":{},\"page_table_invalidations\":{},\"delta_page_table_invalidations\":{},\"stage1_leaf_invalidations\":{},\"delta_stage1_leaf_invalidations\":{},\"page_walk_writes\":{},\"delta_page_walk_writes\":{},\"linear_cache_fills\":{},\"delta_linear_cache_fills\":{},\"linear_cache_invalidations\":{},\"delta_linear_cache_invalidations\":{},\"shadow_errors\":{},\"last_mailbox_kind\":{},\"last_mailbox_seq\":{},\"top_eip_bucket\":{},\"top_eip_samples\":{}}}",
                                snapshot.heartbeat,
                                snapshot.phase,
                                snapshot.steps,
                                delta.steps,
                                snapshot.eip,
                                snapshot.vtime,
                                snapshot.halted(),
                                snapshot.native_reads,
                                delta.native_reads,
                                snapshot.native_read_hits,
                                delta.native_read_hits,
                                snapshot.stage1_installs,
                                delta.stage1_installs,
                                snapshot.shadow_flushes,
                                delta.shadow_flushes,
                                snapshot.shadow_flush_by_reason[2],
                                delta.page_table_flushes,
                                snapshot.shadow_page_table_write_invalidations,
                                delta.page_table_invalidations,
                                snapshot.stage1_leaf_invalidations,
                                delta.stage1_leaf_invalidations,
                                snapshot.shadow_page_walk_write_ignores,
                                delta.page_walk_writes,
                                snapshot.linear_cache[1],
                                delta.linear_cache_fills,
                                snapshot.linear_cache[2],
                                delta.linear_cache_invalidations,
                                snapshot.shadow_errors,
                                snapshot.last_mailbox_kind,
                                snapshot.last_mailbox_seq,
                                top_eip_bucket,
                                top_eip_samples
                            );
                        } else {
                            eprintln!(
                                "r86-progress heartbeat={} phase={} steps={} +{}/s eip=0x{:08x} vtime={} halted={} native_reads={}/{}(+{}/{}) stage1={} (+{}) flush={} (+{} pt_flush={} +{} pt_inv={} +{} leaf_inv={} +{} ad={}) lcache={}/{}/{} (+{}/+{}) shadow_errors={} mailbox={}:{} eip_bucket={}:{}",
                                snapshot.heartbeat,
                                snapshot.phase_name(),
                                snapshot.steps,
                                delta.steps,
                                snapshot.eip,
                                snapshot.vtime,
                                snapshot.halted(),
                                snapshot.native_reads,
                                snapshot.native_read_hits,
                                delta.native_reads,
                                delta.native_read_hits,
                                snapshot.stage1_installs,
                                delta.stage1_installs,
                                snapshot.shadow_flushes,
                                delta.shadow_flushes,
                                snapshot.shadow_flush_by_reason[2],
                                delta.page_table_flushes,
                                snapshot.shadow_page_table_write_invalidations,
                                delta.page_table_invalidations,
                                snapshot.stage1_leaf_invalidations,
                                delta.stage1_leaf_invalidations,
                                snapshot.shadow_page_walk_write_ignores,
                                snapshot.linear_cache[0],
                                snapshot.linear_cache[1],
                                snapshot.linear_cache[2],
                                delta.linear_cache_fills,
                                delta.linear_cache_invalidations,
                                snapshot.shadow_errors,
                                snapshot.last_mailbox_kind,
                                snapshot.last_mailbox_seq,
                                top_eip_bucket,
                                top_eip_samples
                            );
                        }
                    }
                    if snapshot.panic_flag != 0 {
                        set_observer_cancel(
                            &observer_cancel,
                            &observer_reason,
                            format!("monitor panic_flag={}", snapshot.panic_flag),
                        );
                    }
                    last_snapshot = Some(snapshot);
                } else if progress && json {
                    eprintln!("{{\"event\":\"r86_progress\",\"state\":\"pre_mmu_or_untrusted\"}}");
                } else if progress {
                    eprintln!("r86-progress state=pre-mmu-or-untrusted");
                }

                if let Some(timeout) = timeout {
                    if now.duration_since(started) >= timeout {
                        set_observer_cancel(
                            &observer_cancel,
                            &observer_reason,
                            format!("timeout after {}s", timeout.as_secs()),
                        );
                    }
                }
                if let Some(stall_timeout) = stall_timeout {
                    if now.duration_since(last_heartbeat_at) >= stall_timeout {
                        set_observer_cancel(
                            &observer_cancel,
                            &observer_reason,
                            format!("stalled heartbeat for {}s", stall_timeout.as_secs()),
                        );
                    }
                }
            }
        });
        Some(Self {
            stop,
            cancel,
            reason,
            handle: Some(handle),
        })
    }

    fn cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    fn cancel_reason(&self) -> Option<String> {
        self.reason
            .lock()
            .ok()
            .and_then(|reason| reason.as_ref().cloned())
    }

    fn finish(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

impl Drop for MonitorRunObserver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

fn set_observer_cancel(cancel: &AtomicBool, reason: &Mutex<Option<String>>, message: String) {
    cancel.store(true, Ordering::Release);
    if let Ok(mut guard) = reason.lock() {
        if guard.is_none() {
            *guard = Some(message);
        }
    }
}

pub fn run_tinycore_hello_smoke(
    probe: &ProbeConfig,
    boot: &DirectBootConfig,
) -> Result<TinyCoreHelloSmokeReport> {
    let prepared = crate::kvm::boot::prepare_tinycore_direct_boot(probe, boot)?;
    run_prepared_hello_smoke(probe, prepared)
}

pub fn run_tinycore_pio_smoke(
    probe: &ProbeConfig,
    boot: &DirectBootConfig,
) -> Result<TinyCorePioSmokeReport> {
    let prepared = crate::kvm::boot::prepare_tinycore_direct_boot(probe, boot)?;
    run_prepared_pio_smoke(probe, prepared)
}

pub fn run_tinycore_rust_monitor_smoke(
    probe: &ProbeConfig,
    boot: &DirectBootConfig,
    monitor_elf: &Path,
) -> Result<TinyCoreRustMonitorSmokeReport> {
    let prepared = crate::kvm::boot::prepare_tinycore_direct_boot(probe, boot)?;
    run_prepared_rust_monitor_smoke(probe, prepared, monitor_elf)
}

pub fn run_tinycore_x86_monitor_smoke(
    probe: &ProbeConfig,
    boot: &DirectBootConfig,
    monitor_elf: &Path,
) -> Result<TinyCoreX86MonitorSmokeReport> {
    let prepared = crate::kvm::boot::prepare_tinycore_direct_boot(probe, boot)?;
    run_prepared_x86_monitor_smoke(probe, prepared, monitor_elf)
}

pub fn run_tinycore_null_monitor_smoke(
    probe: &ProbeConfig,
    boot: &DirectBootConfig,
    monitor_elf: &Path,
) -> Result<TinyCoreNullMonitorSmokeReport> {
    if probe.machine_monitor && (boot.snapshot_in.is_some() || boot.snapshot_out.is_some()) {
        return Err(KvmError::BadArgument(
            "shared Machine monitor snapshots require its actual EL1 device state; legacy host-device snapshots are unsupported".into(),
        ));
    }
    let restore_snapshot = if let Some(path) = boot.snapshot_in.as_deref() {
        Some(load_hardmmu_snapshot(path)?)
    } else {
        None
    };
    let prepared = if let Some(snapshot) = restore_snapshot.as_ref() {
        crate::kvm::boot::prepare_snapshot_restore_boot(probe, snapshot, boot)?
    } else {
        crate::kvm::boot::prepare_tinycore_direct_boot(probe, boot)?
    };
    run_prepared_null_monitor_smoke(
        probe,
        prepared,
        monitor_elf,
        boot,
        restore_snapshot.map(|snapshot| snapshot.device),
    )
}

pub fn run_prepared_hello_smoke(
    probe: &ProbeConfig,
    prepared: PreparedHardMmuBoot,
) -> Result<TinyCoreHelloSmokeReport> {
    let PreparedHardMmuBoot {
        planned,
        mut backing,
        loaded,
        boot_block_ipa,
        mailbox_ipa,
        ..
    } = prepared;

    let kvm = Kvm::open()?;
    let api_version = kvm.require_api_version()?;
    let arm_ipa_bits_cap =
        kvm.require_extension(sys::KVM_CAP_ARM_VM_IPA_SIZE, "KVM_CAP_ARM_VM_IPA_SIZE")?;
    let chosen_ipa_bits = choose_ipa_bits(
        planned.required_ipa_bits,
        arm_ipa_bits_cap,
        probe.requested_ipa_bits,
    )?;
    planned.layout.validate(chosen_ipa_bits)?;
    let vcpu_mmap_size = usize::try_from(kvm.vcpu_mmap_size()?)
        .map_err(|_| KvmError::UnsupportedHost("negative vcpu mmap size"))?;

    let entry_ipa = load_hello_program(&planned, &mut backing)?;
    let stack_ipa = monitor_stack_ipa(&planned)?;
    let vm = kvm.create_arm_vm(chosen_ipa_bits)?;
    let mut vcpu_init = vm.arm_preferred_target()?;
    vcpu_init.features[0] &= !(1_u32 << sys::KVM_ARM_VCPU_POWER_OFF);
    let registered = vm.register_backing(&planned.memslots, backing)?;
    let vcpu = vm.create_vcpu(0)?;
    vcpu.arm_init(&vcpu_init)?;
    vcpu.set_arm_entry_state_with_arg0(entry_ipa, stack_ipa, boot_block_ipa)?;
    let run_map = vcpu.map_run(vcpu_mmap_size)?;
    if !vcpu.run_with_timeout(&run_map, HELLO_RUN_TIMEOUT)? {
        let pc = vcpu.get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PC))?;
        let pstate = vcpu.get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PSTATE))?;
        return Err(KvmError::BadArgument(format!(
            "KVM_RUN timed out before monitor HELLO doorbell pc=0x{pc:x} pstate=0x{pstate:x}"
        )));
    }

    let mmio = run_map
        .mmio()
        .ok_or_else(|| unexpected_exit_error(&run_map))?;
    let mmio_value = u32::from_le_bytes(mmio.data[..4].try_into().unwrap());
    if mmio.phys_addr != DOORBELL_IPA
        || mmio.len != 4
        || mmio.is_write == 0
        || mmio_value != DOORBELL_MAGIC
    {
        return Err(KvmError::BadArgument(format!(
            "unexpected monitor HELLO doorbell ipa=0x{:x} len={} write={} value=0x{:08x}",
            mmio.phys_addr, mmio.len, mmio.is_write, mmio_value
        )));
    }

    let mailbox = read_mailbox_request(&planned.memslots, registered.backing())?;
    validate_hello_request(&mailbox)?;
    Ok(TinyCoreHelloSmokeReport {
        api_version,
        arm_ipa_bits_cap,
        chosen_ipa_bits,
        vcpu_mmap_size: i32::try_from(vcpu_mmap_size).unwrap_or(i32::MAX),
        vcpu_target: vcpu_init.target,
        entry_ipa,
        stack_ipa,
        boot_block_ipa,
        mailbox_ipa,
        kernel_entry: loaded.entry,
        kernel_payload_len: loaded.kernel_payload_len,
        initrd_len: loaded.initrd_len,
        exit_reason: sys::KVM_EXIT_MMIO,
        mmio_ipa: mmio.phys_addr,
        mmio_len: mmio.len,
        mmio_is_write: mmio.is_write != 0,
        mmio_value,
        mailbox_req_seq: mailbox.req_seq,
        request_kind: mailbox.request.kind,
        request_size: mailbox.request.size,
        request_seq: mailbox.request.seq,
        request_flags: mailbox.request.flags,
        request_a: mailbox.request.a,
        request_b: mailbox.request.b,
        request_c: mailbox.request.c,
        request_d: mailbox.request.d,
    })
}

pub fn run_prepared_pio_smoke(
    probe: &ProbeConfig,
    prepared: PreparedHardMmuBoot,
) -> Result<TinyCorePioSmokeReport> {
    let PreparedHardMmuBoot {
        planned,
        mut backing,
        loaded,
        boot_block_ipa,
        mailbox_ipa,
        ..
    } = prepared;

    let kvm = Kvm::open()?;
    let api_version = kvm.require_api_version()?;
    let arm_ipa_bits_cap =
        kvm.require_extension(sys::KVM_CAP_ARM_VM_IPA_SIZE, "KVM_CAP_ARM_VM_IPA_SIZE")?;
    let chosen_ipa_bits = choose_ipa_bits(
        planned.required_ipa_bits,
        arm_ipa_bits_cap,
        probe.requested_ipa_bits,
    )?;
    planned.layout.validate(chosen_ipa_bits)?;
    let vcpu_mmap_size = usize::try_from(kvm.vcpu_mmap_size()?)
        .map_err(|_| KvmError::UnsupportedHost("negative vcpu mmap size"))?;

    let entry_ipa = load_pio_smoke_program(&planned, &mut backing)?;
    let stack_ipa = monitor_stack_ipa(&planned)?;
    let vm = kvm.create_arm_vm(chosen_ipa_bits)?;
    let mut vcpu_init = vm.arm_preferred_target()?;
    vcpu_init.features[0] &= !(1_u32 << sys::KVM_ARM_VCPU_POWER_OFF);
    let mut registered = vm.register_backing(&planned.memslots, backing)?;
    let vcpu = vm.create_vcpu(0)?;
    vcpu.arm_init(&vcpu_init)?;
    vcpu.set_arm_entry_state_with_arg0(entry_ipa, stack_ipa, boot_block_ipa)?;
    let run_map = vcpu.map_run(vcpu_mmap_size)?;
    let mut service = MonitorHostService::new();
    let mut mmio_exits = 0_u64;
    let max_mmio_exits = 2;

    while !service.halted() && !contains_bytes(service.serial_output(), PIO_SMOKE_MARKER) {
        if mmio_exits >= max_mmio_exits {
            return Err(KvmError::BadArgument(format!(
                "monitor PIO smoke exceeded {max_mmio_exits} doorbell exits without marker serial={:?} requests={}",
                String::from_utf8_lossy(service.serial_output()),
                service.requests_serviced()
            )));
        }
        if !vcpu.run_with_timeout(&run_map, HELLO_RUN_TIMEOUT)? {
            let pc = vcpu.get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PC))?;
            let pstate =
                vcpu.get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PSTATE))?;
            return Err(KvmError::BadArgument(format!(
                "KVM_RUN timed out during monitor PIO smoke pc=0x{pc:x} pstate=0x{pstate:x} serial={:?}",
                String::from_utf8_lossy(service.serial_output())
            )));
        }

        let mmio = run_map
            .mmio()
            .ok_or_else(|| unexpected_exit_error(&run_map))?;
        let mmio_value = u32::from_le_bytes(mmio.data[..4].try_into().unwrap());
        if mmio.phys_addr != DOORBELL_IPA
            || mmio.len != 4
            || mmio.is_write == 0
            || mmio_value != DOORBELL_MAGIC
        {
            return Err(KvmError::BadArgument(format!(
                "unexpected monitor PIO doorbell ipa=0x{:x} len={} write={} value=0x{:08x}",
                mmio.phys_addr, mmio.len, mmio.is_write, mmio_value
            )));
        }

        mmio_exits = mmio_exits.wrapping_add(1);
        service.service_doorbell(&planned.memslots, registered.backing_mut())?;
        if let Some(panic) = service.panic() {
            return Err(KvmError::BadArgument(format!(
                "monitor panic {panic:?} serial={:?}",
                String::from_utf8_lossy(service.serial_output())
            )));
        }

        if service.halted() && !contains_bytes(service.serial_output(), NULL_HARDMMU_STEP_MARKER) {
            return Err(KvmError::BadArgument(format!(
                "null monitor halted without success marker error_marker={} serial={:?} requests={} vga={:?}",
                contains_bytes(service.serial_output(), NULL_HARDMMU_ERROR_MARKER),
                String::from_utf8_lossy(service.serial_output()),
                service.requests_serviced(),
                vga_text_snapshot(service.vga_text_bytes())
            )));
        }
    }

    if !contains_bytes(service.serial_output(), PIO_SMOKE_MARKER) {
        return Err(KvmError::BadArgument(format!(
            "monitor PIO smoke did not emit marker {:?}; serial={:?}",
            String::from_utf8_lossy(PIO_SMOKE_MARKER),
            String::from_utf8_lossy(service.serial_output())
        )));
    }

    Ok(TinyCorePioSmokeReport {
        api_version,
        arm_ipa_bits_cap,
        chosen_ipa_bits,
        vcpu_mmap_size: i32::try_from(vcpu_mmap_size).unwrap_or(i32::MAX),
        vcpu_target: vcpu_init.target,
        entry_ipa,
        stack_ipa,
        boot_block_ipa,
        mailbox_ipa,
        kernel_entry: loaded.entry,
        kernel_payload_len: loaded.kernel_payload_len,
        initrd_len: loaded.initrd_len,
        mmio_exits,
        requests_serviced: service.requests_serviced(),
        halt_seq: service.halt_seq().unwrap_or(0),
        serial_output: service.serial_output().to_vec(),
    })
}

pub fn run_prepared_rust_monitor_smoke(
    probe: &ProbeConfig,
    prepared: PreparedHardMmuBoot,
    monitor_elf: &Path,
) -> Result<TinyCoreRustMonitorSmokeReport> {
    let PreparedHardMmuBoot {
        planned,
        mut backing,
        loaded,
        boot_block_ipa,
        mailbox_ipa,
        ..
    } = prepared;

    let monitor_elf_bytes = fs::read(monitor_elf).map_err(|source| KvmError::Io {
        op: "read monitor ELF",
        source,
    })?;
    let elf = load_monitor_elf(&planned, &mut backing, &monitor_elf_bytes)?;

    let kvm = Kvm::open()?;
    let api_version = kvm.require_api_version()?;
    let arm_ipa_bits_cap =
        kvm.require_extension(sys::KVM_CAP_ARM_VM_IPA_SIZE, "KVM_CAP_ARM_VM_IPA_SIZE")?;
    let chosen_ipa_bits = choose_ipa_bits(
        planned.required_ipa_bits,
        arm_ipa_bits_cap,
        probe.requested_ipa_bits,
    )?;
    planned.layout.validate(chosen_ipa_bits)?;
    let vcpu_mmap_size = usize::try_from(kvm.vcpu_mmap_size()?)
        .map_err(|_| KvmError::UnsupportedHost("negative vcpu mmap size"))?;

    let stack_ipa = monitor_stack_ipa(&planned)?;
    let vm = kvm.create_arm_vm(chosen_ipa_bits)?;
    let mut vcpu_init = vm.arm_preferred_target()?;
    vcpu_init.features[0] &= !(1_u32 << sys::KVM_ARM_VCPU_POWER_OFF);
    let mut registered = vm.register_backing(&planned.memslots, backing)?;
    let vcpu = vm.create_vcpu(0)?;
    vcpu.arm_init(&vcpu_init)?;
    vcpu.set_arm_entry_state_with_arg0(elf.entry_ipa, stack_ipa, boot_block_ipa)?;
    let run_map = vcpu.map_run(vcpu_mmap_size)?;
    let mut service = MonitorHostService::new();
    let mut mmio_exits = 0_u64;
    let max_mmio_exits = 8;

    while !service.halted() || !contains_bytes(service.serial_output(), RUST_MONITOR_SMOKE_MARKER) {
        if mmio_exits >= max_mmio_exits {
            return Err(KvmError::BadArgument(format!(
                "rust monitor smoke exceeded {max_mmio_exits} doorbell exits without halt+marker serial={:?} requests={}",
                String::from_utf8_lossy(service.serial_output()),
                service.requests_serviced()
            )));
        }
        if !vcpu.run_with_timeout(&run_map, HELLO_RUN_TIMEOUT)? {
            let pc = vcpu.get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PC))?;
            let pstate =
                vcpu.get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PSTATE))?;
            return Err(KvmError::BadArgument(format!(
                "KVM_RUN timed out during rust monitor smoke pc=0x{pc:x} pstate=0x{pstate:x} serial={:?} requests={} mailbox={}",
                String::from_utf8_lossy(service.serial_output()),
                service.requests_serviced(),
                mailbox_debug(&planned.memslots, registered.backing())
            )));
        }

        let mmio = run_map
            .mmio()
            .ok_or_else(|| unexpected_exit_error(&run_map))?;
        let mmio_value = u32::from_le_bytes(mmio.data[..4].try_into().unwrap());
        if mmio.phys_addr != DOORBELL_IPA
            || mmio.len != 4
            || mmio.is_write == 0
            || mmio_value != DOORBELL_MAGIC
        {
            return Err(KvmError::BadArgument(format!(
                "unexpected rust monitor doorbell ipa=0x{:x} len={} write={} value=0x{:08x}",
                mmio.phys_addr, mmio.len, mmio.is_write, mmio_value
            )));
        }

        mmio_exits = mmio_exits.wrapping_add(1);
        service.service_doorbell(&planned.memslots, registered.backing_mut())?;
        if let Some(panic) = service.panic() {
            return Err(KvmError::BadArgument(format!(
                "monitor panic {panic:?} serial={:?}",
                String::from_utf8_lossy(service.serial_output())
            )));
        }
    }

    Ok(TinyCoreRustMonitorSmokeReport {
        api_version,
        arm_ipa_bits_cap,
        chosen_ipa_bits,
        vcpu_mmap_size: i32::try_from(vcpu_mmap_size).unwrap_or(i32::MAX),
        vcpu_target: vcpu_init.target,
        entry_ipa: elf.entry_ipa,
        stack_ipa,
        boot_block_ipa,
        mailbox_ipa,
        kernel_entry: loaded.entry,
        kernel_payload_len: loaded.kernel_payload_len,
        initrd_len: loaded.initrd_len,
        monitor_elf_len: monitor_elf_bytes.len(),
        elf,
        mmio_exits,
        requests_serviced: service.requests_serviced(),
        halt_seq: service.halt_seq().unwrap_or(0),
        serial_output: service.serial_output().to_vec(),
    })
}

pub fn run_prepared_x86_monitor_smoke(
    probe: &ProbeConfig,
    prepared: PreparedHardMmuBoot,
    monitor_elf: &Path,
) -> Result<TinyCoreX86MonitorSmokeReport> {
    let PreparedHardMmuBoot {
        planned,
        mut backing,
        loaded,
        boot_block_ipa,
        mailbox_ipa,
        ..
    } = prepared;

    let monitor_elf_bytes = fs::read(monitor_elf).map_err(|source| KvmError::Io {
        op: "read monitor ELF",
        source,
    })?;
    let elf = load_monitor_elf(&planned, &mut backing, &monitor_elf_bytes)?;

    let kvm = Kvm::open()?;
    let api_version = kvm.require_api_version()?;
    let arm_ipa_bits_cap =
        kvm.require_extension(sys::KVM_CAP_ARM_VM_IPA_SIZE, "KVM_CAP_ARM_VM_IPA_SIZE")?;
    let chosen_ipa_bits = choose_ipa_bits(
        planned.required_ipa_bits,
        arm_ipa_bits_cap,
        probe.requested_ipa_bits,
    )?;
    planned.layout.validate(chosen_ipa_bits)?;
    let vcpu_mmap_size = usize::try_from(kvm.vcpu_mmap_size()?)
        .map_err(|_| KvmError::UnsupportedHost("negative vcpu mmap size"))?;

    let stack_ipa = monitor_stack_ipa(&planned)?;
    let vm = kvm.create_arm_vm(chosen_ipa_bits)?;
    let mut vcpu_init = vm.arm_preferred_target()?;
    vcpu_init.features[0] &= !(1_u32 << sys::KVM_ARM_VCPU_POWER_OFF);
    let mut registered = vm.register_backing(&planned.memslots, backing)?;
    let vcpu = vm.create_vcpu(0)?;
    vcpu.arm_init(&vcpu_init)?;
    vcpu.set_arm_entry_state_with_arg0(elf.entry_ipa, stack_ipa, boot_block_ipa)?;
    let run_map = vcpu.map_run(vcpu_mmap_size)?;
    let mut service = MonitorHostService::new();
    let mut mmio_exits = 0_u64;
    let max_mmio_exits = 64;

    while !service.halted() || !contains_bytes(service.serial_output(), X86_MONITOR_SMOKE_MARKER) {
        if mmio_exits >= max_mmio_exits {
            return Err(KvmError::BadArgument(format!(
                "x86 monitor smoke exceeded {max_mmio_exits} doorbell exits without halt+marker serial={:?} requests={}",
                String::from_utf8_lossy(service.serial_output()),
                service.requests_serviced()
            )));
        }
        if !vcpu.run_with_timeout(&run_map, HELLO_RUN_TIMEOUT)? {
            let pc = vcpu.get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PC))?;
            let pstate =
                vcpu.get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PSTATE))?;
            return Err(KvmError::BadArgument(format!(
                "KVM_RUN timed out during x86 monitor smoke pc=0x{pc:x} pstate=0x{pstate:x} serial={:?} requests={} mailbox={}",
                String::from_utf8_lossy(service.serial_output()),
                service.requests_serviced(),
                mailbox_debug(&planned.memslots, registered.backing())
            )));
        }

        let mmio = run_map
            .mmio()
            .ok_or_else(|| unexpected_exit_error(&run_map))?;
        let mmio_value = u32::from_le_bytes(mmio.data[..4].try_into().unwrap());
        if mmio.phys_addr != DOORBELL_IPA
            || mmio.len != 4
            || mmio.is_write == 0
            || mmio_value != DOORBELL_MAGIC
        {
            return Err(KvmError::BadArgument(format!(
                "unexpected x86 monitor doorbell ipa=0x{:x} len={} write={} value=0x{:08x}",
                mmio.phys_addr, mmio.len, mmio.is_write, mmio_value
            )));
        }

        mmio_exits = mmio_exits.wrapping_add(1);
        service.service_doorbell(&planned.memslots, registered.backing_mut())?;
        if let Some(panic) = service.panic() {
            return Err(KvmError::BadArgument(format!(
                "monitor panic {panic:?} serial={:?}",
                String::from_utf8_lossy(service.serial_output())
            )));
        }
    }

    Ok(TinyCoreX86MonitorSmokeReport {
        api_version,
        arm_ipa_bits_cap,
        chosen_ipa_bits,
        vcpu_mmap_size: i32::try_from(vcpu_mmap_size).unwrap_or(i32::MAX),
        vcpu_target: vcpu_init.target,
        entry_ipa: elf.entry_ipa,
        stack_ipa,
        boot_block_ipa,
        mailbox_ipa,
        kernel_entry: loaded.entry,
        kernel_payload_len: loaded.kernel_payload_len,
        initrd_len: loaded.initrd_len,
        monitor_elf_len: monitor_elf_bytes.len(),
        elf,
        mmio_exits,
        requests_serviced: service.requests_serviced(),
        halt_seq: service.halt_seq().unwrap_or(0),
        serial_output: service.serial_output().to_vec(),
    })
}

pub fn run_prepared_null_monitor_smoke(
    probe: &ProbeConfig,
    prepared: PreparedHardMmuBoot,
    monitor_elf: &Path,
    boot_config: &DirectBootConfig,
    restore_device: Option<crate::kvm::service::PcPioBusSnapshot>,
) -> Result<TinyCoreNullMonitorSmokeReport> {
    let PreparedHardMmuBoot {
        planned,
        mut backing,
        loaded,
        reboot_assets,
        boot_block,
        boot_block_ipa,
        mailbox_ipa,
        ..
    } = prepared;

    let monitor_elf_bytes = fs::read(monitor_elf).map_err(|source| KvmError::Io {
        op: "read monitor ELF",
        source,
    })?;
    let elf = load_monitor_elf(&planned, &mut backing, &monitor_elf_bytes)?;

    let kvm = Kvm::open()?;
    let api_version = kvm.require_api_version()?;
    let arm_ipa_bits_cap =
        kvm.require_extension(sys::KVM_CAP_ARM_VM_IPA_SIZE, "KVM_CAP_ARM_VM_IPA_SIZE")?;
    let chosen_ipa_bits = choose_ipa_bits(
        planned.required_ipa_bits,
        arm_ipa_bits_cap,
        probe.requested_ipa_bits,
    )?;
    planned.layout.validate(chosen_ipa_bits)?;
    let vcpu_mmap_size = usize::try_from(kvm.vcpu_mmap_size()?)
        .map_err(|_| KvmError::UnsupportedHost("negative vcpu mmap size"))?;

    let stack_ipa = monitor_stack_ipa(&planned)?;
    let vm = kvm.create_arm_vm(chosen_ipa_bits)?;
    let mut vcpu_init = vm.arm_preferred_target()?;
    vcpu_init.features[0] &= !(1_u32 << sys::KVM_ARM_VCPU_POWER_OFF);
    let mut registered = vm.register_backing(&planned.memslots, backing)?;
    let debug_reader =
        MonitorDebugReader::from_backing(&planned.memslots, registered.backing(), boot_block)?;
    let observer = MonitorRunObserver::start(debug_reader, boot_config);
    let observer_needs_control_exits = boot_config.timeout_secs.is_some()
        || boot_config.stall_timeout_secs.is_some()
        || boot_config.stop_at_step.is_some();
    let kvm_run_timeout = if observer_needs_control_exits {
        Duration::from_secs(1)
    } else {
        NULL_MONITOR_RUN_TIMEOUT
    };
    let vcpu = vm.create_vcpu(0)?;
    vcpu.arm_init(&vcpu_init)?;
    vcpu.set_arm_entry_state_with_arg0(elf.entry_ipa, stack_ipa, boot_block_ipa)?;
    let run_map = vcpu.map_run(vcpu_mmap_size)?;
    let mut service = if let Some(snapshot) = restore_device {
        MonitorHostService::from_pio_snapshot(snapshot)?
    } else {
        MonitorHostService::new()
    };
    if reboot_assets.is_some() {
        service.enable_machine_boot_reload();
    }
    let mut mmio_exits = 0_u64;
    let effective_step_limit = if boot_block.step_limit == 0 {
        NULL_HARDMMU_DEFAULT_STEPS
    } else {
        boot_block.step_limit
    };
    let max_mmio_exits = 100_000_u64
        .max(effective_step_limit / 100)
        .saturating_add(100_000);
    let serial_success_marker = boot_config
        .serial_success_marker
        .as_deref()
        .map(str::as_bytes);
    let stop_on_serial_marker = boot_config
        .stop_on_serial_marker
        .as_deref()
        .map(str::as_bytes)
        .or(serial_success_marker);
    let mut serial_success_marker_seen = false;
    let mut serial_success_stop_seq = 0_u32;
    let mut serial_stop_marker_seen = false;
    let mut serial_forwarded = 0;
    let mut stop_at_step_sent = false;
    let started_at = Instant::now();

    while !service.halted()
        || (!contains_bytes(service.serial_output(), NULL_HARDMMU_STEP_MARKER)
            && !contains_bytes(service.serial_output(), NULL_HARDMMU_ERROR_MARKER))
    {
        if let Some(observer) = observer.as_ref() {
            if observer.cancel_requested() {
                let _ = service.publish_stop_command(&planned.memslots, registered.backing_mut());
                return Err(KvmError::BadArgument(format!(
                    "null monitor observer stopped run: {} debug={:?} panic={:?} serial={:?} stop_ack={:?} request_stats={:?}",
                    observer
                        .cancel_reason()
                        .unwrap_or_else(|| "cancelled".to_string()),
                    debug_reader.read_debug_state(),
                    debug_reader.read_panic_block(),
                    String::from_utf8_lossy(service.serial_output()),
                    service.stop_ack(),
                    service.request_stats()
                )));
            }
        }
        if let Some(stop_at_step) = boot_config.stop_at_step {
            if !stop_at_step_sent {
                if let Some(snapshot) = debug_reader.read_debug_state() {
                    if snapshot.steps >= stop_at_step {
                        serial_success_stop_seq = service
                            .publish_stop_command(&planned.memslots, registered.backing_mut())?;
                        stop_at_step_sent = true;
                    }
                }
            }
        }
        if mmio_exits >= max_mmio_exits {
            return Err(KvmError::BadArgument(format!(
                "null monitor smoke exceeded {max_mmio_exits} doorbell exits without halt+marker serial={:?} requests={} request_stats={:?} stop_ack={:?} vga={:?}",
                String::from_utf8_lossy(service.serial_output()),
                service.requests_serviced(),
                service.request_stats(),
                service.stop_ack(),
                vga_text_snapshot(service.vga_text_bytes())
            )));
        }
        if !vcpu.run_with_timeout(&run_map, kvm_run_timeout)? {
            if observer_needs_control_exits {
                if let Some(observer) = observer.as_ref() {
                    if observer.cancel_requested() {
                        let _ = service
                            .publish_stop_command(&planned.memslots, registered.backing_mut());
                        return Err(KvmError::BadArgument(format!(
                            "null monitor observer stopped run: {} debug={:?} panic={:?} serial={:?} stop_ack={:?} request_stats={:?}",
                            observer
                                .cancel_reason()
                                .unwrap_or_else(|| "cancelled".to_string()),
                            debug_reader.read_debug_state(),
                            debug_reader.read_panic_block(),
                            String::from_utf8_lossy(service.serial_output()),
                            service.stop_ack(),
                            service.request_stats()
                        )));
                    }
                }
                continue;
            }
            let pc = vcpu.get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PC))?;
            let pstate =
                vcpu.get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PSTATE))?;
            return Err(KvmError::BadArgument(format!(
                "KVM_RUN timed out during null monitor smoke pc=0x{pc:x} pstate=0x{pstate:x} serial={:?} requests={} request_stats={:?} stop_ack={:?} debug={:?} mailbox={} vga={:?}",
                String::from_utf8_lossy(service.serial_output()),
                service.requests_serviced(),
                service.request_stats(),
                service.stop_ack(),
                debug_reader.read_debug_state(),
                mailbox_debug(&planned.memslots, registered.backing()),
                vga_text_snapshot(service.vga_text_bytes())
            )));
        }

        let mmio = run_map
            .mmio()
            .ok_or_else(|| unexpected_exit_error(&run_map))?;
        let mmio_value = u32::from_le_bytes(mmio.data[..4].try_into().unwrap());
        if mmio.phys_addr != DOORBELL_IPA
            || mmio.len != 4
            || mmio.is_write == 0
            || mmio_value != DOORBELL_MAGIC
        {
            return Err(KvmError::BadArgument(format!(
                "unexpected null monitor doorbell ipa=0x{:x} len={} write={} value=0x{:08x}",
                mmio.phys_addr, mmio.len, mmio.is_write, mmio_value
            )));
        }

        mmio_exits = mmio_exits.wrapping_add(1);
        let outcome = service.service_doorbell(&planned.memslots, registered.backing_mut())?;
        if boot_config.serial_stdout {
            let serial = service.serial_output();
            if serial.len() != serial_forwarded {
                let mut out = std::io::stdout().lock();
                out.write_all(&serial[serial_forwarded..])
                    .and_then(|()| out.flush())
                    .map_err(|source| KvmError::Io {
                        op: "forward monitor serial",
                        source,
                    })?;
                serial_forwarded = serial.len();
            }
        }
        if outcome.kind == MonitorRequestKindV1::ReloadBoot as u32
            && outcome.status == crate::monitor::abi::ResponseStatusV1::Ok as u32
        {
            let assets = reboot_assets
                .as_ref()
                .expect("reload enabled with original assets");
            crate::kvm::boot::reload_machine_boot(
                &planned,
                registered.backing_mut(),
                assets,
                boot_config,
            )?;
        }
        if !serial_stop_marker_seen {
            if let Some(marker) = stop_on_serial_marker {
                if contains_bytes(service.serial_output(), marker) {
                    serial_stop_marker_seen = true;
                    if serial_success_marker
                        .map(|success| contains_bytes(service.serial_output(), success))
                        .unwrap_or(false)
                    {
                        serial_success_marker_seen = true;
                    }
                    serial_success_stop_seq = service
                        .publish_stop_command(&planned.memslots, registered.backing_mut())?;
                }
            }
        } else if !serial_success_marker_seen {
            if let Some(marker) = serial_success_marker {
                serial_success_marker_seen = contains_bytes(service.serial_output(), marker);
            }
        }
        if let Some(panic) = service.panic() {
            return Err(KvmError::BadArgument(format!(
                "monitor panic {panic:?} serial={:?}",
                String::from_utf8_lossy(service.serial_output())
            )));
        }
    }

    let vga_text = vga_text_snapshot(service.vga_text_bytes());
    let debug_state = debug_reader.read_debug_state();
    let panic_block = debug_reader.read_panic_block();
    if let Some(observer) = observer {
        observer.finish();
    }
    let elapsed = started_at.elapsed();
    let elapsed_ms = elapsed.as_millis();
    let stop_steps_per_sec = steps_per_sec(service.stop_ack(), elapsed);
    let vga_decompress_marker = vga_text
        .as_bytes()
        .windows(TINYCORE_VGA_DECOMPRESS_MARKER.len())
        .any(|window| window == TINYCORE_VGA_DECOMPRESS_MARKER);
    if contains_bytes(service.serial_output(), NULL_HARDMMU_ERROR_MARKER) {
        return Err(KvmError::BadArgument(format!(
            "null monitor smoke reached error marker serial={:?} request_stats={:?} stop_ack={:?} stats={:?} vga={vga_text:?}",
            String::from_utf8_lossy(service.serial_output()),
            service.request_stats(),
            service.stop_ack(),
            service.hardmmu_stats()
        )));
    }
    if serial_success_marker.is_some() && !serial_success_marker_seen {
        return Err(KvmError::BadArgument(format!(
            "null monitor smoke stopped before serial success marker; serial={:?} stop_ack={:?} vga={vga_text:?}",
            String::from_utf8_lossy(service.serial_output()),
            service.stop_ack()
        )));
    }
    if boot_config.stop_on_serial_marker.is_some() && !serial_stop_marker_seen {
        return Err(KvmError::BadArgument(format!(
            "null monitor smoke stopped before stop-on-serial marker; serial={:?} stop_ack={:?} vga={vga_text:?}",
            String::from_utf8_lossy(service.serial_output()),
            service.stop_ack()
        )));
    }
    if let Some(snapshot_path) = boot_config.snapshot_out.as_deref() {
        let cpu_state = read_cpu_state_from_shared(&planned.memslots, registered.backing())?;
        let ram = collect_x86_ram(
            &planned.memslots,
            registered.backing(),
            planned.layout.ram_size,
        )?;
        save_hardmmu_snapshot(
            snapshot_path,
            &HardMmuSnapshot {
                ram_size: planned.layout.ram_size,
                cpu_state,
                device: service.pio_snapshot(),
                ram,
            },
        )?;
    }
    if let Some(path) = boot_config.machine_state_out.as_deref() {
        let cpu = read_cpu_state_from_shared(&planned.memslots, registered.backing())?;
        let ram = collect_x86_ram(
            &planned.memslots,
            registered.backing(),
            planned.layout.ram_size,
        )?;
        let trace = find_slot(&planned.memslots, MemslotKind::Trace)?;
        let bytes = registered
            .backing()
            .iter()
            .find(|b| b.slot == trace.slot)
            .ok_or(KvmError::MemslotPlan("trace backing not found"))?
            .memory
            .as_slice();
        let bytes = bytes
            .get(boot_block.trace_offset as usize..)
            .ok_or(KvmError::MemslotPlan("machine state offset out of range"))?;
        let header = bytes
            .get(..16)
            .ok_or(KvmError::MemslotPlan("machine state header missing"))?;
        let magic = u64::from_le_bytes(header[..8].try_into().unwrap());
        let len = usize::try_from(u64::from_le_bytes(header[8..16].try_into().unwrap()))
            .map_err(|_| KvmError::MemslotPlan("machine state length out of range"))?;
        if magic != crate::monitor::abi::MACHINE_STATE_MAGIC {
            return Err(KvmError::MemslotPlan("machine state was not exported"));
        }
        let device = bytes
            .get(16..)
            .and_then(|bytes| bytes.get(..len))
            .ok_or(KvmError::MemslotPlan("machine state exceeds trace backing"))?;
        for (suffix, data) in [
            ("cpu", format!("{cpu:?}\n").as_bytes()),
            ("ram", ram.as_slice()),
            ("devices", device),
        ] {
            fs::write(path.with_extension(suffix), data).map_err(|source| KvmError::Io {
                op: "write machine comparison state",
                source,
            })?;
        }
    }
    if let Some(path) = boot_config.frame_out.as_deref() {
        use crate::monitor::firmware::{FRAME_MAGIC, FRAME_OFFSET, INPUT_OFFSET};
        let trace = find_slot(&planned.memslots, MemslotKind::Trace)?;
        let bytes = registered
            .backing()
            .iter()
            .find(|b| b.slot == trace.slot)
            .ok_or(KvmError::MemslotPlan("trace backing not found"))?
            .memory
            .as_slice();
        let frame = bytes
            .get(FRAME_OFFSET..INPUT_OFFSET)
            .ok_or(KvmError::MemslotPlan("frame area missing"))?;
        let field = |i: usize| u64::from_le_bytes(frame[i * 8..(i + 1) * 8].try_into().unwrap());
        let (width, height, len) = (field(1), field(2), field(3));
        if field(0) != FRAME_MAGIC
            || width == 0
            || height == 0
            || width.checked_mul(height).and_then(|n| n.checked_mul(4)) != Some(len)
        {
            return Err(KvmError::MemslotPlan("invalid monitor frame"));
        }
        let rgba = frame
            .get(32..)
            .and_then(|bytes| bytes.get(..usize::try_from(len).ok()?))
            .ok_or(KvmError::MemslotPlan("frame exceeds output area"))?;
        let mut out =
            std::io::BufWriter::new(fs::File::create(path).map_err(|source| KvmError::Io {
                op: "create monitor frame",
                source,
            })?);
        let mut write_frame = || -> std::io::Result<()> {
            write!(out, "P6\n{width} {height}\n255\n")?;
            for pixel in rgba.chunks_exact(4) {
                out.write_all(&pixel[..3])?;
            }
            out.flush()
        };
        write_frame().map_err(|source| KvmError::Io {
            op: "write monitor frame",
            source,
        })?;
    }
    if !probe.machine_monitor
        && serial_success_marker.is_none()
        && effective_step_limit >= TINYCORE_DECOMPRESS_MARKER_MIN_STEPS
        && !vga_decompress_marker
    {
        let vga_nonzero = vga_nonzero_summary(service.vga_text_bytes());
        return Err(KvmError::BadArgument(format!(
            "null monitor smoke reached marker but TinyCore VGA decompress marker was missing; vga={vga_text:?} vga_nonzero={vga_nonzero} serial={:?} stop_ack={:?}",
            String::from_utf8_lossy(service.serial_output()),
            service.stop_ack()
        )));
    }

    Ok(TinyCoreNullMonitorSmokeReport {
        api_version,
        arm_ipa_bits_cap,
        chosen_ipa_bits,
        vcpu_mmap_size: i32::try_from(vcpu_mmap_size).unwrap_or(i32::MAX),
        vcpu_target: vcpu_init.target,
        entry_ipa: elf.entry_ipa,
        stack_ipa,
        boot_block_ipa,
        mailbox_ipa,
        kernel_entry: loaded.entry,
        kernel_payload_len: loaded.kernel_payload_len,
        initrd_len: loaded.initrd_len,
        monitor_elf_len: monitor_elf_bytes.len(),
        elf,
        step_limit: effective_step_limit,
        timer_poll_interval: boot_block.timer_poll_interval,
        elapsed_ms,
        stop_steps_per_sec,
        mmio_exits,
        requests_serviced: service.requests_serviced(),
        halt_seq: service.halt_seq().unwrap_or(0),
        stop_ack: service.stop_ack(),
        hardmmu_stats: service.hardmmu_stats(),
        debug_state,
        panic_block,
        request_stats: service.request_stats(),
        serial_output: service.serial_output().to_vec(),
        serial_success_marker_seen,
        serial_success_stop_seq,
        vga_decompress_marker,
        vga_text,
    })
}

pub fn hello_program_bytes() -> [u8; HELLO_PROGRAM_LEN] {
    assert_eq!(BOOT_BLOCK_MAILBOX_OFFSET_FIELD_OFFSET, 496);
    let mut out = [0_u8; HELLO_PROGRAM_LEN];
    write_u32(&mut out, 0x00, 0xf940_f814);
    write_u32(&mut out, 0x04, 0x8b00_0294);
    write_u32(&mut out, 0x08, 0x5280_0021);
    write_u32(&mut out, 0x0c, 0x5280_0022);
    write_u32(&mut out, 0x10, 0xb900_1282);
    write_u32(&mut out, 0x14, 0x5280_0602);
    write_u32(&mut out, 0x18, 0xb900_1682);
    write_u32(&mut out, 0x1c, 0xb900_1a81);
    write_u32(&mut out, 0x20, 0xb900_1e9f);
    write_u32(&mut out, 0x24, 0x5280_0022);
    write_u32(&mut out, 0x28, 0xf900_1282);
    write_u32(&mut out, 0x2c, 0xf900_169f);
    write_u32(&mut out, 0x30, 0xf900_1a9f);
    write_u32(&mut out, 0x34, 0xf900_1e9f);
    write_u32(&mut out, 0x38, 0xb900_0681);
    write_u32(&mut out, 0x3c, 0xd503_3abf);
    write_u32(&mut out, 0x40, 0x5800_00c3);
    write_u32(&mut out, 0x44, 0x5287_06c2);
    write_u32(&mut out, 0x48, 0x72aa_4862);
    write_u32(&mut out, 0x4c, 0xb900_0062);
    write_u32(&mut out, 0x50, 0xd503_3f9f);
    write_u32(&mut out, 0x54, 0x1400_0000);
    out[0x58..0x60].copy_from_slice(&DOORBELL_IPA.to_le_bytes());
    out
}

pub fn pio_smoke_program_bytes() -> [u8; PIO_SMOKE_PROGRAM_LEN] {
    assert_eq!(BOOT_BLOCK_MAILBOX_OFFSET_FIELD_OFFSET, 496);
    assert_eq!(PIO_SMOKE_MARKER.len(), 21);
    let mut out = [0_u8; PIO_SMOKE_PROGRAM_LEN];
    write_u32(&mut out, 0x00, 0xf940_f814);
    write_u32(&mut out, 0x04, 0x8b00_0294);
    write_u32(&mut out, 0x08, 0x9100_4295);
    write_u32(&mut out, 0x0c, 0x5800_02f8);
    write_u32(&mut out, 0x10, 0x5280_0033);
    write_u32(&mut out, 0x14, 0x5280_0122);
    write_u32(&mut out, 0x18, 0xb900_02a2);
    write_u32(&mut out, 0x1c, 0x5280_0602);
    write_u32(&mut out, 0x20, 0xb900_06a2);
    write_u32(&mut out, 0x24, 0xb900_0ab3);
    write_u32(&mut out, 0x28, 0xb900_0ebf);
    write_u32(&mut out, 0x2c, 0xd280_02a2);
    write_u32(&mut out, 0x30, 0xf900_0aa2);
    write_u32(&mut out, 0x34, 0x5800_01e2);
    write_u32(&mut out, 0x38, 0xf900_0ea2);
    write_u32(&mut out, 0x3c, 0x5800_01e2);
    write_u32(&mut out, 0x40, 0xf900_12a2);
    write_u32(&mut out, 0x44, 0x5800_01e2);
    write_u32(&mut out, 0x48, 0xf900_16a2);
    write_u32(&mut out, 0x4c, 0xb900_0693);
    write_u32(&mut out, 0x50, 0xd50b_7a34);
    write_u32(&mut out, 0x54, 0xd503_3b9f);
    write_u32(&mut out, 0x58, 0x5287_06c2);
    write_u32(&mut out, 0x5c, 0x72aa_4862);
    write_u32(&mut out, 0x60, 0xb900_0302);
    write_u32(&mut out, 0x64, 0x1400_0000);
    out[0x68..0x70].copy_from_slice(&DOORBELL_IPA.to_le_bytes());
    out[0x70..0x78].copy_from_slice(&PIO_SMOKE_MARKER[0..8]);
    out[0x78..0x80].copy_from_slice(&PIO_SMOKE_MARKER[8..16]);
    out[0x80..0x85].copy_from_slice(&PIO_SMOKE_MARKER[16..21]);
    out
}

fn load_hello_program(planned: &PlannedHardMmu, backing: &mut [SlotBacking]) -> Result<u64> {
    let monitor = find_slot(&planned.memslots, MemslotKind::Monitor)?;
    let entry_ipa = monitor.guest_phys_addr + HELLO_CODE_OFFSET;
    let slot_backing = backing
        .iter_mut()
        .find(|backing| backing.slot == monitor.slot)
        .ok_or(KvmError::MemslotPlan("monitor backing not found"))?;
    let program = hello_program_bytes();
    slot_backing.write_at(HELLO_CODE_OFFSET, &program)?;
    slot_backing.flush_instruction_cache_at(HELLO_CODE_OFFSET, program.len())?;
    Ok(entry_ipa)
}

fn load_pio_smoke_program(planned: &PlannedHardMmu, backing: &mut [SlotBacking]) -> Result<u64> {
    let monitor = find_slot(&planned.memslots, MemslotKind::Monitor)?;
    let entry_ipa = monitor.guest_phys_addr + HELLO_CODE_OFFSET;
    let slot_backing = backing
        .iter_mut()
        .find(|backing| backing.slot == monitor.slot)
        .ok_or(KvmError::MemslotPlan("monitor backing not found"))?;
    let program = pio_smoke_program_bytes();
    slot_backing.write_at(HELLO_CODE_OFFSET, &program)?;
    slot_backing.flush_instruction_cache_at(HELLO_CODE_OFFSET, program.len())?;
    Ok(entry_ipa)
}

fn monitor_stack_ipa(planned: &PlannedHardMmu) -> Result<u64> {
    let monitor = find_slot(&planned.memslots, MemslotKind::Monitor)?;
    if monitor.memory_size <= MONITOR_STACK_TOP_GUARD {
        return Err(KvmError::MemslotPlan(
            "monitor slot too small for hello stack",
        ));
    }
    Ok((monitor.guest_phys_addr + monitor.memory_size - MONITOR_STACK_TOP_GUARD) & !0xf)
}

fn read_mailbox_request(
    plan: &MemslotPlan,
    backing: &[SlotBacking],
) -> Result<MailboxRequestSnapshot> {
    let shared = find_slot(plan, MemslotKind::Shared)?;
    let shared_backing = backing
        .iter()
        .find(|backing| backing.slot == shared.slot)
        .ok_or(KvmError::MemslotPlan("shared backing not found"))?;
    let mailbox_start = usize::try_from(SHARED_MAILBOX_OFFSET)
        .map_err(|_| KvmError::MemslotPlan("mailbox offset too large"))?;
    let mailbox = shared_backing
        .memory
        .as_slice()
        .get(mailbox_start..)
        .ok_or(KvmError::MemslotPlan(
            "mailbox offset exceeds shared backing",
        ))?;
    let request = mailbox
        .get(MAILBOX_REQUEST_OFFSET..)
        .ok_or(KvmError::MemslotPlan("mailbox request exceeds backing"))?;
    Ok(MailboxRequestSnapshot {
        req_seq: read_u32(mailbox, MAILBOX_REQ_SEQ_OFFSET)?,
        request: MonitorRequestV1 {
            kind: read_u32(request, REQUEST_KIND_OFFSET)?,
            size: read_u32(request, REQUEST_SIZE_OFFSET)?,
            seq: read_u32(request, REQUEST_SEQ_OFFSET)?,
            flags: read_u32(request, REQUEST_FLAGS_OFFSET)?,
            a: read_u64(request, REQUEST_A_OFFSET)?,
            b: read_u64(request, REQUEST_B_OFFSET)?,
            c: read_u64(request, REQUEST_C_OFFSET)?,
            d: read_u64(request, REQUEST_D_OFFSET)?,
        },
    })
}

fn mailbox_debug(plan: &MemslotPlan, backing: &[SlotBacking]) -> String {
    let Ok(shared) = find_slot(plan, MemslotKind::Shared) else {
        return "shared_slot=missing".to_string();
    };
    let Some(shared_backing) = backing.iter().find(|backing| backing.slot == shared.slot) else {
        return "shared_backing=missing".to_string();
    };
    let Ok(mailbox_start) = usize::try_from(SHARED_MAILBOX_OFFSET) else {
        return "mailbox_offset=too_large".to_string();
    };
    let Some(mailbox) = shared_backing.memory.as_slice().get(mailbox_start..) else {
        return "mailbox=out_of_range".to_string();
    };
    let request = read_mailbox_request_from_debug_bytes(mailbox);
    let resp_seq = read_u32(mailbox, MAILBOX_RESP_SEQ_OFFSET).unwrap_or(u32::MAX);
    let response = mailbox
        .get(MAILBOX_RESPONSE_OFFSET..)
        .map(|response| {
            (
                read_u32(response, RESPONSE_STATUS_OFFSET).unwrap_or(u32::MAX),
                read_u32(response, RESPONSE_SIZE_OFFSET).unwrap_or(u32::MAX),
                read_u32(response, RESPONSE_SEQ_OFFSET).unwrap_or(u32::MAX),
                read_u64(response, RESPONSE_VALUE_OFFSET).unwrap_or(u64::MAX),
            )
        })
        .unwrap_or((u32::MAX, u32::MAX, u32::MAX, u64::MAX));
    format!(
        "req_seq={} req_kind={} req_seq_field={} resp_seq={} resp_status={} resp_size={} resp_seq_field={} resp_value=0x{:x}",
        request.0, request.1, request.2, resp_seq, response.0, response.1, response.2, response.3
    )
}

fn read_cpu_state_from_shared(plan: &MemslotPlan, backing: &[SlotBacking]) -> Result<CpuStateV1> {
    let shared = find_slot(plan, MemslotKind::Shared)?;
    let shared_backing = backing
        .iter()
        .find(|candidate| candidate.slot == shared.slot)
        .ok_or(KvmError::MemslotPlan("shared backing not found"))?;
    let offset = usize::try_from(SHARED_CPU_STATE_OFFSET)
        .map_err(|_| KvmError::MemslotPlan("CPU state offset too large"))?;
    let end = offset
        .checked_add(core::mem::size_of::<CpuStateV1>())
        .ok_or(KvmError::MemslotPlan("CPU state end overflows"))?;
    if end > shared_backing.memory.len() {
        return Err(KvmError::MemslotPlan("CPU state exceeds shared backing"));
    }
    let ptr = unsafe { shared_backing.memory.as_slice().as_ptr().add(offset) as *const CpuStateV1 };
    Ok(unsafe { ptr::read_volatile(ptr) })
}

fn read_mailbox_request_from_debug_bytes(mailbox: &[u8]) -> (u32, u32, u32) {
    let req_seq = read_u32(mailbox, MAILBOX_REQ_SEQ_OFFSET).unwrap_or(u32::MAX);
    let request = mailbox.get(MAILBOX_REQUEST_OFFSET..).unwrap_or(&[]);
    let kind = read_u32(request, REQUEST_KIND_OFFSET).unwrap_or(u32::MAX);
    let seq = read_u32(request, REQUEST_SEQ_OFFSET).unwrap_or(u32::MAX);
    (req_seq, kind, seq)
}

fn validate_hello_request(mailbox: &MailboxRequestSnapshot) -> Result<()> {
    if mailbox.req_seq != 1
        || mailbox.request.kind != MonitorRequestKindV1::Hello as u32
        || mailbox.request.size != core::mem::size_of::<MonitorRequestV1>() as u32
        || mailbox.request.seq != 1
        || mailbox.request.flags != 0
        || mailbox.request.a != ABI_VERSION as u64
        || mailbox.request.b != 0
        || mailbox.request.c != 0
        || mailbox.request.d != 0
    {
        return Err(KvmError::BadArgument(format!(
            "bad monitor HELLO mailbox req_seq={} kind={} size={} seq={} flags={} a=0x{:x} b=0x{:x} c=0x{:x} d=0x{:x}",
            mailbox.req_seq,
            mailbox.request.kind,
            mailbox.request.size,
            mailbox.request.seq,
            mailbox.request.flags,
            mailbox.request.a,
            mailbox.request.b,
            mailbox.request.c,
            mailbox.request.d
        )));
    }
    Ok(())
}

fn find_slot(plan: &MemslotPlan, kind: MemslotKind) -> Result<&PlannedMemslot> {
    plan.slots()
        .iter()
        .find(|slot| slot.kind == kind)
        .ok_or(KvmError::MemslotPlan("required memslot kind missing"))
}

fn unexpected_exit_error(run_map: &RunMap) -> KvmError {
    if let Some(fail) = run_map.fail_entry() {
        return KvmError::BadArgument(format!(
            "KVM_EXIT_FAIL_ENTRY reason=0x{:x}",
            fail.hardware_entry_failure_reason
        ));
    }
    if let Some(internal) = run_map.internal_error() {
        return KvmError::BadArgument(format!(
            "KVM_EXIT_INTERNAL_ERROR suberror={} ndata={}",
            internal.suberror, internal.ndata
        ));
    }
    if let Some(system) = run_map.system_event() {
        return KvmError::BadArgument(format!(
            "KVM_EXIT_SYSTEM_EVENT type={} ndata={}",
            system.event_type, system.ndata
        ));
    }
    KvmError::BadArgument(format!(
        "unexpected KVM exit reason {}",
        run_map.exit_reason()
    ))
}

fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let data = bytes
        .get(offset..offset + 4)
        .ok_or(KvmError::MemslotPlan("mailbox u32 read exceeds backing"))?;
    Ok(u32::from_le_bytes(data.try_into().unwrap()))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let data = bytes
        .get(offset..offset + 8)
        .ok_or(KvmError::MemslotPlan("mailbox u64 read exceeds backing"))?;
    Ok(u64::from_le_bytes(data.try_into().unwrap()))
}

fn vga_text_snapshot(bytes: &[u8]) -> String {
    let mut out = String::new();
    let Some(text) = bytes.get(..core::cmp::min(bytes.len(), VGA_TEXT_BYTES)) else {
        return out;
    };
    let rows = core::cmp::min(text.len() / (80 * 2), 25);
    for row in 0..rows {
        let mut line = String::new();
        for col in 0..80 {
            let index = (row * 80 + col) * 2;
            let ch = text[index];
            let ch = if ch.is_ascii_graphic() || ch == b' ' {
                ch as char
            } else {
                ' '
            };
            line.push(ch);
        }
        let line = line.trim_end();
        if !line.is_empty() {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn vga_nonzero_summary(bytes: &[u8]) -> String {
    let mut count = 0_usize;
    let mut first = None;
    let mut last = 0_usize;
    let mut samples = String::new();
    for (index, byte) in bytes.iter().copied().enumerate() {
        if byte == 0 {
            continue;
        }
        count += 1;
        if first.is_none() {
            first = Some(index);
        }
        last = index;
        if count <= 8 {
            samples.push_str(&format!(" {index:#x}:{byte:#04x}"));
        }
    }
    match first {
        Some(first) => format!("count={count} first=0x{first:x} last=0x{last:x} samples={samples}"),
        None => "count=0".to_string(),
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn steps_per_sec(stop_ack: Option<MonitorStopAck>, elapsed: Duration) -> u64 {
    let Some(stop) = stop_ack else {
        return 0;
    };
    let nanos = elapsed.as_nanos();
    if nanos == 0 {
        return 0;
    }
    let steps = u128::from(stop.steps);
    steps
        .saturating_mul(1_000_000_000)
        .checked_div(nanos)
        .unwrap_or(0)
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MailboxRequestSnapshot {
    req_seq: u32,
    request: MonitorRequestV1,
}

impl core::fmt::Display for TinyCoreHelloSmokeReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "r86-kvm.tinycore_hello_smoke=ok")?;
        writeln!(f, "kvm.api_version={}", self.api_version)?;
        writeln!(f, "kvm.arm_ipa_bits_cap={}", self.arm_ipa_bits_cap)?;
        writeln!(f, "kvm.chosen_ipa_bits={}", self.chosen_ipa_bits)?;
        writeln!(f, "kvm.vcpu_mmap_size={}", self.vcpu_mmap_size)?;
        writeln!(f, "vcpu.target={}", self.vcpu_target)?;
        writeln!(f, "vcpu.entry_ipa=0x{:x}", self.entry_ipa)?;
        writeln!(f, "vcpu.stack_ipa=0x{:x}", self.stack_ipa)?;
        writeln!(f, "boot.block_ipa=0x{:x}", self.boot_block_ipa)?;
        writeln!(f, "boot.mailbox_ipa=0x{:x}", self.mailbox_ipa)?;
        writeln!(f, "boot.kernel_entry=0x{:x}", self.kernel_entry)?;
        writeln!(f, "boot.kernel_payload_len={}", self.kernel_payload_len)?;
        writeln!(f, "boot.initrd_len={}", self.initrd_len)?;
        writeln!(f, "exit.reason={}", self.exit_reason)?;
        writeln!(f, "mmio.ipa=0x{:x}", self.mmio_ipa)?;
        writeln!(f, "mmio.len={}", self.mmio_len)?;
        writeln!(f, "mmio.write={}", self.mmio_is_write)?;
        writeln!(f, "mmio.value=0x{:08x}", self.mmio_value)?;
        writeln!(f, "mailbox.req_seq={}", self.mailbox_req_seq)?;
        writeln!(f, "mailbox.request.kind={}", self.request_kind)?;
        writeln!(f, "mailbox.request.size={}", self.request_size)?;
        writeln!(f, "mailbox.request.seq={}", self.request_seq)?;
        writeln!(f, "mailbox.request.flags={}", self.request_flags)?;
        writeln!(f, "mailbox.request.a=0x{:x}", self.request_a)?;
        writeln!(f, "mailbox.request.b=0x{:x}", self.request_b)?;
        writeln!(f, "mailbox.request.c=0x{:x}", self.request_c)?;
        writeln!(f, "mailbox.request.d=0x{:x}", self.request_d)
    }
}

impl core::fmt::Display for TinyCorePioSmokeReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "r86-kvm.tinycore_pio_smoke=ok")?;
        writeln!(f, "kvm.api_version={}", self.api_version)?;
        writeln!(f, "kvm.arm_ipa_bits_cap={}", self.arm_ipa_bits_cap)?;
        writeln!(f, "kvm.chosen_ipa_bits={}", self.chosen_ipa_bits)?;
        writeln!(f, "kvm.vcpu_mmap_size={}", self.vcpu_mmap_size)?;
        writeln!(f, "vcpu.target={}", self.vcpu_target)?;
        writeln!(f, "vcpu.entry_ipa=0x{:x}", self.entry_ipa)?;
        writeln!(f, "vcpu.stack_ipa=0x{:x}", self.stack_ipa)?;
        writeln!(f, "boot.block_ipa=0x{:x}", self.boot_block_ipa)?;
        writeln!(f, "boot.mailbox_ipa=0x{:x}", self.mailbox_ipa)?;
        writeln!(f, "boot.kernel_entry=0x{:x}", self.kernel_entry)?;
        writeln!(f, "boot.kernel_payload_len={}", self.kernel_payload_len)?;
        writeln!(f, "boot.initrd_len={}", self.initrd_len)?;
        writeln!(f, "mmio.exits={}", self.mmio_exits)?;
        writeln!(f, "mailbox.requests_serviced={}", self.requests_serviced)?;
        writeln!(f, "mailbox.halt_seq={}", self.halt_seq)?;
        writeln!(
            f,
            "serial.output={}",
            String::from_utf8_lossy(&self.serial_output)
        )
    }
}

impl core::fmt::Display for TinyCoreRustMonitorSmokeReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "r86-kvm.tinycore_rust_monitor_smoke=ok")?;
        writeln!(f, "kvm.api_version={}", self.api_version)?;
        writeln!(f, "kvm.arm_ipa_bits_cap={}", self.arm_ipa_bits_cap)?;
        writeln!(f, "kvm.chosen_ipa_bits={}", self.chosen_ipa_bits)?;
        writeln!(f, "kvm.vcpu_mmap_size={}", self.vcpu_mmap_size)?;
        writeln!(f, "vcpu.target={}", self.vcpu_target)?;
        writeln!(f, "vcpu.entry_ipa=0x{:x}", self.entry_ipa)?;
        writeln!(f, "vcpu.stack_ipa=0x{:x}", self.stack_ipa)?;
        writeln!(f, "boot.block_ipa=0x{:x}", self.boot_block_ipa)?;
        writeln!(f, "boot.mailbox_ipa=0x{:x}", self.mailbox_ipa)?;
        writeln!(f, "boot.kernel_entry=0x{:x}", self.kernel_entry)?;
        writeln!(f, "boot.kernel_payload_len={}", self.kernel_payload_len)?;
        writeln!(f, "boot.initrd_len={}", self.initrd_len)?;
        writeln!(f, "monitor.elf_len={}", self.monitor_elf_len)?;
        writeln!(f, "monitor.elf_entry_offset=0x{:x}", self.elf.entry_offset)?;
        writeln!(f, "monitor.elf_load_segments={}", self.elf.load_segments)?;
        writeln!(f, "monitor.elf_file_bytes={}", self.elf.loaded_file_bytes)?;
        writeln!(f, "monitor.elf_mem_bytes={}", self.elf.loaded_mem_bytes)?;
        writeln!(
            f,
            "monitor.elf_executable_segments={}",
            self.elf.executable_segments
        )?;
        writeln!(f, "mmio.exits={}", self.mmio_exits)?;
        writeln!(f, "mailbox.requests_serviced={}", self.requests_serviced)?;
        writeln!(f, "mailbox.halt_seq={}", self.halt_seq)?;
        writeln!(
            f,
            "serial.output={}",
            String::from_utf8_lossy(&self.serial_output)
        )
    }
}

impl core::fmt::Display for TinyCoreX86MonitorSmokeReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "r86-kvm.tinycore_x86_monitor_smoke=ok")?;
        writeln!(f, "kvm.api_version={}", self.api_version)?;
        writeln!(f, "kvm.arm_ipa_bits_cap={}", self.arm_ipa_bits_cap)?;
        writeln!(f, "kvm.chosen_ipa_bits={}", self.chosen_ipa_bits)?;
        writeln!(f, "kvm.vcpu_mmap_size={}", self.vcpu_mmap_size)?;
        writeln!(f, "vcpu.target={}", self.vcpu_target)?;
        writeln!(f, "vcpu.entry_ipa=0x{:x}", self.entry_ipa)?;
        writeln!(f, "vcpu.stack_ipa=0x{:x}", self.stack_ipa)?;
        writeln!(f, "boot.block_ipa=0x{:x}", self.boot_block_ipa)?;
        writeln!(f, "boot.mailbox_ipa=0x{:x}", self.mailbox_ipa)?;
        writeln!(f, "boot.kernel_entry=0x{:x}", self.kernel_entry)?;
        writeln!(f, "boot.kernel_payload_len={}", self.kernel_payload_len)?;
        writeln!(f, "boot.initrd_len={}", self.initrd_len)?;
        writeln!(f, "monitor.elf_len={}", self.monitor_elf_len)?;
        writeln!(f, "monitor.elf_entry_offset=0x{:x}", self.elf.entry_offset)?;
        writeln!(f, "monitor.elf_load_segments={}", self.elf.load_segments)?;
        writeln!(f, "monitor.elf_file_bytes={}", self.elf.loaded_file_bytes)?;
        writeln!(f, "monitor.elf_mem_bytes={}", self.elf.loaded_mem_bytes)?;
        writeln!(
            f,
            "monitor.elf_executable_segments={}",
            self.elf.executable_segments
        )?;
        writeln!(f, "mmio.exits={}", self.mmio_exits)?;
        writeln!(f, "mailbox.requests_serviced={}", self.requests_serviced)?;
        writeln!(f, "mailbox.halt_seq={}", self.halt_seq)?;
        writeln!(
            f,
            "serial.output={}",
            String::from_utf8_lossy(&self.serial_output)
        )
    }
}

impl core::fmt::Display for TinyCoreNullMonitorSmokeReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "r86-kvm.tinycore_null_monitor_smoke=ok")?;
        writeln!(f, "kvm.api_version={}", self.api_version)?;
        writeln!(f, "kvm.arm_ipa_bits_cap={}", self.arm_ipa_bits_cap)?;
        writeln!(f, "kvm.chosen_ipa_bits={}", self.chosen_ipa_bits)?;
        writeln!(f, "kvm.vcpu_mmap_size={}", self.vcpu_mmap_size)?;
        writeln!(f, "vcpu.target={}", self.vcpu_target)?;
        writeln!(f, "vcpu.entry_ipa=0x{:x}", self.entry_ipa)?;
        writeln!(f, "vcpu.stack_ipa=0x{:x}", self.stack_ipa)?;
        writeln!(f, "boot.block_ipa=0x{:x}", self.boot_block_ipa)?;
        writeln!(f, "boot.mailbox_ipa=0x{:x}", self.mailbox_ipa)?;
        writeln!(f, "boot.kernel_entry=0x{:x}", self.kernel_entry)?;
        writeln!(f, "boot.kernel_payload_len={}", self.kernel_payload_len)?;
        writeln!(f, "boot.initrd_len={}", self.initrd_len)?;
        writeln!(f, "boot.step_limit={}", self.step_limit)?;
        writeln!(f, "boot.timer_poll_interval={}", self.timer_poll_interval)?;
        writeln!(f, "monitor.elapsed_ms={}", self.elapsed_ms)?;
        writeln!(f, "monitor.stop_steps_per_sec={}", self.stop_steps_per_sec)?;
        writeln!(f, "monitor.elf_len={}", self.monitor_elf_len)?;
        writeln!(f, "monitor.elf_entry_offset=0x{:x}", self.elf.entry_offset)?;
        writeln!(f, "monitor.elf_load_segments={}", self.elf.load_segments)?;
        writeln!(f, "monitor.elf_file_bytes={}", self.elf.loaded_file_bytes)?;
        writeln!(f, "monitor.elf_mem_bytes={}", self.elf.loaded_mem_bytes)?;
        writeln!(
            f,
            "monitor.elf_executable_segments={}",
            self.elf.executable_segments
        )?;
        writeln!(f, "mmio.exits={}", self.mmio_exits)?;
        writeln!(f, "mailbox.requests_serviced={}", self.requests_serviced)?;
        writeln!(f, "mailbox.halt_seq={}", self.halt_seq)?;
        writeln!(
            f,
            "mailbox.status_counts={}/{}/{}/{}",
            self.request_stats.ok,
            self.request_stats.unsupported,
            self.request_stats.fault,
            self.request_stats.bad_packet
        )?;
        writeln!(
            f,
            "mailbox.last_request=seq:{} kind:{} status:{} a:0x{:x} b:0x{:x} c:0x{:x} d:0x{:x}",
            self.request_stats.last_seq,
            self.request_stats.last_kind,
            self.request_stats.last_status,
            self.request_stats.last_a,
            self.request_stats.last_b,
            self.request_stats.last_c,
            self.request_stats.last_d
        )?;
        write!(f, "mailbox.kind_counts=")?;
        let mut wrote_kind = false;
        for index in 0..MONITOR_REQUEST_KIND_SLOTS {
            let count = self.request_stats.kind_counts[index];
            if count == 0 {
                continue;
            }
            if wrote_kind {
                write!(f, ",")?;
            }
            wrote_kind = true;
            write!(f, "{}:{}", index, count)?;
        }
        if !wrote_kind {
            write!(f, "none")?;
        }
        writeln!(f)?;
        for (index, counter) in self.request_stats.hot_pio.iter().enumerate() {
            if counter.count == 0 {
                continue;
            }
            let direction = if counter.is_write { "out" } else { "in" };
            writeln!(
                f,
                "mailbox.hot_pio.{index}={direction}:0x{:04x}/{} count={} last_c=0x{:08x}",
                counter.port, counter.width, counter.count, counter.last_c
            )?;
        }
        if let Some(stop) = self.stop_ack {
            writeln!(f, "monitor.stop.steps={}", stop.steps)?;
            writeln!(f, "monitor.stop.eip=0x{:08x}", stop.eip)?;
            writeln!(f, "monitor.stop.eflags=0x{:08x}", stop.eflags)?;
            writeln!(f, "monitor.stop.phys_writes={}", stop.phys_writes)?;
            writeln!(f, "monitor.stop.mmio_writes={}", stop.mmio_writes)?;
            writeln!(f, "monitor.stop.pio_outs={}", stop.pio_outs)?;
            writeln!(f, "monitor.stop.irq_polls={}", stop.irq_polls)?;
        }
        if let Some(stats) = self.hardmmu_stats {
            writeln!(f, "hardmmu.stats.parts_seen=0x{:08x}", stats.parts_seen)?;
            writeln!(f, "hardmmu.cpu.cr0=0x{:08x}", stats.cr0)?;
            writeln!(f, "hardmmu.cpu.cr3=0x{:08x}", stats.cr3)?;
            writeln!(f, "hardmmu.cpu.cr4=0x{:08x}", stats.cr4)?;
            writeln!(f, "hardmmu.cpu.eflags=0x{:08x}", stats.eflags)?;
            writeln!(
                f,
                "hardmmu.cpu.fetch_cache={}/{}/{}",
                stats.cpu_fetch_translation_hits,
                stats.cpu_fetch_translation_fills,
                stats.cpu_fetch_translation_invalidations
            )?;
            writeln!(
                f,
                "hardmmu.cpu.linear_cache={}/{}/{}",
                stats.cpu_linear_translation_hits,
                stats.cpu_linear_translation_fills,
                stats.cpu_linear_translation_invalidations
            )?;
            writeln!(
                f,
                "hardmmu.bus.native_reads={}/{}/{}",
                stats.bus_native_read_hits,
                stats.bus_native_read_attempts,
                stats.bus_native_read_aborts
            )?;
            writeln!(
                f,
                "hardmmu.bus.native_read_descriptor_aborts={}",
                stats.bus_native_read_descriptor_aborts
            )?;
            writeln!(
                f,
                "hardmmu.bus.native_read_abort_kinds={}/{}/{}",
                stats.bus_native_read_translation_aborts,
                stats.bus_native_read_access_flag_aborts,
                stats.bus_native_read_permission_aborts
            )?;
            writeln!(f, "hardmmu.bus.phys_reads={}", stats.bus_phys_reads)?;
            writeln!(f, "hardmmu.bus.tracked_writes={}", stats.bus_tracked_writes)?;
            writeln!(
                f,
                "hardmmu.shadow.translations={}",
                stats.shadow_translations
            )?;
            writeln!(f, "hardmmu.shadow.eligible_4k={}", stats.shadow_eligible_4k)?;
            writeln!(f, "hardmmu.shadow.read_fills={}", stats.shadow_read_fills)?;
            writeln!(f, "hardmmu.shadow.write_fills={}", stats.shadow_write_fills)?;
            writeln!(
                f,
                "hardmmu.shadow.stage1_installs={}",
                stats.shadow_stage1_installs
            )?;
            writeln!(
                f,
                "hardmmu.shadow.soft_unsupported_mode={}",
                stats.shadow_soft_unsupported_mode
            )?;
            writeln!(
                f,
                "hardmmu.shadow.soft_large_or_direct={}",
                stats.shadow_soft_large_or_direct
            )?;
            writeln!(
                f,
                "hardmmu.shadow.flushes={}/{}/{}/{}",
                stats.shadow_stage1_flushes,
                stats.shadow_control_flushes,
                stats.shadow_invlpg_flushes,
                stats.shadow_page_table_write_flushes
            )?;
            writeln!(
                f,
                "hardmmu.shadow.native_reads={}/{}/{}",
                stats.shadow_native_read_hits,
                stats.shadow_native_read_attempts,
                stats.shadow_native_read_aborts
            )?;
            writeln!(
                f,
                "hardmmu.shadow.native_read_descriptor_aborts={}",
                stats.shadow_native_read_descriptor_aborts
            )?;
            writeln!(
                f,
                "hardmmu.shadow.native_read_abort_kinds={}/{}/{}",
                stats.shadow_native_read_translation_aborts,
                stats.shadow_native_read_access_flag_aborts,
                stats.shadow_native_read_permission_aborts
            )?;
            writeln!(f, "hardmmu.shadow.errors={}", stats.shadow_errors)?;
            writeln!(
                f,
                "hardmmu.shadow.contexts={}/{}/{}",
                stats.context_hits, stats.context_misses, stats.context_evictions
            )?;
        }
        if let Some(debug) = self.debug_state {
            writeln!(f, "debug.heartbeat={}", debug.heartbeat)?;
            writeln!(f, "debug.phase={}", debug.phase_name())?;
            writeln!(f, "debug.panic_flag={}", debug.panic_flag)?;
            writeln!(f, "debug.steps={}", debug.steps)?;
            writeln!(f, "debug.eip=0x{:08x}", debug.eip)?;
            writeln!(f, "debug.cr3=0x{:08x}", debug.cr3)?;
            writeln!(
                f,
                "debug.native_reads={}/{}/{}",
                debug.native_read_hits, debug.native_reads, debug.native_read_aborts
            )?;
            writeln!(f, "debug.stage1_installs={}", debug.stage1_installs)?;
            writeln!(
                f,
                "debug.stage1_leaf_invalidations={}",
                debug.stage1_leaf_invalidations
            )?;
            writeln!(f, "debug.shadow.translations={}", debug.shadow_translations)?;
            writeln!(f, "debug.shadow.eligible_4k={}", debug.shadow_eligible_4k)?;
            writeln!(
                f,
                "debug.shadow.fills={}/{}/{}/{}",
                debug.shadow_read_fills,
                debug.shadow_write_fills,
                debug.shadow_upgrades,
                debug.shadow_unchanged_fills
            )?;
            writeln!(
                f,
                "debug.shadow.soft={}/{}/{}/{}",
                debug.shadow_soft_counts[0],
                debug.shadow_soft_counts[1],
                debug.shadow_soft_counts[2],
                debug.shadow_soft_counts[3]
            )?;
            writeln!(
                f,
                "debug.shadow.flushes={}/{}/{}/{}",
                debug.shadow_flushes,
                debug.shadow_flush_by_reason[0],
                debug.shadow_flush_by_reason[1],
                debug.shadow_flush_by_reason[2]
            )?;
            writeln!(
                f,
                "debug.shadow.page_walk_write_ignores={}",
                debug.shadow_page_walk_write_ignores
            )?;
            writeln!(
                f,
                "debug.shadow.page_table_write_invalidations={}",
                debug.shadow_page_table_write_invalidations
            )?;
            writeln!(
                f,
                "debug.linear_cache={}/{}/{}",
                debug.linear_cache[0], debug.linear_cache[1], debug.linear_cache[2]
            )?;
            let (top_eip_bucket, top_eip_samples) = debug.top_eip_bucket();
            writeln!(
                f,
                "debug.eip_histogram.top={}:{}",
                top_eip_bucket, top_eip_samples
            )?;
            writeln!(f, "debug.shadow_errors={}", debug.shadow_errors)?;
            writeln!(
                f,
                "debug.mailbox={} seq={}",
                debug.last_mailbox_kind, debug.last_mailbox_seq
            )?;
        }
        if let Some(panic) = self.panic_block {
            writeln!(f, "panic.code={}", panic.panic_code)?;
            writeln!(f, "panic.depth={}", panic.in_panic_depth)?;
            writeln!(f, "panic.esr=0x{:016x}", panic.esr)?;
            writeln!(f, "panic.far=0x{:016x}", panic.far)?;
            writeln!(f, "panic.elr=0x{:016x}", panic.elr)?;
            writeln!(f, "panic.sp=0x{:016x}", panic.sp)?;
        }
        writeln!(
            f,
            "serial.output={}",
            String::from_utf8_lossy(&self.serial_output)
        )?;
        writeln!(
            f,
            "serial.success_marker_seen={}",
            self.serial_success_marker_seen
        )?;
        writeln!(
            f,
            "serial.success_stop_seq={}",
            self.serial_success_stop_seq
        )?;
        writeln!(f, "vga.decompress_marker={}", self.vga_decompress_marker)?;
        writeln!(f, "vga.text<<EOF")?;
        write!(f, "{}", self.vga_text)?;
        writeln!(f, "EOF")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kvm::boot::{DirectBootAssets, prepare_direct_linux_boot};
    use crate::kvm::probe::ProbeConfig;
    use crate::linux_boot::KERNEL_LOAD_ADDR;

    #[test]
    fn vga_text_snapshot_reads_text_plane_inside_legacy_vga_window() {
        let mut vga = vec![0_u8; VGA_TEXT_BYTES];
        vga[0] = b'D';
        vga[1] = 0x07;
        vga[2] = b'e';
        vga[3] = 0x07;

        assert_eq!(vga_text_snapshot(&vga), "De\n");
    }

    #[test]
    fn validates_published_hello_request_snapshot() {
        let mailbox = MailboxRequestSnapshot {
            req_seq: 1,
            request: MonitorRequestV1::hello(1, 0, 0),
        };

        validate_hello_request(&mailbox).unwrap();
    }

    #[test]
    fn mailbox_offsets_match_abi_packet_layout() {
        assert_eq!(MAILBOX_REQ_SEQ_OFFSET, 4);
        assert_eq!(MAILBOX_REQUEST_OFFSET, 16);
        assert_eq!(REQUEST_KIND_OFFSET, 0);
        assert_eq!(REQUEST_SIZE_OFFSET, 4);
        assert_eq!(REQUEST_SEQ_OFFSET, 8);
        assert_eq!(REQUEST_FLAGS_OFFSET, 12);
        assert_eq!(REQUEST_A_OFFSET, 16);
        assert_eq!(REQUEST_B_OFFSET, 24);
        assert_eq!(REQUEST_C_OFFSET, 32);
        assert_eq!(
            REQUEST_D_OFFSET + 8,
            core::mem::size_of::<MonitorRequestV1>()
        );
    }

    #[test]
    fn monitor_debug_reader_accepts_stable_seqlock_snapshot() {
        let mut debug = DebugStateBlockV1::new();
        debug.seq = 2;
        debug.heartbeat = 7;
        debug.phase = 3;
        debug.steps = 123;
        debug.eip = 0xc001_2345;
        debug.cr3 = 0x00ab_c000;
        debug.stage1_installs = 99;
        debug.native_reads = 88;
        debug.shadow_errors = 1;
        let mut panic = PanicBlockV1::new();
        panic.panic_code = 2;
        panic.esr = 0x9600_0045;
        panic.elr = 0xffff_0000_1234_5678;
        let reader = MonitorDebugReader {
            debug_ptr: (&debug as *const DebugStateBlockV1) as usize,
            panic_ptr: (&panic as *const PanicBlockV1) as usize,
        };

        let snapshot = reader.read_debug_state().unwrap();
        assert_eq!(snapshot.heartbeat, 7);
        assert_eq!(snapshot.steps, 123);
        assert_eq!(snapshot.eip, 0xc001_2345);
        assert_eq!(snapshot.cr3, 0x00ab_c000);
        assert_eq!(snapshot.stage1_installs, 99);
        assert_eq!(snapshot.native_reads, 88);
        assert_eq!(snapshot.shadow_errors, 1);

        let panic_snapshot = reader.read_panic_block().unwrap();
        assert_eq!(panic_snapshot.panic_code, 2);
        assert_eq!(panic_snapshot.esr, 0x9600_0045);
        assert_eq!(panic_snapshot.elr, 0xffff_0000_1234_5678);
    }

    #[test]
    fn can_read_hello_request_from_prepared_shared_backing() {
        let mut probe = ProbeConfig::default();
        probe.ram_size = 64 * 1024 * 1024;
        let config = DirectBootConfig {
            host_decompress_initrd: false,
            ..DirectBootConfig::default()
        };
        let prepared = prepare_direct_linux_boot(&probe, &synthetic_assets(), &config).unwrap();
        let mut backing = prepared.backing;
        let shared_slot = find_slot(&prepared.planned.memslots, MemslotKind::Shared).unwrap();
        let shared = backing
            .iter_mut()
            .find(|backing| backing.slot == shared_slot.slot)
            .unwrap();
        let mailbox_start = SHARED_MAILBOX_OFFSET as usize;
        let request_start = mailbox_start + MAILBOX_REQUEST_OFFSET;
        write_test_u32(
            shared.memory.as_mut_slice(),
            mailbox_start + MAILBOX_REQ_SEQ_OFFSET,
            1,
        );
        write_test_u32(
            shared.memory.as_mut_slice(),
            request_start + REQUEST_KIND_OFFSET,
            MonitorRequestKindV1::Hello as u32,
        );
        write_test_u32(
            shared.memory.as_mut_slice(),
            request_start + REQUEST_SIZE_OFFSET,
            core::mem::size_of::<MonitorRequestV1>() as u32,
        );
        write_test_u32(
            shared.memory.as_mut_slice(),
            request_start + REQUEST_SEQ_OFFSET,
            1,
        );
        write_test_u64(
            shared.memory.as_mut_slice(),
            request_start + REQUEST_A_OFFSET,
            ABI_VERSION as u64,
        );

        let mailbox = read_mailbox_request(&prepared.planned.memslots, &backing).unwrap();
        validate_hello_request(&mailbox).unwrap();
    }

    fn synthetic_assets() -> DirectBootAssets {
        let mut kernel = vec![0_u8; 0x5000];
        kernel[0x1f1] = 31;
        kernel[0x202..0x206].copy_from_slice(b"HdrS");
        kernel[0x206..0x208].copy_from_slice(&0x020f_u16.to_le_bytes());
        kernel[0x4000..0x5000].fill(0x5a);
        let assets = DirectBootAssets {
            kernel,
            initrd: vec![0xaa; 4096],
        };
        assert_eq!(KERNEL_LOAD_ADDR, 0x0010_0000);
        assets
    }

    fn write_test_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_test_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
}
