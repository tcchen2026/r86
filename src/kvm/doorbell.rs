use crate::monitor::abi::{DOORBELL_IPA, X86_RAM_IPA_BASE};
use std::time::Duration;

use crate::kvm::backing::SlotBacking;
use crate::kvm::error::{KvmError, Result};
use crate::kvm::kvm::{Kvm, RunMap};
use crate::kvm::memslot::{MemslotKind, MemslotPlan, PlannedMemslot};
use crate::kvm::probe::{PlannedHardMmu, ProbeConfig, choose_ipa_bits};
use crate::kvm::sys;

pub const DOORBELL_MAGIC: u32 = 0x5243_3836;
pub const DOORBELL_CODE_OFFSET: u64 = 0;
pub const DOORBELL_TABLE_OFFSET: u64 = 0x1000;
pub const DOORBELL_STACK_OFFSET: u64 = 0x8000;
pub const DOORBELL_PROGRAM_LEN: usize = 64;
pub const DOORBELL_STAGE1_PROGRAM_LEN: usize = 104;
pub const L1_TABLE_SIZE: usize = 4096;
const DOORBELL_RUN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoorbellSmokeReport {
    pub api_version: i32,
    pub arm_ipa_bits_cap: i32,
    pub chosen_ipa_bits: u8,
    pub vcpu_mmap_size: i32,
    pub vcpu_target: u32,
    pub entry_ipa: u64,
    pub stack_ipa: u64,
    pub exit_reason: u32,
    pub mmio_ipa: u64,
    pub mmio_len: u32,
    pub mmio_is_write: bool,
    pub mmio_value: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoorbellStateReport {
    pub api_version: i32,
    pub arm_ipa_bits_cap: i32,
    pub chosen_ipa_bits: u8,
    pub vcpu_target: u32,
    pub entry_ipa: u64,
    pub stack_ipa: u64,
    pub readback_pc: u64,
    pub readback_sp_el1: u64,
    pub readback_pstate: u64,
    pub init_features: [u32; 7],
}

pub fn run_doorbell_state_probe(config: &ProbeConfig) -> Result<DoorbellStateReport> {
    let planned = config.validate_for_layout_only()?;
    let kvm = Kvm::open()?;
    let api_version = kvm.require_api_version()?;
    let arm_ipa_bits_cap =
        kvm.require_extension(sys::KVM_CAP_ARM_VM_IPA_SIZE, "KVM_CAP_ARM_VM_IPA_SIZE")?;
    let chosen_ipa_bits = choose_ipa_bits(
        planned.required_ipa_bits,
        arm_ipa_bits_cap,
        config.requested_ipa_bits,
    )?;
    let vm = kvm.create_arm_vm(chosen_ipa_bits)?;
    let mut backing = planned.memslots.allocate_backing()?;
    let entry_ipa = load_doorbell_program(&planned, &mut backing)?;
    let stack_ipa = monitor_stack_ipa(&planned)?;
    let mut vcpu_init = vm.arm_preferred_target()?;
    vcpu_init.features[0] &= !(1_u32 << sys::KVM_ARM_VCPU_POWER_OFF);
    let _registered = vm.register_backing(&planned.memslots, backing)?;
    let vcpu = vm.create_vcpu(0)?;
    vcpu.arm_init(&vcpu_init)?;
    vcpu.set_arm_entry_state(entry_ipa, stack_ipa)?;

    Ok(DoorbellStateReport {
        api_version,
        arm_ipa_bits_cap,
        chosen_ipa_bits,
        vcpu_target: vcpu_init.target,
        entry_ipa,
        stack_ipa,
        readback_pc: vcpu.get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PC))?,
        readback_sp_el1: vcpu
            .get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_SP_EL1))?,
        readback_pstate: vcpu
            .get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PSTATE))?,
        init_features: vcpu_init.features,
    })
}

pub fn run_doorbell_smoke(config: &ProbeConfig) -> Result<DoorbellSmokeReport> {
    let planned = config.validate_for_layout_only()?;
    let kvm = Kvm::open()?;
    let api_version = kvm.require_api_version()?;
    let arm_ipa_bits_cap =
        kvm.require_extension(sys::KVM_CAP_ARM_VM_IPA_SIZE, "KVM_CAP_ARM_VM_IPA_SIZE")?;
    let chosen_ipa_bits = choose_ipa_bits(
        planned.required_ipa_bits,
        arm_ipa_bits_cap,
        config.requested_ipa_bits,
    )?;
    planned.layout.validate(chosen_ipa_bits)?;
    let vcpu_mmap_size = usize::try_from(kvm.vcpu_mmap_size()?)
        .map_err(|_| KvmError::UnsupportedHost("negative vcpu mmap size"))?;

    let vm = kvm.create_arm_vm(chosen_ipa_bits)?;
    let mut backing = planned.memslots.allocate_backing()?;
    let entry_ipa = load_doorbell_program(&planned, &mut backing)?;
    let stack_ipa = monitor_stack_ipa(&planned)?;
    let mut vcpu_init = vm.arm_preferred_target()?;
    vcpu_init.features[0] &= !(1_u32 << sys::KVM_ARM_VCPU_POWER_OFF);
    let registered = vm.register_backing(&planned.memslots, backing)?;
    let vcpu = vm.create_vcpu(0)?;
    vcpu.arm_init(&vcpu_init)?;
    vcpu.set_arm_entry_state(entry_ipa, stack_ipa)?;
    let run_map = vcpu.map_run(vcpu_mmap_size)?;
    if !vcpu.run_with_timeout(&run_map, DOORBELL_RUN_TIMEOUT)? {
        let pc = vcpu.get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PC))?;
        let pstate = vcpu.get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PSTATE))?;
        return Err(KvmError::BadArgument(format!(
            "KVM_RUN timed out before doorbell exit pc=0x{pc:x} pstate=0x{pstate:x}"
        )));
    }

    let mmio = run_map
        .mmio()
        .ok_or_else(|| unexpected_exit_error(&run_map))?;
    let value = u32::from_le_bytes(mmio.data[..4].try_into().unwrap());
    if mmio.phys_addr != DOORBELL_IPA
        || mmio.len != 4
        || mmio.is_write == 0
        || value != DOORBELL_MAGIC
    {
        return Err(KvmError::BadArgument(format!(
            "unexpected doorbell MMIO ipa=0x{:x} len={} write={} value=0x{:08x}",
            mmio.phys_addr, mmio.len, mmio.is_write, value
        )));
    }

    // KVM completes the store and advances PC when KVM_RUN resumes. The ADD
    // immediately following the store must execute exactly once.
    if !vcpu.run_with_timeout(&run_map, DOORBELL_RUN_TIMEOUT)? {
        return Err(KvmError::BadArgument("doorbell resume timed out".into()));
    }
    let resumed = run_map
        .mmio()
        .ok_or_else(|| unexpected_exit_error(&run_map))?;
    let resumed_value = u32::from_le_bytes(resumed.data[..4].try_into().unwrap());
    if resumed.phys_addr != DOORBELL_IPA
        || resumed.len != 4
        || resumed.is_write == 0
        || resumed_value != DOORBELL_MAGIC + 1
    {
        return Err(KvmError::BadArgument(format!(
            "doorbell resume skipped or repeated an instruction: {resumed:?}"
        )));
    }

    let ram_slot = find_slot(&planned.memslots, MemslotKind::X86Ram)?;
    let ram_backing = registered
        .backing()
        .iter()
        .find(|b| b.slot == ram_slot.slot)
        .unwrap();
    let ram = &ram_backing.memory.as_slice()[0x1000..0x1004];
    if ram != DOORBELL_MAGIC.to_le_bytes() {
        return Err(KvmError::BadArgument(format!(
            "guest RAM store did not reach host backing: {ram:02x?}"
        )));
    }
    Ok(DoorbellSmokeReport {
        api_version,
        arm_ipa_bits_cap,
        chosen_ipa_bits,
        vcpu_mmap_size: i32::try_from(vcpu_mmap_size).unwrap_or(i32::MAX),
        vcpu_target: vcpu_init.target,
        entry_ipa,
        stack_ipa,
        exit_reason: sys::KVM_EXIT_MMIO,
        mmio_ipa: mmio.phys_addr,
        mmio_len: mmio.len,
        mmio_is_write: mmio.is_write != 0,
        mmio_value: value,
    })
}

pub fn doorbell_program_bytes() -> [u8; DOORBELL_PROGRAM_LEN] {
    let mut out = [0_u8; DOORBELL_PROGRAM_LEN];
    write_u32(&mut out, 0x00, 0x5800_0181); // ldr x1, doorbell literal
    write_u32(&mut out, 0x04, 0x5287_06c0);
    write_u32(&mut out, 0x08, 0x72aa_4860);
    write_u32(&mut out, 0x0c, 0x5800_0162); // ldr x2, RAM literal
    write_u32(&mut out, 0x10, 0xb900_0040); // str w0, [x2]
    write_u32(&mut out, 0x14, 0x5280_0000); // mov w0, #0
    write_u32(&mut out, 0x18, 0xb940_0040); // ldr w0, [x2]
    write_u32(&mut out, 0x1c, 0xb900_0020); // str w0, [x1]
    write_u32(&mut out, 0x20, 0x1100_0400); // add w0, w0, #1
    write_u32(&mut out, 0x24, 0xb900_0020); // str w0, [x1]
    write_u32(&mut out, 0x28, 0xd503_207f);
    write_u32(&mut out, 0x2c, 0x17ff_ffff);
    out[0x30..0x38].copy_from_slice(&DOORBELL_IPA.to_le_bytes());
    out[0x38..0x40].copy_from_slice(&(X86_RAM_IPA_BASE + 0x1000).to_le_bytes());
    out
}

pub fn doorbell_stage1_program_bytes(ttbr0_ipa: u64) -> [u8; DOORBELL_STAGE1_PROGRAM_LEN] {
    let mut out = [0_u8; DOORBELL_STAGE1_PROGRAM_LEN];
    write_u32(&mut out, 0x00, 0x5800_0240);
    write_u32(&mut out, 0x04, 0xd518_a200);
    write_u32(&mut out, 0x08, 0x5800_0240);
    write_u32(&mut out, 0x0c, 0xd518_2040);
    write_u32(&mut out, 0x10, 0x5800_0240);
    write_u32(&mut out, 0x14, 0xd518_2000);
    write_u32(&mut out, 0x18, 0xd503_3b9f);
    write_u32(&mut out, 0x1c, 0xd503_3fdf);
    write_u32(&mut out, 0x20, 0xd538_1000);
    write_u32(&mut out, 0x24, 0xb240_0000);
    write_u32(&mut out, 0x28, 0xd518_1000);
    write_u32(&mut out, 0x2c, 0xd503_3fdf);
    write_u32(&mut out, 0x30, 0x5800_0181);
    write_u32(&mut out, 0x34, 0x5287_06c0);
    write_u32(&mut out, 0x38, 0x72aa_4860);
    write_u32(&mut out, 0x3c, 0xb900_0020);
    write_u32(&mut out, 0x40, 0xd503_207f);
    write_u32(&mut out, 0x44, 0xd503_201f);
    out[0x48..0x50].copy_from_slice(&0x0000_0000_0000_04ff_u64.to_le_bytes());
    out[0x50..0x58].copy_from_slice(&0x0000_0002_0000_351e_u64.to_le_bytes());
    out[0x58..0x60].copy_from_slice(&ttbr0_ipa.to_le_bytes());
    out[0x60..0x68].copy_from_slice(&DOORBELL_IPA.to_le_bytes());
    out
}

pub fn stage1_l1_table_bytes() -> [u8; L1_TABLE_SIZE] {
    let mut table = [0_u8; L1_TABLE_SIZE];
    for index in 0..16_u64 {
        let base = index << 30;
        let entry = if index == (DOORBELL_IPA >> 30) {
            block_descriptor(base, 1, false, true)
        } else {
            block_descriptor(base, 0, true, false)
        };
        table[index as usize * 8..index as usize * 8 + 8].copy_from_slice(&entry.to_le_bytes());
    }
    table
}

fn block_descriptor(output_addr: u64, attr_index: u64, inner_shareable: bool, xn: bool) -> u64 {
    let mut entry = (output_addr & 0x0000_ffff_c000_0000) | 1;
    entry |= (attr_index & 0x7) << 2;
    if inner_shareable {
        entry |= 0x3 << 8;
    }
    entry |= 1 << 10;
    if xn {
        entry |= (1_u64 << 53) | (1_u64 << 54);
    }
    entry
}

fn write_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn load_doorbell_program(planned: &PlannedHardMmu, backing: &mut [SlotBacking]) -> Result<u64> {
    let monitor = find_slot(&planned.memslots, MemslotKind::Monitor)?;
    let entry_ipa = monitor.guest_phys_addr + DOORBELL_CODE_OFFSET;
    let slot_backing = backing
        .iter_mut()
        .find(|backing| backing.slot == monitor.slot)
        .ok_or(KvmError::MemslotPlan("monitor backing not found"))?;
    let program = doorbell_program_bytes();
    slot_backing.write_at(DOORBELL_CODE_OFFSET, &program)?;
    slot_backing.flush_instruction_cache_at(DOORBELL_CODE_OFFSET, program.len())?;
    Ok(entry_ipa)
}

fn monitor_stack_ipa(planned: &PlannedHardMmu) -> Result<u64> {
    let monitor = find_slot(&planned.memslots, MemslotKind::Monitor)?;
    if monitor.memory_size <= DOORBELL_STACK_OFFSET + 16 {
        return Err(KvmError::MemslotPlan(
            "monitor slot too small for smoke stack",
        ));
    }
    Ok(monitor.guest_phys_addr + DOORBELL_STACK_OFFSET)
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

impl core::fmt::Display for DoorbellSmokeReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "r86-kvm.doorbell_smoke=ok")?;
        writeln!(f, "kvm.api_version={}", self.api_version)?;
        writeln!(f, "kvm.arm_ipa_bits_cap={}", self.arm_ipa_bits_cap)?;
        writeln!(f, "kvm.chosen_ipa_bits={}", self.chosen_ipa_bits)?;
        writeln!(f, "kvm.vcpu_mmap_size={}", self.vcpu_mmap_size)?;
        writeln!(f, "vcpu.target={}", self.vcpu_target)?;
        writeln!(f, "vcpu.entry_ipa=0x{:x}", self.entry_ipa)?;
        writeln!(f, "vcpu.stack_ipa=0x{:x}", self.stack_ipa)?;
        writeln!(f, "exit.reason={}", self.exit_reason)?;
        writeln!(f, "mmio.ipa=0x{:x}", self.mmio_ipa)?;
        writeln!(f, "mmio.len={}", self.mmio_len)?;
        writeln!(f, "mmio.write={}", self.mmio_is_write)?;
        writeln!(f, "mmio.value=0x{:08x}", self.mmio_value)
    }
}

impl core::fmt::Display for DoorbellStateReport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        writeln!(f, "r86-kvm.doorbell_state=ok")?;
        writeln!(f, "kvm.api_version={}", self.api_version)?;
        writeln!(f, "kvm.arm_ipa_bits_cap={}", self.arm_ipa_bits_cap)?;
        writeln!(f, "kvm.chosen_ipa_bits={}", self.chosen_ipa_bits)?;
        writeln!(f, "vcpu.target={}", self.vcpu_target)?;
        writeln!(f, "vcpu.entry_ipa=0x{:x}", self.entry_ipa)?;
        writeln!(f, "vcpu.stack_ipa=0x{:x}", self.stack_ipa)?;
        writeln!(f, "vcpu.readback_pc=0x{:x}", self.readback_pc)?;
        writeln!(f, "vcpu.readback_sp_el1=0x{:x}", self.readback_sp_el1)?;
        writeln!(f, "vcpu.readback_pstate=0x{:x}", self.readback_pstate)?;
        for (index, value) in self.init_features.iter().enumerate() {
            writeln!(f, "vcpu.init_features.{index}=0x{value:x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kvm::probe::ProbeConfig;

    #[test]
    fn stage1_table_maps_doorbell_gib_as_device_xn() {
        let table = stage1_l1_table_bytes();
        let normal = u64::from_le_bytes(table[0..8].try_into().unwrap());
        let doorbell_index = (DOORBELL_IPA >> 30) as usize;
        let doorbell = u64::from_le_bytes(
            table[doorbell_index * 8..doorbell_index * 8 + 8]
                .try_into()
                .unwrap(),
        );

        assert_eq!(normal & 0x705, 0x701);
        assert_eq!(doorbell & 0x405, 0x405);
        assert_ne!(doorbell & ((1_u64 << 53) | (1_u64 << 54)), 0);
    }

    #[test]
    fn doorbell_program_loads_into_monitor_backing_only() {
        let config = ProbeConfig::default();
        let planned = config.validate_for_layout_only().unwrap();
        let mut backing = planned.memslots.allocate_backing().unwrap();
        let entry = load_doorbell_program(&planned, &mut backing).unwrap();
        let monitor = find_slot(&planned.memslots, MemslotKind::Monitor).unwrap();
        let monitor_backing = backing
            .iter()
            .find(|backing| backing.slot == monitor.slot)
            .unwrap();

        assert_eq!(entry, monitor.guest_phys_addr);
        assert_eq!(
            &monitor_backing.memory.as_slice()[..doorbell_program_bytes().len()],
            &doorbell_program_bytes()
        );
        assert_eq!(
            monitor_stack_ipa(&planned).unwrap(),
            monitor.guest_phys_addr + DOORBELL_STACK_OFFSET
        );
    }
}
