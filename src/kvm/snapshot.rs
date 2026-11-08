use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use crate::monitor::abi::{
    CpuMsrStateV1, CpuStateV1, DescriptorTableV1, FpuStateV1, SegmentStateV1,
};

use crate::kvm::backing::SlotBacking;
use crate::kvm::error::{KvmError, Result};
use crate::kvm::memslot::{MemslotKind, MemslotPlan};
use crate::kvm::service::{PcPioBusSnapshot, Uart8250Snapshot};

const SNAPSHOT_MAGIC: u64 = 0x3150_4e53_3638_3652; // "R86SNP1", little-endian.
const SNAPSHOT_VERSION: u32 = 2;
const DEVICE_FIXED_SIZE: u32 = 43;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HardMmuSnapshot {
    pub ram_size: u64,
    pub cpu_state: CpuStateV1,
    pub device: PcPioBusSnapshot,
    pub ram: Vec<u8>,
}

pub fn save_hardmmu_snapshot(path: &Path, snapshot: &HardMmuSnapshot) -> Result<()> {
    if snapshot.ram.len() as u64 != snapshot.ram_size {
        return Err(KvmError::BadArgument(format!(
            "snapshot RAM length {} does not match ram_size {}",
            snapshot.ram.len(),
            snapshot.ram_size
        )));
    }
    let file = File::create(path).map_err(|source| KvmError::Io {
        op: "create HardMMU snapshot",
        source,
    })?;
    let mut writer = BufWriter::new(file);
    write_u64(&mut writer, SNAPSHOT_MAGIC)?;
    write_u32(&mut writer, SNAPSHOT_VERSION)?;
    write_u32(&mut writer, 0)?;
    write_u64(&mut writer, snapshot.ram_size)?;
    write_u32(&mut writer, core::mem::size_of::<CpuStateV1>() as u32)?;
    write_u32(&mut writer, DEVICE_FIXED_SIZE)?;
    write_u64(&mut writer, snapshot.device.serial.output.len() as u64)?;
    write_u64(&mut writer, snapshot.device.serial.input.len() as u64)?;
    write_u64(&mut writer, snapshot.device.vga.len() as u64)?;
    write_u64(&mut writer, snapshot.ram.len() as u64)?;
    write_cpu_state(&mut writer, &snapshot.cpu_state)?;
    write_device_fixed(&mut writer, &snapshot.device)?;
    writer
        .write_all(&snapshot.device.serial.output)
        .map_err(snapshot_write_error)?;
    writer
        .write_all(&snapshot.device.serial.input)
        .map_err(snapshot_write_error)?;
    writer
        .write_all(&snapshot.device.vga)
        .map_err(snapshot_write_error)?;
    writer
        .write_all(&snapshot.ram)
        .map_err(snapshot_write_error)?;
    writer.flush().map_err(snapshot_write_error)?;
    Ok(())
}

pub fn load_hardmmu_snapshot(path: &Path) -> Result<HardMmuSnapshot> {
    let file = File::open(path).map_err(|source| KvmError::Io {
        op: "open HardMMU snapshot",
        source,
    })?;
    let mut reader = BufReader::new(file);
    let magic = read_u64(&mut reader)?;
    if magic != SNAPSHOT_MAGIC {
        return Err(KvmError::BadArgument(format!(
            "bad HardMMU snapshot magic 0x{magic:016x}"
        )));
    }
    let version = read_u32(&mut reader)?;
    if version != SNAPSHOT_VERSION {
        return Err(KvmError::BadArgument(format!(
            "unsupported HardMMU snapshot version {version}"
        )));
    }
    let _flags = read_u32(&mut reader)?;
    let ram_size = read_u64(&mut reader)?;
    let cpu_state_size = read_u32(&mut reader)?;
    if cpu_state_size as usize != core::mem::size_of::<CpuStateV1>() {
        return Err(KvmError::BadArgument(format!(
            "snapshot CPU state size {cpu_state_size} does not match {}",
            core::mem::size_of::<CpuStateV1>()
        )));
    }
    let device_fixed_size = read_u32(&mut reader)?;
    if device_fixed_size != DEVICE_FIXED_SIZE {
        return Err(KvmError::BadArgument(format!(
            "snapshot device fixed size {device_fixed_size} does not match {DEVICE_FIXED_SIZE}"
        )));
    }
    let serial_output_len = checked_len("serial output", read_u64(&mut reader)?)?;
    let serial_input_len = checked_len("serial input", read_u64(&mut reader)?)?;
    let vga_len = checked_len("VGA", read_u64(&mut reader)?)?;
    let ram_len = checked_len("RAM", read_u64(&mut reader)?)?;
    if ram_len as u64 != ram_size {
        return Err(KvmError::BadArgument(format!(
            "snapshot RAM payload length {ram_len} does not match ram_size {ram_size}"
        )));
    }

    let cpu_state = read_cpu_state(&mut reader)?;
    let mut device = read_device_fixed(&mut reader)?;
    device.serial.output = read_vec(&mut reader, serial_output_len)?;
    device.serial.input = read_vec(&mut reader, serial_input_len)?;
    device.vga = read_vec(&mut reader, vga_len)?;
    let ram = read_vec(&mut reader, ram_len)?;
    Ok(HardMmuSnapshot {
        ram_size,
        cpu_state,
        device,
        ram,
    })
}

pub fn collect_x86_ram(
    plan: &MemslotPlan,
    backing: &[SlotBacking],
    ram_size: u64,
) -> Result<Vec<u8>> {
    let mut ram = vec![
        0_u8;
        usize::try_from(ram_size).map_err(|_| KvmError::MemslotPlan(
            "snapshot RAM does not fit host usize"
        ))?
    ];
    for slot in plan
        .slots()
        .iter()
        .filter(|slot| slot.kind == MemslotKind::X86Ram)
    {
        let start = usize::try_from(
            slot.guest_phys_addr
                .checked_sub(crate::monitor::abi::X86_RAM_IPA_BASE)
                .ok_or(KvmError::MemslotPlan("RAM slot precedes RAM IPA base"))?,
        )
        .map_err(|_| KvmError::MemslotPlan("RAM slot base too large"))?;
        let len = usize::try_from(slot.memory_size)
            .map_err(|_| KvmError::MemslotPlan("RAM slot size too large"))?;
        let end = start
            .checked_add(len)
            .ok_or(KvmError::MemslotPlan("RAM slot end overflows"))?;
        let bytes = ram
            .get_mut(start..end)
            .ok_or(KvmError::MemslotPlan("RAM slot exceeds snapshot RAM"))?;
        let slot_backing = backing
            .iter()
            .find(|candidate| candidate.slot == slot.slot)
            .ok_or(KvmError::MemslotPlan("RAM backing slot not found"))?;
        bytes.copy_from_slice(slot_backing.memory.as_slice());
    }
    Ok(ram)
}

pub fn scatter_x86_ram(plan: &MemslotPlan, backing: &mut [SlotBacking], ram: &[u8]) -> Result<()> {
    for slot in plan
        .slots()
        .iter()
        .filter(|slot| slot.kind == MemslotKind::X86Ram)
    {
        let start = usize::try_from(
            slot.guest_phys_addr
                .checked_sub(crate::monitor::abi::X86_RAM_IPA_BASE)
                .ok_or(KvmError::MemslotPlan("RAM slot precedes RAM IPA base"))?,
        )
        .map_err(|_| KvmError::MemslotPlan("RAM slot base too large"))?;
        let len = usize::try_from(slot.memory_size)
            .map_err(|_| KvmError::MemslotPlan("RAM slot size too large"))?;
        let end = start
            .checked_add(len)
            .ok_or(KvmError::MemslotPlan("RAM slot end overflows"))?;
        let bytes = ram.get(start..end).ok_or(KvmError::MemslotPlan(
            "snapshot RAM does not cover RAM slot",
        ))?;
        let slot_backing = backing
            .iter_mut()
            .find(|candidate| candidate.slot == slot.slot)
            .ok_or(KvmError::MemslotPlan("RAM backing slot not found"))?;
        slot_backing.write_at(0, bytes)?;
    }
    Ok(())
}

fn write_cpu_state<W: Write>(writer: &mut W, state: &CpuStateV1) -> Result<()> {
    for value in state.gpr {
        write_u32(writer, value)?;
    }
    write_u32(writer, state.eip)?;
    write_u32(writer, state.eflags)?;
    write_u32(writer, state.cr0)?;
    write_u32(writer, state.cr2)?;
    write_u32(writer, state.cr3)?;
    write_u32(writer, state.cr4)?;
    write_u8(writer, state.cpl)?;
    write_u8(writer, state.halted)?;
    write_u8(writer, state.interrupt_shadow)?;
    write_u8(writer, state.reserved0)?;
    for segment in state.seg {
        write_segment(writer, segment)?;
    }
    write_descriptor_table(writer, state.gdtr)?;
    write_descriptor_table(writer, state.idtr)?;
    write_segment(writer, state.ldtr)?;
    write_segment(writer, state.tr)?;
    write_u64(writer, state.tsc)?;
    write_u64(writer, state.instr_count)?;
    writer
        .write_all(&state.fxsave)
        .map_err(snapshot_write_error)?;
    for value in state.pae_pdptrs {
        write_u64(writer, value)?;
    }
    for value in state.dr {
        write_u32(writer, value)?;
    }
    write_cpu_msr_state(writer, state.msr)?;
    write_fpu_state(writer, state.fpu)?;
    Ok(())
}

fn read_cpu_state<R: Read>(reader: &mut R) -> Result<CpuStateV1> {
    let mut state = CpuStateV1::ZERO;
    for value in &mut state.gpr {
        *value = read_u32(reader)?;
    }
    state.eip = read_u32(reader)?;
    state.eflags = read_u32(reader)?;
    state.cr0 = read_u32(reader)?;
    state.cr2 = read_u32(reader)?;
    state.cr3 = read_u32(reader)?;
    state.cr4 = read_u32(reader)?;
    state.cpl = read_u8(reader)?;
    state.halted = read_u8(reader)?;
    state.interrupt_shadow = read_u8(reader)?;
    state.reserved0 = read_u8(reader)?;
    for segment in &mut state.seg {
        *segment = read_segment(reader)?;
    }
    state.gdtr = read_descriptor_table(reader)?;
    state.idtr = read_descriptor_table(reader)?;
    state.ldtr = read_segment(reader)?;
    state.tr = read_segment(reader)?;
    state.tsc = read_u64(reader)?;
    state.instr_count = read_u64(reader)?;
    reader
        .read_exact(&mut state.fxsave)
        .map_err(snapshot_read_error)?;
    for value in &mut state.pae_pdptrs {
        *value = read_u64(reader)?;
    }
    for value in &mut state.dr {
        *value = read_u32(reader)?;
    }
    state.msr = read_cpu_msr_state(reader)?;
    state.fpu = read_fpu_state(reader)?;
    Ok(state)
}

fn write_segment<W: Write>(writer: &mut W, segment: SegmentStateV1) -> Result<()> {
    write_u16(writer, segment.selector)?;
    write_u16(writer, segment.attr)?;
    write_u32(writer, segment.limit)?;
    write_u32(writer, segment.base)
}

fn read_segment<R: Read>(reader: &mut R) -> Result<SegmentStateV1> {
    Ok(SegmentStateV1 {
        selector: read_u16(reader)?,
        attr: read_u16(reader)?,
        limit: read_u32(reader)?,
        base: read_u32(reader)?,
    })
}

fn write_descriptor_table<W: Write>(writer: &mut W, table: DescriptorTableV1) -> Result<()> {
    write_u64(writer, table.base)?;
    write_u16(writer, table.limit)?;
    for value in table.reserved {
        write_u16(writer, value)?;
    }
    Ok(())
}

fn read_descriptor_table<R: Read>(reader: &mut R) -> Result<DescriptorTableV1> {
    let mut table = DescriptorTableV1::ZERO;
    table.base = read_u64(reader)?;
    table.limit = read_u16(reader)?;
    for value in &mut table.reserved {
        *value = read_u16(reader)?;
    }
    Ok(table)
}

fn write_cpu_msr_state<W: Write>(writer: &mut W, msr: CpuMsrStateV1) -> Result<()> {
    write_u64(writer, msr.apic_base)?;
    write_u32(writer, msr.test_ctl)?;
    write_u32(writer, msr.reserved0)?;
    write_u64(writer, msr.bios_update_trigger)?;
    for value in msr.bbl_cr_data {
        write_u64(writer, value)?;
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
        write_u64(writer, value)?;
    }
    for value in msr.perfctr {
        write_u64(writer, value)?;
    }
    for value in msr.evntsel {
        write_u64(writer, value)?;
    }
    for value in msr.mtrr_phys {
        write_u64(writer, value)?;
    }
    write_u64(writer, msr.mtrr_fix64k_00000)?;
    for value in msr.mtrr_fix16k {
        write_u64(writer, value)?;
    }
    for value in msr.mtrr_fix4k {
        write_u64(writer, value)?;
    }
    write_u64(writer, msr.mtrr_def_type)?;
    write_u64(writer, msr.pat)?;
    write_u64(writer, msr.mcg_ctl)?;
    for value in msr.mca_ctl {
        write_u64(writer, value)?;
    }
    write_u64(writer, msr.sysenter_cs)?;
    write_u64(writer, msr.sysenter_esp)?;
    write_u64(writer, msr.sysenter_eip)
}

fn read_cpu_msr_state<R: Read>(reader: &mut R) -> Result<CpuMsrStateV1> {
    let mut msr = CpuMsrStateV1::RESET;
    msr.apic_base = read_u64(reader)?;
    msr.test_ctl = read_u32(reader)?;
    msr.reserved0 = read_u32(reader)?;
    msr.bios_update_trigger = read_u64(reader)?;
    for value in &mut msr.bbl_cr_data {
        *value = read_u64(reader)?;
    }
    msr.bbl_cr_addr = read_u64(reader)?;
    msr.bbl_cr_decc = read_u64(reader)?;
    msr.bbl_cr_ctl = read_u64(reader)?;
    msr.bbl_cr_trig = read_u64(reader)?;
    msr.bbl_cr_busy = read_u64(reader)?;
    msr.bbl_cr_ctl3 = read_u64(reader)?;
    msr.misc_enable = read_u64(reader)?;
    for value in &mut msr.perfctr {
        *value = read_u64(reader)?;
    }
    for value in &mut msr.evntsel {
        *value = read_u64(reader)?;
    }
    for value in &mut msr.mtrr_phys {
        *value = read_u64(reader)?;
    }
    msr.mtrr_fix64k_00000 = read_u64(reader)?;
    for value in &mut msr.mtrr_fix16k {
        *value = read_u64(reader)?;
    }
    for value in &mut msr.mtrr_fix4k {
        *value = read_u64(reader)?;
    }
    msr.mtrr_def_type = read_u64(reader)?;
    msr.pat = read_u64(reader)?;
    msr.mcg_ctl = read_u64(reader)?;
    for value in &mut msr.mca_ctl {
        *value = read_u64(reader)?;
    }
    msr.sysenter_cs = read_u64(reader)?;
    msr.sysenter_esp = read_u64(reader)?;
    msr.sysenter_eip = read_u64(reader)?;
    Ok(msr)
}

fn write_fpu_state<W: Write>(writer: &mut W, fpu: FpuStateV1) -> Result<()> {
    write_u16(writer, fpu.control)?;
    write_u16(writer, fpu.status)?;
    write_u8(writer, fpu.top)?;
    write_u8(writer, fpu.mmx_active)?;
    writer
        .write_all(&fpu.reserved0)
        .map_err(snapshot_write_error)?;
    write_u32(writer, fpu.mxcsr)?;
    write_u32(writer, fpu.reserved1)?;
    for value in fpu.st_bits {
        write_u64(writer, value)?;
    }
    for value in fpu.mm {
        write_u64(writer, value)?;
    }
    Ok(())
}

fn read_fpu_state<R: Read>(reader: &mut R) -> Result<FpuStateV1> {
    let mut fpu = FpuStateV1::RESET;
    fpu.control = read_u16(reader)?;
    fpu.status = read_u16(reader)?;
    fpu.top = read_u8(reader)?;
    fpu.mmx_active = read_u8(reader)?;
    reader
        .read_exact(&mut fpu.reserved0)
        .map_err(snapshot_read_error)?;
    fpu.mxcsr = read_u32(reader)?;
    fpu.reserved1 = read_u32(reader)?;
    for value in &mut fpu.st_bits {
        *value = read_u64(reader)?;
    }
    for value in &mut fpu.mm {
        *value = read_u64(reader)?;
    }
    Ok(fpu)
}

fn write_device_fixed<W: Write>(writer: &mut W, device: &PcPioBusSnapshot) -> Result<()> {
    write_u8(writer, device.serial.dll)?;
    write_u8(writer, device.serial.dlh)?;
    write_u8(writer, device.serial.ier)?;
    write_u8(writer, device.serial.lcr)?;
    write_u8(writer, device.serial.mcr)?;
    write_u8(writer, device.serial.scratch)?;
    write_bool(writer, device.serial.tx_interrupt)?;
    write_u8(writer, 0)?;
    write_u32(writer, device.pci_config_addr)?;
    write_u16(writer, device.pit0_reload)?;
    write_u16(writer, device.pit0_counter)?;
    write_u8(writer, device.pit0_tmp_low)?;
    write_bool(writer, device.pit0_read_high)?;
    write_bool(writer, device.pit0_write_high)?;
    write_u8(writer, device.port61)?;
    write_u16(writer, device.pit2_reload)?;
    write_u16(writer, device.pit2_counter)?;
    write_u32(writer, device.pit2_status_reads)?;
    write_u8(writer, device.pit2_tmp_low)?;
    write_bool(writer, device.pit2_read_high)?;
    write_bool(writer, device.pit2_write_high)?;
    write_u8(writer, device.pic1_mask)?;
    write_u8(writer, device.pic2_mask)?;
    write_u8(writer, device.pic1_vector_base)?;
    write_u8(writer, device.pic2_vector_base)?;
    write_u8(writer, device.pic1_in_service)?;
    write_u8(writer, device.pic2_in_service)?;
    write_u8(writer, device.pic1_init_step)?;
    write_u8(writer, device.pic2_init_step)?;
    write_bool(writer, device.pic1_expect_icw4)?;
    write_bool(writer, device.pic2_expect_icw4)?;
    write_u8(writer, device.cmos_index)?;
    write_bool(writer, device.reset_requested)
}

fn read_device_fixed<R: Read>(reader: &mut R) -> Result<PcPioBusSnapshot> {
    let serial = Uart8250Snapshot {
        output: Vec::new(),
        input: Vec::new(),
        dll: read_u8(reader)?,
        dlh: read_u8(reader)?,
        ier: read_u8(reader)?,
        lcr: read_u8(reader)?,
        mcr: read_u8(reader)?,
        scratch: read_u8(reader)?,
        tx_interrupt: read_bool(reader)?,
    };
    let _reserved = read_u8(reader)?;
    Ok(PcPioBusSnapshot {
        serial,
        vga: Vec::new(),
        pci_config_addr: read_u32(reader)?,
        pit0_reload: read_u16(reader)?,
        pit0_counter: read_u16(reader)?,
        pit0_tmp_low: read_u8(reader)?,
        pit0_read_high: read_bool(reader)?,
        pit0_write_high: read_bool(reader)?,
        port61: read_u8(reader)?,
        pit2_reload: read_u16(reader)?,
        pit2_counter: read_u16(reader)?,
        pit2_status_reads: read_u32(reader)?,
        pit2_tmp_low: read_u8(reader)?,
        pit2_read_high: read_bool(reader)?,
        pit2_write_high: read_bool(reader)?,
        pic1_mask: read_u8(reader)?,
        pic2_mask: read_u8(reader)?,
        pic1_vector_base: read_u8(reader)?,
        pic2_vector_base: read_u8(reader)?,
        pic1_in_service: read_u8(reader)?,
        pic2_in_service: read_u8(reader)?,
        pic1_init_step: read_u8(reader)?,
        pic2_init_step: read_u8(reader)?,
        pic1_expect_icw4: read_bool(reader)?,
        pic2_expect_icw4: read_bool(reader)?,
        cmos_index: read_u8(reader)?,
        reset_requested: read_bool(reader)?,
    })
}

fn checked_len(name: &str, len: u64) -> Result<usize> {
    usize::try_from(len)
        .map_err(|_| KvmError::BadArgument(format!("snapshot {name} length is too large")))
}

fn read_vec<R: Read>(reader: &mut R, len: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0_u8; len];
    reader.read_exact(&mut bytes).map_err(snapshot_read_error)?;
    Ok(bytes)
}

fn write_bool<W: Write>(writer: &mut W, value: bool) -> Result<()> {
    write_u8(writer, u8::from(value))
}

fn read_bool<R: Read>(reader: &mut R) -> Result<bool> {
    Ok(read_u8(reader)? != 0)
}

fn write_u8<W: Write>(writer: &mut W, value: u8) -> Result<()> {
    writer.write_all(&[value]).map_err(snapshot_write_error)
}

fn write_u16<W: Write>(writer: &mut W, value: u16) -> Result<()> {
    writer
        .write_all(&value.to_le_bytes())
        .map_err(snapshot_write_error)
}

fn write_u32<W: Write>(writer: &mut W, value: u32) -> Result<()> {
    writer
        .write_all(&value.to_le_bytes())
        .map_err(snapshot_write_error)
}

fn write_u64<W: Write>(writer: &mut W, value: u64) -> Result<()> {
    writer
        .write_all(&value.to_le_bytes())
        .map_err(snapshot_write_error)
}

fn read_u8<R: Read>(reader: &mut R) -> Result<u8> {
    let mut bytes = [0_u8; 1];
    reader.read_exact(&mut bytes).map_err(snapshot_read_error)?;
    Ok(bytes[0])
}

fn read_u16<R: Read>(reader: &mut R) -> Result<u16> {
    let mut bytes = [0_u8; 2];
    reader.read_exact(&mut bytes).map_err(snapshot_read_error)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32<R: Read>(reader: &mut R) -> Result<u32> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes).map_err(snapshot_read_error)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64<R: Read>(reader: &mut R) -> Result<u64> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes).map_err(snapshot_read_error)?;
    Ok(u64::from_le_bytes(bytes))
}

fn snapshot_write_error(source: std::io::Error) -> KvmError {
    KvmError::Io {
        op: "write HardMMU snapshot",
        source,
    }
}

fn snapshot_read_error(source: std::io::Error) -> KvmError {
    KvmError::Io {
        op: "read HardMMU snapshot",
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kvm::service::PcPioBus;

    #[test]
    fn snapshot_ram_offsets_are_x86_physical_addresses() {
        use crate::kvm::memslot::{default_ipa_layout, default_pc_region_table, plan_memslots};

        let layout = default_ipa_layout(2 * 1024 * 1024);
        let regions = default_pc_region_table(layout.ram_size).unwrap();
        let plan = plan_memslots(layout, &regions).unwrap();
        let mut backing = plan.allocate_backing().unwrap();
        let mut ram = vec![0; layout.ram_size as usize];
        ram[0] = 0x12;
        ram[0x1000] = 0x34;
        ram[0x9ffff] = 0x56;
        ram[0x100000] = 0x78;
        ram[0x1fffff] = 0x9a;
        scatter_x86_ram(&plan, &mut backing, &ram).unwrap();
        assert_eq!(
            collect_x86_ram(&plan, &backing, layout.ram_size).unwrap(),
            ram
        );
    }

    #[test]
    fn snapshot_roundtrip_preserves_cpu_device_and_ram() {
        let mut cpu = CpuStateV1::protected_flat_linux_entry(0x1234_5678, 0x90000);
        cpu.gpr[0] = 0xaabb_ccdd;
        cpu.interrupt_shadow = 1;
        cpu.cr3 = 0x0012_3000;
        cpu.pae_pdptrs = [0x2001, 0, 0, 0];
        cpu.dr[7] = 0x400;
        cpu.msr.sysenter_cs = 0x10;
        cpu.msr.sysenter_esp = 0xcafe_babe;
        cpu.msr.sysenter_eip = 0xc001_f00d;
        cpu.fpu.status = 0x3800;
        cpu.fpu.top = 3;
        cpu.fpu.st_bits[0] = 3.5f64.to_bits();
        cpu.fpu.mm[0] = 0x0123_4567_89ab_cdef;
        cpu.fpu.mmx_active = 1;
        cpu.tsc = 44;
        cpu.instr_count = 55;

        let mut pio = PcPioBus::new();
        pio.queue_serial_input(b"in");
        assert_eq!(
            pio.write(0x3f8 + 3, 1, 0x80),
            crate::kvm::service::PioResult::Ok(0)
        );
        assert_eq!(
            pio.write(0x3f8, 1, 0x01),
            crate::kvm::service::PioResult::Ok(0)
        );
        assert_eq!(
            pio.write(0x3f8 + 3, 1, 0x00),
            crate::kvm::service::PioResult::Ok(0)
        );
        assert_eq!(
            pio.write(0x3f8, 1, b'Z' as u32),
            crate::kvm::service::PioResult::Ok(0)
        );

        let snapshot = HardMmuSnapshot {
            ram_size: 4096,
            cpu_state: cpu,
            device: pio.snapshot(),
            ram: vec![0x5a; 4096],
        };
        let path = std::env::temp_dir().join(format!(
            "r86-hardmmu-snapshot-test-{}.bin",
            std::process::id()
        ));
        save_hardmmu_snapshot(&path, &snapshot).unwrap();
        let loaded = load_hardmmu_snapshot(&path).unwrap();
        let _ = std::fs::remove_file(path);
        assert_eq!(loaded, snapshot);
    }
}
