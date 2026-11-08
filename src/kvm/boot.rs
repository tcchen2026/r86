use std::fs;
use std::mem::{align_of, size_of};
use std::path::PathBuf;

use crate::initrd_overlay::InitrdFile;
use crate::iso9660::Iso9660;
use crate::linux_boot::{BootConfig, DirectLinuxBoot, LoadedBoot};
use crate::monitor::abi::{
    BootBlockV1, CpuMsrStateV1, CpuStateV1, DescriptorTableV1, FpuStateV1, HostCommandV1,
    HostResponseV1, IpaLayoutV1, MailboxV1, MonitorRequestV1, PAGE_SIZE_4K, RegionEntryV1,
    RegionTableV1, SegmentStateV1, TraceModeV1,
    debug::{DEBUG_STATE_BLOCK_OFFSET, PANIC_BLOCK_OFFSET},
};

use crate::kvm::backing::SlotBacking;
use crate::kvm::error::{KvmError, Result};
use crate::kvm::memslot::{MemslotKind, PlannedMemslot};
use crate::kvm::probe::{PlannedHardMmu, ProbeConfig};

pub const DEFAULT_TINYCORE_ISO: &str = "refs/TinyCore-current.iso";
pub const DEFAULT_TINYCORE_CMDLINE: &str =
    "console=ttyS0,115200n8 earlyprintk=serial,ttyS0,115200 loglevel=7 nozswap";

pub const SHARED_BOOT_BLOCK_OFFSET: u64 = 0;
pub const SHARED_CPU_STATE_OFFSET: u64 = PAGE_SIZE_4K;
pub const SHARED_MAILBOX_OFFSET: u64 = PAGE_SIZE_4K * 2;
pub const SHARED_DEBUG_STATE_OFFSET: u64 = DEBUG_STATE_BLOCK_OFFSET;
pub const SHARED_PANIC_BLOCK_OFFSET: u64 = PANIC_BLOCK_OFFSET;
pub const TRACE_RING_OFFSET: u64 = 0;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectBootConfig {
    pub iso_path: PathBuf,
    pub snapshot_in: Option<PathBuf>,
    pub snapshot_out: Option<PathBuf>,
    pub machine_state_out: Option<PathBuf>,
    /// Forward monitor serial packets immediately, including observer endpoints.
    pub serial_stdout: bool,
    /// Export the firmware monitor's completed RGBA frame after it stops.
    pub frame_out: Option<PathBuf>,
    pub cmdline: String,
    pub host_decompress_kernel: bool,
    pub host_decompress_initrd: bool,
    pub initrd_files: Vec<InitrdFile>,
    pub bootlocal_command: Option<String>,
    pub serial_success_marker: Option<String>,
    pub stop_on_serial_marker: Option<String>,
    pub stop_at_step: Option<u64>,
    pub timer_poll_interval: u64,
    pub step_limit: u64,
    pub trace_mode: TraceModeV1,
    pub progress: bool,
    pub timeout_secs: Option<u64>,
    pub stall_timeout_secs: Option<u64>,
    pub json_progress: bool,
}

impl Default for DirectBootConfig {
    fn default() -> Self {
        Self {
            iso_path: PathBuf::from(DEFAULT_TINYCORE_ISO),
            snapshot_in: None,
            snapshot_out: None,
            machine_state_out: None,
            serial_stdout: false,
            frame_out: None,
            cmdline: DEFAULT_TINYCORE_CMDLINE.to_string(),
            host_decompress_kernel: false,
            host_decompress_initrd: true,
            initrd_files: Vec::new(),
            bootlocal_command: None,
            serial_success_marker: None,
            stop_on_serial_marker: None,
            stop_at_step: None,
            timer_poll_interval: 10_000,
            step_limit: 0,
            trace_mode: TraceModeV1::Off,
            progress: false,
            timeout_secs: None,
            stall_timeout_secs: None,
            json_progress: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectBootAssets {
    pub kernel: Vec<u8>,
    pub initrd: Vec<u8>,
}

#[derive(Debug)]
pub struct PreparedHardMmuBoot {
    pub planned: PlannedHardMmu,
    pub backing: Vec<SlotBacking>,
    pub boot_block: BootBlockV1,
    pub cpu_state: CpuStateV1,
    pub loaded: LoadedBoot,
    pub reboot_assets: Option<DirectBootAssets>,
    pub boot_block_ipa: u64,
    pub cpu_state_ipa: u64,
    pub mailbox_ipa: u64,
    pub trace_ipa: u64,
}

pub fn prepare_tinycore_direct_boot(
    probe: &ProbeConfig,
    config: &DirectBootConfig,
) -> Result<PreparedHardMmuBoot> {
    let iso_bytes = fs::read(&config.iso_path).map_err(|source| KvmError::Io {
        op: "read TinyCore ISO",
        source,
    })?;
    let assets = tinycore_assets_from_iso(&iso_bytes)?;
    prepare_direct_linux_boot(probe, &assets, config)
}

/// Prepare immutable ISO/ROM input for a full firmware Machine monitor.
pub fn prepare_firmware_boot(
    probe: &ProbeConfig,
    config: &DirectBootConfig,
    bios: &std::path::Path,
    vga: &std::path::Path,
    frame_ms: u64,
) -> Result<PreparedHardMmuBoot> {
    use crate::monitor::firmware::{INPUT_MAGIC, INPUT_OFFSET};
    use std::io::Read;
    if !probe.machine_monitor || frame_ms == 0 {
        return Err(KvmError::BadArgument(
            "firmware monitor needs Machine and a positive frame interval".into(),
        ));
    }
    let planned = probe.validate_for_layout_only()?;
    let mut backing = planned.memslots.allocate_backing()?;
    let cpu_state = crate::monitor::cpu_state_v1_from_cpu(&crate::x86::cpu::Cpu::new(), 0);
    let (mut boot_block, boot_block_ipa, cpu_state_ipa, mailbox_ipa, trace_ipa) =
        write_control_pages(&planned, &mut backing, cpu_state, config)?;
    boot_block.feature_bits &= !crate::monitor::abi::FEATURE_MACHINE_BOOT_RELOAD;
    let shared = required_slot(&planned.memslots, MemslotKind::Shared)?;
    write_boot_block_at(
        backing_for_slot_mut(&mut backing, shared.slot)?,
        SHARED_BOOT_BLOCK_OFFSET,
        &boot_block,
    )?;
    let trace = required_slot(&planned.memslots, MemslotKind::Trace)?;
    let bytes = backing_for_slot_mut(&mut backing, trace.slot)?
        .memory
        .as_mut_slice();
    let mut header = [INPUT_MAGIC, 0, 0, 0, 0, 0, 0, frame_ms];
    let mut cursor = INPUT_OFFSET + 4096;
    for (i, path) in [config.iso_path.as_path(), bios, vga].iter().enumerate() {
        let mut file = fs::File::open(path).map_err(|source| KvmError::Io {
            op: "open firmware input",
            source,
        })?;
        let len = usize::try_from(
            file.metadata()
                .map_err(|source| KvmError::Io {
                    op: "stat firmware input",
                    source,
                })?
                .len(),
        )
        .map_err(|_| KvmError::MemslotPlan("firmware input length"))?;
        let end = cursor
            .checked_add(len)
            .ok_or(KvmError::MemslotPlan("input length overflow"))?;
        let dst = bytes
            .get_mut(cursor..end)
            .ok_or(KvmError::MemslotPlan("input exceeds trace backing"))?;
        file.read_exact(dst).map_err(|source| KvmError::Io {
            op: "read firmware input",
            source,
        })?;
        header[1 + i * 2] = cursor as u64;
        header[2 + i * 2] = len as u64;
        cursor = end
            .checked_add(4095)
            .ok_or(KvmError::MemslotPlan("input alignment overflow"))?
            & !4095;
    }
    for (i, value) in header.iter().enumerate() {
        bytes[INPUT_OFFSET + i * 8..INPUT_OFFSET + (i + 1) * 8]
            .copy_from_slice(&value.to_le_bytes());
    }
    Ok(PreparedHardMmuBoot {
        planned,
        backing,
        boot_block,
        cpu_state,
        loaded: LoadedBoot {
            entry: cpu_state.eip,
            boot_params: 0,
            kernel_payload_len: 0,
            initrd_len: 0,
            initrd_load_addr: 0,
        },
        reboot_assets: None,
        boot_block_ipa,
        cpu_state_ipa,
        mailbox_ipa,
        trace_ipa,
    })
}

pub fn prepare_snapshot_restore_boot(
    probe: &ProbeConfig,
    snapshot: &crate::kvm::snapshot::HardMmuSnapshot,
    config: &DirectBootConfig,
) -> Result<PreparedHardMmuBoot> {
    let mut probe = probe.clone();
    probe.ram_size = snapshot.ram_size;
    let planned = probe.validate_for_layout_only()?;
    let mut backing = planned.memslots.allocate_backing()?;
    crate::kvm::snapshot::scatter_x86_ram(&planned.memslots, &mut backing, &snapshot.ram)?;
    let (boot_block, boot_block_ipa, cpu_state_ipa, mailbox_ipa, trace_ipa) =
        write_control_pages(&planned, &mut backing, snapshot.cpu_state, config)?;
    let loaded = LoadedBoot {
        entry: snapshot.cpu_state.eip,
        boot_params: snapshot.cpu_state.gpr[6],
        kernel_payload_len: 0,
        initrd_len: 0,
        initrd_load_addr: 0,
    };

    Ok(PreparedHardMmuBoot {
        planned,
        backing,
        boot_block,
        cpu_state: snapshot.cpu_state,
        reboot_assets: None,
        loaded,
        boot_block_ipa,
        cpu_state_ipa,
        mailbox_ipa,
        trace_ipa,
    })
}

pub fn tinycore_assets_from_iso(iso_bytes: &[u8]) -> Result<DirectBootAssets> {
    let iso = Iso9660::open(iso_bytes).map_err(KvmError::BadArgument)?;
    Ok(DirectBootAssets {
        kernel: iso
            .read_file("boot/vmlinuz")
            .map_err(KvmError::BadArgument)?,
        initrd: iso
            .read_file("boot/core.gz")
            .map_err(KvmError::BadArgument)?,
    })
}

pub fn prepare_direct_linux_boot(
    probe: &ProbeConfig,
    assets: &DirectBootAssets,
    config: &DirectBootConfig,
) -> Result<PreparedHardMmuBoot> {
    let planned = probe.validate_for_layout_only()?;
    let ram_len = usize::try_from(planned.layout.ram_size)
        .map_err(|_| KvmError::MemslotPlan("x86 RAM does not fit host usize"))?;
    let mut ram = vec![0_u8; ram_len];
    let loaded = load_direct_linux_ram(&mut ram, assets, config)?;

    let mut backing = planned.memslots.allocate_backing()?;
    scatter_ram_to_backing(&planned, &ram, &mut backing)?;
    let cpu_state = CpuStateV1::protected_flat_linux_entry(loaded.entry, loaded.boot_params);
    let (boot_block, boot_block_ipa, cpu_state_ipa, mailbox_ipa, trace_ipa) =
        write_control_pages(&planned, &mut backing, cpu_state, config)?;
    let reboot_assets = planned.machine_monitor.then(|| assets.clone());

    Ok(PreparedHardMmuBoot {
        planned,
        backing,
        boot_block,
        cpu_state,
        loaded,
        reboot_assets,
        boot_block_ipa,
        cpu_state_ipa,
        mailbox_ipa,
        trace_ipa,
    })
}

fn load_direct_linux_ram(
    ram: &mut [u8],
    assets: &DirectBootAssets,
    config: &DirectBootConfig,
) -> Result<LoadedBoot> {
    let mut overlay_files = config.initrd_files.clone();
    overlay_files.extend(bootlocal_overlay_files(
        config.bootlocal_command.as_deref(),
    )?);
    let ram_size = u32::try_from(ram.len())
        .map_err(|_| KvmError::MemslotPlan("x86 RAM exceeds 32-bit Linux boot params"))?;
    DirectLinuxBoot::load(
        ram,
        &assets.kernel,
        &assets.initrd,
        &BootConfig {
            mem_size: ram_size,
            cmdline: &config.cmdline,
            host_decompress_kernel: config.host_decompress_kernel,
            host_decompress_initrd: config.host_decompress_initrd,
            overlay_files: &overlay_files,
            dump_initrd_path: None,
        },
    )
    .map_err(KvmError::BadArgument)
}

pub(crate) fn reload_machine_boot(
    planned: &PlannedHardMmu,
    backing: &mut [SlotBacking],
    assets: &DirectBootAssets,
    config: &DirectBootConfig,
) -> Result<()> {
    let ram_slot = required_slot(&planned.memslots, MemslotKind::X86Ram)?;
    if !planned.machine_monitor
        || ram_slot.guest_phys_addr != planned.layout.ram_base
        || ram_slot.memory_size != planned.layout.ram_size
    {
        return Err(KvmError::MemslotPlan(
            "machine reload requires contiguous RAM",
        ));
    }
    // KVM_RUN is stopped on the synchronous ReloadBoot doorbell. The monitor
    // resets CPU caches, shadow mappings and devices before executing again.
    let ram = backing_for_slot_mut(backing, ram_slot.slot)?
        .memory
        .as_mut_slice();
    ram.fill(0);
    load_direct_linux_ram(ram, assets, config)?;
    Ok(())
}

fn scatter_ram_to_backing(
    planned: &PlannedHardMmu,
    ram: &[u8],
    backing: &mut [SlotBacking],
) -> Result<()> {
    for slot in planned
        .memslots
        .slots()
        .iter()
        .filter(|slot| slot.kind == MemslotKind::X86Ram)
    {
        let ram_offset = slot
            .guest_phys_addr
            .checked_sub(planned.layout.ram_base)
            .ok_or(KvmError::MemslotPlan("RAM memslot precedes RAM base"))?;
        let start = usize::try_from(ram_offset)
            .map_err(|_| KvmError::MemslotPlan("RAM memslot offset too large"))?;
        let len = usize::try_from(slot.memory_size)
            .map_err(|_| KvmError::MemslotPlan("RAM memslot size too large"))?;
        let end = start
            .checked_add(len)
            .ok_or(KvmError::MemslotPlan("RAM memslot end overflows"))?;
        let bytes = ram
            .get(start..end)
            .ok_or(KvmError::MemslotPlan("RAM memslot exceeds prepared RAM"))?;
        let slot_backing = backing_for_slot_mut(backing, slot.slot)?;
        slot_backing.write_at(0, bytes)?;
    }
    Ok(())
}

fn write_control_pages(
    planned: &PlannedHardMmu,
    backing: &mut [SlotBacking],
    cpu_state: CpuStateV1,
    config: &DirectBootConfig,
) -> Result<(BootBlockV1, u64, u64, u64, u64)> {
    let shared = required_slot(&planned.memslots, MemslotKind::Shared)?;
    let trace = required_slot(&planned.memslots, MemslotKind::Trace)?;
    ensure_slot_covers(shared, SHARED_PANIC_BLOCK_OFFSET + PAGE_SIZE_4K)?;
    ensure_slot_covers(trace, TRACE_RING_OFFSET + PAGE_SIZE_4K)?;

    let mut boot_block = BootBlockV1::new(planned.layout);
    boot_block.regions = planned.regions;
    boot_block.feature_bits |=
        crate::monitor::abi::FEATURE_DEBUG_STATE_BLOCK | crate::monitor::abi::FEATURE_PANIC_BLOCK;
    boot_block.feature_bits |= if planned.machine_monitor {
        crate::monitor::abi::FEATURE_MACHINE_DEVICES
            | crate::monitor::abi::FEATURE_MACHINE_BOOT_RELOAD
    } else {
        crate::monitor::abi::FEATURE_SNAPSHOT
    };
    if config.machine_state_out.is_some() {
        if !planned.machine_monitor {
            return Err(KvmError::BadArgument(
                "--machine-state-out requires --machine-monitor".into(),
            ));
        }
        boot_block.feature_bits |= crate::monitor::abi::FEATURE_MACHINE_STATE_V1;
    }
    boot_block.initial_cpu_state_offset = SHARED_CPU_STATE_OFFSET;
    boot_block.mailbox_offset = SHARED_MAILBOX_OFFSET;
    boot_block.trace_offset = TRACE_RING_OFFSET;
    boot_block.set_debug_state_offset(SHARED_DEBUG_STATE_OFFSET);
    boot_block.set_panic_block_offset(SHARED_PANIC_BLOCK_OFFSET);
    boot_block.timer_poll_interval = config.timer_poll_interval;
    boot_block.step_limit = config.step_limit;
    boot_block.trace_mode = config.trace_mode as u32;
    boot_block.validate(planned.required_ipa_bits)?;

    let shared_backing = backing_for_slot_mut(backing, shared.slot)?;
    write_boot_block_at(shared_backing, SHARED_BOOT_BLOCK_OFFSET, &boot_block)?;
    write_cpu_state_at(shared_backing, SHARED_CPU_STATE_OFFSET, &cpu_state)?;
    write_mailbox_at(shared_backing, SHARED_MAILBOX_OFFSET, &MailboxV1::new())?;

    Ok((
        boot_block,
        shared.guest_phys_addr + SHARED_BOOT_BLOCK_OFFSET,
        shared.guest_phys_addr + SHARED_CPU_STATE_OFFSET,
        shared.guest_phys_addr + SHARED_MAILBOX_OFFSET,
        trace.guest_phys_addr + TRACE_RING_OFFSET,
    ))
}

fn write_boot_block_at(
    backing: &mut SlotBacking,
    offset: u64,
    boot_block: &BootBlockV1,
) -> Result<()> {
    let mut bytes = vec![0_u8; size_of::<BootBlockV1>()];
    let mut at = 0;
    put_u32(&mut bytes, at, boot_block.magic);
    at += size_of::<u32>();
    put_u16(&mut bytes, at, boot_block.abi_version);
    at += size_of::<u16>();
    put_u16(&mut bytes, at, boot_block.size);
    at += size_of::<u16>();
    put_u64(&mut bytes, at, boot_block.feature_bits);
    at += size_of::<u64>();
    write_ipa_layout(&mut bytes, at, &boot_block.layout);
    at += size_of::<IpaLayoutV1>();
    write_region_table(&mut bytes, at, &boot_block.regions);
    at += size_of::<RegionTableV1>();
    put_u64(&mut bytes, at, boot_block.initial_cpu_state_offset);
    at += size_of::<u64>();
    put_u64(&mut bytes, at, boot_block.mailbox_offset);
    at += size_of::<u64>();
    put_u64(&mut bytes, at, boot_block.trace_offset);
    at += size_of::<u64>();
    put_u64(&mut bytes, at, boot_block.timer_poll_interval);
    at += size_of::<u64>();
    put_u64(&mut bytes, at, boot_block.step_limit);
    at += size_of::<u64>();
    put_u32(&mut bytes, at, boot_block.trace_mode);
    at += size_of::<u32>();
    put_u32(&mut bytes, at, boot_block.reserved0);
    at += size_of::<u32>();
    put_bytes(&mut bytes, at, &boot_block.build_id);
    at += boot_block.build_id.len();
    for value in boot_block.reserved1 {
        put_u64(&mut bytes, at, value);
        at += size_of::<u64>();
    }
    debug_assert!(at <= bytes.len());
    backing.write_at(offset, &bytes)
}

fn write_cpu_state_at(
    backing: &mut SlotBacking,
    offset: u64,
    cpu_state: &CpuStateV1,
) -> Result<()> {
    let mut bytes = vec![0_u8; size_of::<CpuStateV1>()];
    let mut at = 0;
    for value in cpu_state.gpr {
        put_u32(&mut bytes, at, value);
        at += size_of::<u32>();
    }
    put_u32(&mut bytes, at, cpu_state.eip);
    at += size_of::<u32>();
    put_u32(&mut bytes, at, cpu_state.eflags);
    at += size_of::<u32>();
    put_u32(&mut bytes, at, cpu_state.cr0);
    at += size_of::<u32>();
    put_u32(&mut bytes, at, cpu_state.cr2);
    at += size_of::<u32>();
    put_u32(&mut bytes, at, cpu_state.cr3);
    at += size_of::<u32>();
    put_u32(&mut bytes, at, cpu_state.cr4);
    at += size_of::<u32>();
    put_u8(&mut bytes, at, cpu_state.cpl);
    at += size_of::<u8>();
    put_u8(&mut bytes, at, cpu_state.halted);
    at += size_of::<u8>();
    put_u8(&mut bytes, at, cpu_state.interrupt_shadow);
    at += size_of::<u8>();
    put_u8(&mut bytes, at, cpu_state.reserved0);
    at += size_of::<u8>();
    for segment in cpu_state.seg {
        write_segment_state(&mut bytes, at, &segment);
        at += size_of::<SegmentStateV1>();
    }
    at = align_up_usize(at, align_of::<DescriptorTableV1>());
    write_descriptor_table(&mut bytes, at, &cpu_state.gdtr);
    at += size_of::<DescriptorTableV1>();
    write_descriptor_table(&mut bytes, at, &cpu_state.idtr);
    at += size_of::<DescriptorTableV1>();
    write_segment_state(&mut bytes, at, &cpu_state.ldtr);
    at += size_of::<SegmentStateV1>();
    write_segment_state(&mut bytes, at, &cpu_state.tr);
    at += size_of::<SegmentStateV1>();
    at = align_up_usize(at, align_of::<u64>());
    put_u64(&mut bytes, at, cpu_state.tsc);
    at += size_of::<u64>();
    put_u64(&mut bytes, at, cpu_state.instr_count);
    at += size_of::<u64>();
    put_bytes(&mut bytes, at, &cpu_state.fxsave);
    at += cpu_state.fxsave.len();
    for value in cpu_state.pae_pdptrs {
        put_u64(&mut bytes, at, value);
        at += size_of::<u64>();
    }
    for value in cpu_state.dr {
        put_u32(&mut bytes, at, value);
        at += size_of::<u32>();
    }
    write_cpu_msr_state(&mut bytes, at, &cpu_state.msr);
    at += size_of::<CpuMsrStateV1>();
    write_fpu_state(&mut bytes, at, &cpu_state.fpu);
    at += size_of::<FpuStateV1>();
    debug_assert_eq!(at, bytes.len());
    backing.write_at(offset, &bytes)
}

fn write_mailbox_at(backing: &mut SlotBacking, offset: u64, mailbox: &MailboxV1) -> Result<()> {
    let mut bytes = vec![0_u8; size_of::<MailboxV1>()];
    let mut at = 0;
    put_u16(&mut bytes, at, mailbox.abi_version);
    at += size_of::<u16>();
    put_u16(&mut bytes, at, mailbox.header_size);
    at += size_of::<u16>();
    put_u32(&mut bytes, at, mailbox.req_seq);
    at += size_of::<u32>();
    put_u32(&mut bytes, at, mailbox.resp_seq);
    at += size_of::<u32>();
    put_u32(&mut bytes, at, mailbox.command_seq);
    at += size_of::<u32>();
    write_monitor_request(&mut bytes, at, &mailbox.request);
    at += size_of::<MonitorRequestV1>();
    write_host_response(&mut bytes, at, &mailbox.response);
    at += size_of::<HostResponseV1>();
    write_host_command(&mut bytes, at, &mailbox.command);
    at += size_of::<HostCommandV1>();
    debug_assert_eq!(at, bytes.len());
    backing.write_at(offset, &bytes)
}

fn write_ipa_layout(bytes: &mut [u8], offset: usize, layout: &IpaLayoutV1) {
    let mut at = offset;
    for value in [
        layout.ram_base,
        layout.ram_size,
        layout.monitor_base,
        layout.monitor_size,
        layout.shared_base,
        layout.shared_size,
        layout.trace_base,
        layout.trace_size,
        layout.doorbell_ipa,
        layout.reserved,
    ] {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
}

fn write_region_table(bytes: &mut [u8], offset: usize, table: &RegionTableV1) {
    let mut at = offset;
    put_u32(bytes, at, table.count);
    at += size_of::<u32>();
    put_u32(bytes, at, table.reserved);
    at += size_of::<u32>();
    for entry in table.entries {
        write_region_entry(bytes, at, &entry);
        at += size_of::<RegionEntryV1>();
    }
}

fn write_region_entry(bytes: &mut [u8], offset: usize, entry: &RegionEntryV1) {
    let mut at = offset;
    put_u64(bytes, at, entry.base);
    at += size_of::<u64>();
    put_u64(bytes, at, entry.len);
    at += size_of::<u64>();
    put_u32(bytes, at, entry.kind);
    at += size_of::<u32>();
    put_u32(bytes, at, entry.policy);
}

fn write_segment_state(bytes: &mut [u8], offset: usize, segment: &SegmentStateV1) {
    let mut at = offset;
    put_u16(bytes, at, segment.selector);
    at += size_of::<u16>();
    put_u16(bytes, at, segment.attr);
    at += size_of::<u16>();
    put_u32(bytes, at, segment.limit);
    at += size_of::<u32>();
    put_u32(bytes, at, segment.base);
}

fn write_descriptor_table(bytes: &mut [u8], offset: usize, table: &DescriptorTableV1) {
    let mut at = offset;
    put_u64(bytes, at, table.base);
    at += size_of::<u64>();
    put_u16(bytes, at, table.limit);
    at += size_of::<u16>();
    for value in table.reserved {
        put_u16(bytes, at, value);
        at += size_of::<u16>();
    }
}

fn write_cpu_msr_state(bytes: &mut [u8], offset: usize, msr: &CpuMsrStateV1) {
    let mut at = offset;
    put_u64(bytes, at, msr.apic_base);
    at += size_of::<u64>();
    put_u32(bytes, at, msr.test_ctl);
    at += size_of::<u32>();
    put_u32(bytes, at, msr.reserved0);
    at += size_of::<u32>();
    put_u64(bytes, at, msr.bios_update_trigger);
    at += size_of::<u64>();
    for value in msr.bbl_cr_data {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
    for value in [
        msr.bbl_cr_addr,
        msr.bbl_cr_decc,
        msr.bbl_cr_ctl,
        msr.bbl_cr_trig,
        msr.bbl_cr_busy,
        msr.bbl_cr_ctl3,
        msr.misc_enable,
    ] {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
    for value in msr.perfctr {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
    for value in msr.evntsel {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
    for value in msr.mtrr_phys {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
    put_u64(bytes, at, msr.mtrr_fix64k_00000);
    at += size_of::<u64>();
    for value in msr.mtrr_fix16k {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
    for value in msr.mtrr_fix4k {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
    for value in [msr.mtrr_def_type, msr.pat, msr.mcg_ctl] {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
    for value in msr.mca_ctl {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
    for value in [msr.sysenter_cs, msr.sysenter_esp, msr.sysenter_eip] {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
    debug_assert_eq!(at - offset, size_of::<CpuMsrStateV1>());
}

fn write_fpu_state(bytes: &mut [u8], offset: usize, fpu: &FpuStateV1) {
    let mut at = offset;
    put_u16(bytes, at, fpu.control);
    at += size_of::<u16>();
    put_u16(bytes, at, fpu.status);
    at += size_of::<u16>();
    put_u8(bytes, at, fpu.top);
    at += size_of::<u8>();
    put_u8(bytes, at, fpu.mmx_active);
    at += size_of::<u8>();
    put_bytes(bytes, at, &fpu.reserved0);
    at += fpu.reserved0.len();
    put_u32(bytes, at, fpu.mxcsr);
    at += size_of::<u32>();
    put_u32(bytes, at, fpu.reserved1);
    at += size_of::<u32>();
    for value in fpu.st_bits {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
    for value in fpu.mm {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
    debug_assert_eq!(at - offset, size_of::<FpuStateV1>());
}

fn write_monitor_request(bytes: &mut [u8], offset: usize, request: &MonitorRequestV1) {
    let mut at = offset;
    put_u32(bytes, at, request.kind);
    at += size_of::<u32>();
    put_u32(bytes, at, request.size);
    at += size_of::<u32>();
    put_u32(bytes, at, request.seq);
    at += size_of::<u32>();
    put_u32(bytes, at, request.flags);
    at += size_of::<u32>();
    for value in [request.a, request.b, request.c, request.d] {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
}

fn write_host_response(bytes: &mut [u8], offset: usize, response: &HostResponseV1) {
    let mut at = offset;
    put_u32(bytes, at, response.status);
    at += size_of::<u32>();
    put_u32(bytes, at, response.size);
    at += size_of::<u32>();
    put_u32(bytes, at, response.seq);
    at += size_of::<u32>();
    put_u32(bytes, at, response.reserved);
    at += size_of::<u32>();
    for value in [response.value, response.a, response.b, response.c] {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
}

fn write_host_command(bytes: &mut [u8], offset: usize, command: &HostCommandV1) {
    let mut at = offset;
    put_u32(bytes, at, command.kind);
    at += size_of::<u32>();
    put_u32(bytes, at, command.size);
    at += size_of::<u32>();
    put_u32(bytes, at, command.seq);
    at += size_of::<u32>();
    put_u32(bytes, at, command.flags);
    at += size_of::<u32>();
    for value in [command.a, command.b, command.c, command.d] {
        put_u64(bytes, at, value);
        at += size_of::<u64>();
    }
}

fn align_up_usize(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

fn put_u8(bytes: &mut [u8], offset: usize, value: u8) {
    bytes[offset] = value;
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    put_bytes(bytes, offset, &value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    put_bytes(bytes, offset, &value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    put_bytes(bytes, offset, &value.to_le_bytes());
}

fn put_bytes(bytes: &mut [u8], offset: usize, data: &[u8]) {
    let end = offset + data.len();
    bytes[offset..end].copy_from_slice(data);
}

fn bootlocal_overlay_files(command: Option<&str>) -> Result<Vec<InitrdFile>> {
    let Some(command) = command else {
        return Ok(Vec::new());
    };
    Ok(vec![
        InitrdFile::new("opt/bootlocal.sh", 0o755, bootlocal_script(command))
            .map_err(KvmError::BadArgument)?,
        InitrdFile::new("opt/bootsync.sh", 0o755, bootsync_script())
            .map_err(KvmError::BadArgument)?,
    ])
}

fn bootlocal_script(command: &str) -> Vec<u8> {
    let mut script = String::from("#!/bin/sh\n{\n");
    script.push_str(command);
    if !script.ends_with('\n') {
        script.push('\n');
    }
    script.push_str("} >/dev/ttyS0 2>&1\n");
    script.into_bytes()
}

fn bootsync_script() -> Vec<u8> {
    b"#!/bin/sh\n/usr/bin/sethostname box\n/opt/bootlocal.sh\n".to_vec()
}

fn required_slot(
    plan: &crate::kvm::memslot::MemslotPlan,
    kind: MemslotKind,
) -> Result<&PlannedMemslot> {
    plan.slots()
        .iter()
        .find(|slot| slot.kind == kind)
        .ok_or(KvmError::MemslotPlan("required boot memslot kind missing"))
}

fn ensure_slot_covers(slot: &PlannedMemslot, required_size: u64) -> Result<()> {
    if slot.memory_size < required_size {
        return Err(KvmError::MemslotPlan("boot control slot is too small"));
    }
    Ok(())
}

fn backing_for_slot_mut(backing: &mut [SlotBacking], slot: u32) -> Result<&mut SlotBacking> {
    backing
        .iter_mut()
        .find(|backing| backing.slot == slot)
        .ok_or(KvmError::MemslotPlan("memslot backing missing"))
}

#[cfg(test)]
fn backing_for_kind<'a>(
    prepared: &'a PreparedHardMmuBoot,
    kind: MemslotKind,
) -> Result<(&'a PlannedMemslot, &'a SlotBacking)> {
    let slot = required_slot(&prepared.planned.memslots, kind)?;
    let backing = prepared
        .backing
        .iter()
        .find(|backing| backing.slot == slot.slot)
        .ok_or(KvmError::MemslotPlan("memslot backing missing"))?;
    Ok((slot, backing))
}

impl core::fmt::Display for PreparedHardMmuBoot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "r86-kvm.tinycore_boot_plan=ok")?;
        writeln!(
            f,
            "boot.kernel_payload_len={}",
            self.loaded.kernel_payload_len
        )?;
        writeln!(f, "boot.initrd_len={}", self.loaded.initrd_len)?;
        writeln!(f, "boot.entry=0x{:x}", self.loaded.entry)?;
        writeln!(f, "boot.params=0x{:x}", self.loaded.boot_params)?;
        writeln!(f, "boot.initrd_addr=0x{:x}", self.loaded.initrd_load_addr)?;
        writeln!(f, "boot.block_ipa=0x{:x}", self.boot_block_ipa)?;
        writeln!(f, "boot.cpu_state_ipa=0x{:x}", self.cpu_state_ipa)?;
        writeln!(f, "boot.mailbox_ipa=0x{:x}", self.mailbox_ipa)?;
        writeln!(
            f,
            "boot.debug_state_ipa=0x{:x}",
            self.planned.layout.shared_base + self.boot_block.debug_state_offset()
        )?;
        writeln!(
            f,
            "boot.panic_block_ipa=0x{:x}",
            self.planned.layout.shared_base + self.boot_block.panic_block_offset()
        )?;
        writeln!(f, "boot.trace_ipa=0x{:x}", self.trace_ipa)?;
        writeln!(
            f,
            "boot.timer_poll_interval={}",
            self.boot_block.timer_poll_interval
        )?;
        writeln!(f, "boot.step_limit={}", self.boot_block.step_limit)?;
        writeln!(f, "boot.trace_mode={}", self.boot_block.trace_mode)?;
        writeln!(f, "memslots.count={}", self.planned.memslots.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kvm::probe::ProbeConfig;
    use crate::linux_boot::{BOOT_PARAMS_ADDR, CMDLINE_ADDR, KERNEL_LOAD_ADDR};
    use crate::monitor::abi::RegionKindV1;
    use std::path::Path;

    #[test]
    fn machine_boot_reload_restores_original_assets_and_clears_guest_ram() {
        let probe = ProbeConfig {
            machine_monitor: true,
            ram_size: 64 * 1024 * 1024,
            ..ProbeConfig::default()
        };
        let config = DirectBootConfig {
            host_decompress_initrd: false,
            bootlocal_command: Some("echo RESET_OK".into()),
            ..DirectBootConfig::default()
        };
        let mut assets = synthetic_assets();
        let mut prepared = prepare_direct_linux_boot(&probe, &assets, &config).unwrap();
        assets.kernel.fill(0xcc);
        assets.initrd.fill(0xdd);
        let slot = required_slot(&prepared.planned.memslots, MemslotKind::X86Ram)
            .unwrap()
            .slot;
        let original = backing_for_slot_mut(&mut prepared.backing, slot)
            .unwrap()
            .memory
            .as_slice()
            .to_vec();
        for _ in 0..3 {
            backing_for_slot_mut(&mut prepared.backing, slot)
                .unwrap()
                .memory
                .fill(0xa5);
            reload_machine_boot(
                &prepared.planned,
                &mut prepared.backing,
                prepared.reboot_assets.as_ref().unwrap(),
                &config,
            )
            .unwrap();
            assert_eq!(
                backing_for_slot_mut(&mut prepared.backing, slot)
                    .unwrap()
                    .memory
                    .as_slice(),
                original
            );
        }
        assert_ne!(
            prepared.boot_block.feature_bits & crate::monitor::abi::FEATURE_MACHINE_BOOT_RELOAD,
            0
        );
    }

    #[test]
    fn bootlocal_overlay_matches_tinycore_boot_hook_paths() {
        let files = bootlocal_overlay_files(Some("echo ok")).unwrap();

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].guest_path, "opt/bootlocal.sh");
        assert!(files[0].data.starts_with(b"#!/bin/sh\n"));
        assert!(files[0].data.ends_with(b"} >/dev/ttyS0 2>&1\n"));
        assert_eq!(files[1].guest_path, "opt/bootsync.sh");
    }

    #[test]
    fn synthetic_direct_boot_is_scattered_into_kvm_ram_and_control_pages() {
        let mut probe = ProbeConfig::default();
        probe.ram_size = 64 * 1024 * 1024;
        let config = DirectBootConfig {
            host_decompress_initrd: false,
            cmdline: "console=ttyS0 testflag".to_string(),
            step_limit: 1234,
            timer_poll_interval: 55,
            trace_mode: TraceModeV1::Architectural,
            ..DirectBootConfig::default()
        };
        let prepared = prepare_direct_linux_boot(&probe, &synthetic_assets(), &config).unwrap();
        let (_low_slot, low) = backing_for_kind(&prepared, MemslotKind::X86Ram).unwrap();
        let shared = backing_for_kind(&prepared, MemslotKind::Shared).unwrap().1;

        assert_eq!(prepared.loaded.entry, KERNEL_LOAD_ADDR);
        assert_eq!(prepared.cpu_state.eip, KERNEL_LOAD_ADDR);
        assert_eq!(prepared.cpu_state.gpr[6], BOOT_PARAMS_ADDR);
        assert_eq!(
            low.memory.as_slice()[BOOT_PARAMS_ADDR as usize + 0x228],
            CMDLINE_ADDR as u8
        );
        assert!(
            low.memory
                .as_slice()
                .windows(b"console=ttyS0 testflag".len())
                .any(|window| window == b"console=ttyS0 testflag")
        );

        let boot_size = core::mem::size_of::<BootBlockV1>();
        let boot_start = SHARED_BOOT_BLOCK_OFFSET as usize;
        let boot_bytes = &shared.memory.as_slice()[boot_start..boot_start + boot_size];
        assert_eq!(
            &boot_bytes[..4],
            &crate::monitor::abi::ABI_MAGIC.to_le_bytes()
        );
        assert_eq!(
            prepared.boot_block.initial_cpu_state_offset,
            SHARED_CPU_STATE_OFFSET
        );
        assert_eq!(prepared.boot_block.mailbox_offset, SHARED_MAILBOX_OFFSET);
        assert_eq!(
            prepared.boot_block.debug_state_offset(),
            SHARED_DEBUG_STATE_OFFSET
        );
        assert_eq!(
            prepared.boot_block.panic_block_offset(),
            SHARED_PANIC_BLOCK_OFFSET
        );
        assert_ne!(
            prepared.boot_block.feature_bits & crate::monitor::abi::FEATURE_DEBUG_STATE_BLOCK,
            0
        );
        assert_ne!(
            prepared.boot_block.feature_bits & crate::monitor::abi::FEATURE_PANIC_BLOCK,
            0
        );
        assert_eq!(prepared.boot_block.step_limit, 1234);
        assert_eq!(prepared.boot_block.timer_poll_interval, 55);
        assert_eq!(
            prepared.boot_block.trace_mode,
            TraceModeV1::Architectural as u32
        );
        assert_eq!(
            prepared.boot_block.regions.entries[1].kind,
            RegionKindV1::Mmio as u32
        );

        let cpu_start = SHARED_CPU_STATE_OFFSET as usize;
        let cpu_bytes = shared.memory.as_slice();
        assert_eq!(read_u32(cpu_bytes, cpu_start + 16), 0x0009_0000);
        assert_eq!(read_u32(cpu_bytes, cpu_start + 24), BOOT_PARAMS_ADDR);
        assert_eq!(read_u32(cpu_bytes, cpu_start + 32), KERNEL_LOAD_ADDR);
        assert_eq!(read_u32(cpu_bytes, cpu_start + 40), 1);
        assert_eq!(read_u16(cpu_bytes, cpu_start + 72), 0x10);
        assert_eq!(read_u16(cpu_bytes, cpu_start + 74), 0xc09a);

        let mailbox_start = SHARED_MAILBOX_OFFSET as usize;
        assert_eq!(
            read_u16(cpu_bytes, mailbox_start),
            crate::monitor::abi::ABI_VERSION
        );
        assert_eq!(
            read_u16(cpu_bytes, mailbox_start + 2),
            core::mem::size_of::<MailboxV1>() as u16
        );
    }

    #[test]
    fn custom_initrd_overlay_files_are_loaded_into_kvm_ram() {
        let mut probe = ProbeConfig::default();
        probe.ram_size = 64 * 1024 * 1024;
        let config = DirectBootConfig {
            initrd_files: vec![
                InitrdFile::new("tmp/r86-extra", 0o644, b"R86_EXTRA_OVERLAY".to_vec()).unwrap(),
            ],
            ..DirectBootConfig::default()
        };

        let mut assets = synthetic_assets();
        assets.initrd = empty_gzip_initrd();
        let prepared = prepare_direct_linux_boot(&probe, &assets, &config).unwrap();
        let start = prepared.loaded.initrd_load_addr as usize;
        let initrd = ram_slice(&prepared, start as u64, prepared.loaded.initrd_len).unwrap();

        assert!(
            initrd
                .windows(b"R86_EXTRA_OVERLAY".len())
                .any(|window| window == b"R86_EXTRA_OVERLAY")
        );
    }

    #[test]
    fn tinycore_iso_assets_are_reusable_by_kvm_loader() {
        let path = Path::new(DEFAULT_TINYCORE_ISO);
        if !path.exists() {
            return;
        }
        let iso = fs::read(path).unwrap();
        let assets = tinycore_assets_from_iso(&iso).unwrap();

        assert_eq!(assets.kernel.len(), 6_087_168);
        assert_eq!(assets.initrd.len(), 13_952_288);
    }

    fn synthetic_assets() -> DirectBootAssets {
        let mut kernel = vec![0_u8; 0x5000];
        kernel[0x1f1] = 31;
        kernel[0x202..0x206].copy_from_slice(b"HdrS");
        kernel[0x206..0x208].copy_from_slice(&0x020f_u16.to_le_bytes());
        kernel[0x4000..0x5000].fill(0x5a);
        DirectBootAssets {
            kernel,
            initrd: vec![0xaa; 4096],
        }
    }

    fn empty_gzip_initrd() -> Vec<u8> {
        vec![
            0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, 0, 0x03, 0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]
    }

    fn ram_slice(prepared: &PreparedHardMmuBoot, gpa: u64, len: usize) -> Result<&[u8]> {
        let gpa = prepared.planned.layout.ram_base + gpa;
        let end = gpa
            .checked_add(len as u64)
            .ok_or(KvmError::MemslotPlan("test RAM slice overflows"))?;
        let slot = prepared
            .planned
            .memslots
            .slots()
            .iter()
            .find(|slot| {
                slot.kind == MemslotKind::X86Ram
                    && gpa >= slot.guest_phys_addr
                    && end <= slot.end_exclusive().unwrap_or(0)
            })
            .ok_or(KvmError::MemslotPlan("test RAM slice has no backing slot"))?;
        let backing = prepared
            .backing
            .iter()
            .find(|backing| backing.slot == slot.slot)
            .ok_or(KvmError::MemslotPlan("memslot backing missing"))?;
        let offset = (gpa - slot.guest_phys_addr) as usize;
        Ok(&backing.memory.as_slice()[offset..offset + len])
    }

    fn read_u16(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
    }

    fn read_u32(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }
}
