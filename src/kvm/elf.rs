use crate::kvm::backing::SlotBacking;
use crate::kvm::error::{KvmError, Result};
use crate::kvm::memslot::{MemslotKind, PlannedMemslot};
use crate::kvm::probe::PlannedHardMmu;

const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const ELF64_EHDR_SIZE: usize = 64;
const ELF64_PHDR_SIZE: u16 = 56;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonitorElfLoadReport {
    pub entry_ipa: u64,
    pub entry_offset: u64,
    pub load_segments: usize,
    pub loaded_file_bytes: u64,
    pub loaded_mem_bytes: u64,
    pub executable_segments: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ElfHeader {
    elf_type: u16,
    machine: u16,
    entry: u64,
    phoff: u64,
    phentsize: u16,
    phnum: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProgramHeader {
    kind: u32,
    flags: u32,
    offset: u64,
    paddr: u64,
    filesz: u64,
    memsz: u64,
}

pub fn load_monitor_elf(
    planned: &PlannedHardMmu,
    backing: &mut [SlotBacking],
    elf: &[u8],
) -> Result<MonitorElfLoadReport> {
    let monitor = monitor_slot(planned)?;
    let monitor_backing = backing
        .iter_mut()
        .find(|backing| backing.slot == monitor.slot)
        .ok_or(KvmError::MemslotPlan("monitor backing not found"))?;
    let mut header = parse_header(elf)?;
    validate_header(header)?;
    // Current monitors link at their execution IPA, so absolute Rust data and
    // function pointers remain valid. Keep slot-relative legacy smoke images
    // loadable, but the shared Machine requires a correctly linked image.
    let linked_base = if header.entry >= monitor.guest_phys_addr {
        monitor.guest_phys_addr
    } else {
        0
    };
    if planned.machine_monitor && linked_base == 0 {
        return Err(bad_elf(
            "shared Machine monitor must be linked at its execution IPA",
        ));
    }
    header.entry -= linked_base;

    let mut load_segments = 0_usize;
    let mut executable_segments = 0_usize;
    let mut loaded_file_bytes = 0_u64;
    let mut loaded_mem_bytes = 0_u64;
    let mut entry_in_executable_segment = false;

    let phoff = checked_usize(header.phoff, "ELF program header offset too large")?;
    let phentsize = usize::from(header.phentsize);
    let phnum = usize::from(header.phnum);
    let phdr_table_size = phentsize
        .checked_mul(phnum)
        .ok_or_else(|| bad_elf("ELF program header table overflows"))?;
    checked_range(
        elf,
        phoff,
        phdr_table_size,
        "ELF program header table exceeds file",
    )?;

    for index in 0..phnum {
        let offset = phoff + index * phentsize;
        let mut phdr = parse_program_header(&elf[offset..offset + phentsize])?;
        if phdr.kind != PT_LOAD {
            continue;
        }
        phdr.paddr = phdr
            .paddr
            .checked_sub(linked_base)
            .ok_or_else(|| bad_elf("ELF segment precedes monitor link base"))?;
        validate_load_segment(monitor, elf, phdr)?;
        let file_offset = checked_usize(phdr.offset, "ELF segment file offset too large")?;
        let file_size = checked_usize(phdr.filesz, "ELF segment file size too large")?;
        let load_offset = phdr.paddr;
        let bss_offset = load_offset
            .checked_add(phdr.filesz)
            .ok_or_else(|| bad_elf("ELF segment bss offset overflows"))?;
        let bss_len = phdr.memsz - phdr.filesz;

        if file_size != 0 {
            monitor_backing.write_at(load_offset, &elf[file_offset..file_offset + file_size])?;
        }
        if bss_len != 0 {
            zero_monitor_range(monitor_backing, bss_offset, bss_len)?;
        }
        if phdr.flags & PF_X != 0 {
            monitor_backing.flush_instruction_cache_at(load_offset, file_size)?;
            executable_segments += 1;
            if header.entry >= phdr.paddr && header.entry < phdr.paddr + phdr.memsz {
                entry_in_executable_segment = true;
            }
        }
        load_segments += 1;
        loaded_file_bytes = loaded_file_bytes
            .checked_add(phdr.filesz)
            .ok_or_else(|| bad_elf("ELF loaded file byte count overflows"))?;
        loaded_mem_bytes = loaded_mem_bytes
            .checked_add(phdr.memsz)
            .ok_or_else(|| bad_elf("ELF loaded memory byte count overflows"))?;
    }

    if load_segments == 0 {
        return Err(bad_elf("ELF has no PT_LOAD segments"));
    }
    if !entry_in_executable_segment {
        return Err(bad_elf("ELF entry is not inside an executable PT_LOAD"));
    }
    if header.entry >= monitor.memory_size {
        return Err(bad_elf("ELF entry exceeds monitor slot"));
    }

    Ok(MonitorElfLoadReport {
        entry_ipa: monitor.guest_phys_addr + header.entry,
        entry_offset: header.entry,
        load_segments,
        loaded_file_bytes,
        loaded_mem_bytes,
        executable_segments,
    })
}

fn parse_header(elf: &[u8]) -> Result<ElfHeader> {
    if elf.len() < ELF64_EHDR_SIZE {
        return Err(bad_elf("ELF header is truncated"));
    }
    if &elf[0..4] != b"\x7fELF" {
        return Err(bad_elf("ELF magic mismatch"));
    }
    if elf[EI_CLASS] != ELFCLASS64 || elf[EI_DATA] != ELFDATA2LSB {
        return Err(bad_elf("ELF must be 64-bit little-endian"));
    }
    Ok(ElfHeader {
        elf_type: read_u16(elf, 16)?,
        machine: read_u16(elf, 18)?,
        entry: read_u64(elf, 24)?,
        phoff: read_u64(elf, 32)?,
        phentsize: read_u16(elf, 54)?,
        phnum: read_u16(elf, 56)?,
    })
}

fn validate_header(header: ElfHeader) -> Result<()> {
    if header.elf_type != ET_EXEC && header.elf_type != ET_DYN {
        return Err(bad_elf("ELF type must be EXEC or DYN"));
    }
    if header.machine != EM_AARCH64 {
        return Err(bad_elf("ELF machine must be AArch64"));
    }
    if header.phentsize != ELF64_PHDR_SIZE {
        return Err(bad_elf("unexpected ELF program header size"));
    }
    if header.phnum == 0 {
        return Err(bad_elf("ELF has no program headers"));
    }
    Ok(())
}

fn parse_program_header(bytes: &[u8]) -> Result<ProgramHeader> {
    Ok(ProgramHeader {
        kind: read_u32(bytes, 0)?,
        flags: read_u32(bytes, 4)?,
        offset: read_u64(bytes, 8)?,
        paddr: read_u64(bytes, 24)?,
        filesz: read_u64(bytes, 32)?,
        memsz: read_u64(bytes, 40)?,
    })
}

fn validate_load_segment(monitor: &PlannedMemslot, elf: &[u8], phdr: ProgramHeader) -> Result<()> {
    if phdr.memsz < phdr.filesz {
        return Err(bad_elf("ELF segment memsz is smaller than filesz"));
    }
    let file_offset = checked_usize(phdr.offset, "ELF segment file offset too large")?;
    let file_size = checked_usize(phdr.filesz, "ELF segment file size too large")?;
    checked_range(elf, file_offset, file_size, "ELF segment exceeds file")?;
    let segment_end = phdr
        .paddr
        .checked_add(phdr.memsz)
        .ok_or_else(|| bad_elf("ELF segment monitor offset overflows"))?;
    if segment_end > monitor.memory_size {
        return Err(bad_elf("ELF segment exceeds monitor slot"));
    }
    Ok(())
}

fn zero_monitor_range(backing: &mut SlotBacking, offset: u64, len: u64) -> Result<()> {
    let offset = checked_usize(offset, "ELF bss offset too large")?;
    let len = checked_usize(len, "ELF bss length too large")?;
    let end = offset
        .checked_add(len)
        .ok_or(KvmError::MemslotPlan("ELF bss end overflows"))?;
    if end > backing.memory.len() {
        return Err(KvmError::MemslotPlan("ELF bss exceeds monitor backing"));
    }
    backing.memory.as_mut_slice()[offset..end].fill(0);
    Ok(())
}

fn monitor_slot(planned: &PlannedHardMmu) -> Result<&PlannedMemslot> {
    planned
        .memslots
        .slots()
        .iter()
        .find(|slot| slot.kind == MemslotKind::Monitor)
        .ok_or(KvmError::MemslotPlan("monitor memslot missing"))
}

fn checked_range(bytes: &[u8], offset: usize, len: usize, message: &'static str) -> Result<()> {
    let end = offset.checked_add(len).ok_or_else(|| bad_elf(message))?;
    if end > bytes.len() {
        return Err(bad_elf(message));
    }
    Ok(())
}

fn checked_usize(value: u64, message: &'static str) -> Result<usize> {
    usize::try_from(value).map_err(|_| bad_elf(message))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let data = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| bad_elf("ELF u16 read exceeds input"))?;
    Ok(u16::from_le_bytes(data.try_into().unwrap()))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let data = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| bad_elf("ELF u32 read exceeds input"))?;
    Ok(u32::from_le_bytes(data.try_into().unwrap()))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let data = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| bad_elf("ELF u64 read exceeds input"))?;
    Ok(u64::from_le_bytes(data.try_into().unwrap()))
}

fn bad_elf(message: &'static str) -> KvmError {
    KvmError::BadArgument(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kvm::memslot::default_ipa_layout;
    use crate::kvm::probe::ProbeConfig;

    #[test]
    fn loads_aarch64_exec_segments_into_monitor_slot() {
        let planned = ProbeConfig {
            ram_size: 64 * 1024 * 1024,
            monitor_size: 4 * 1024 * 1024,
            shared_size: 1024 * 1024,
            trace_size: 1024 * 1024,
            requested_ipa_bits: None,
            create_vm: false,
            register_memslots: false,
            machine_monitor: false,
        }
        .validate_for_layout_only()
        .unwrap();
        let mut backing = planned.memslots.allocate_backing().unwrap();
        let elf = synthetic_elf(0x1004, 0x1000, 0x100, b"R86!", 8, PF_X);

        let report = load_monitor_elf(&planned, &mut backing, &elf).unwrap();

        let monitor = monitor_slot(&planned).unwrap();
        let monitor_backing = backing
            .iter()
            .find(|backing| backing.slot == monitor.slot)
            .unwrap();
        assert_eq!(report.entry_ipa, monitor.guest_phys_addr + 0x1004);
        assert_eq!(report.load_segments, 1);
        assert_eq!(report.executable_segments, 1);
        assert_eq!(&monitor_backing.memory.as_slice()[0x1000..0x1004], b"R86!");
        assert_eq!(&monitor_backing.memory.as_slice()[0x1004..0x1008], &[0; 4]);
    }

    #[test]
    fn rejects_entry_outside_executable_load() {
        let planned = ProbeConfig::default().validate_for_layout_only().unwrap();
        let mut backing = planned.memslots.allocate_backing().unwrap();
        let elf = synthetic_elf(0x2000, 0x1000, 0x100, b"R86!", 4, PF_X);

        assert!(load_monitor_elf(&planned, &mut backing, &elf).is_err());
    }

    #[test]
    fn execution_ipa_link_preserves_absolute_function_pointer_data() {
        let planned = ProbeConfig {
            machine_monitor: true,
            ram_size: 2 * 1024 * 1024,
            ..Default::default()
        }
        .validate_for_layout_only()
        .unwrap();
        let monitor = monitor_slot(&planned).unwrap();
        let entry = monitor.guest_phys_addr + 0x1004;
        let elf = synthetic_elf(
            entry,
            monitor.guest_phys_addr + 0x1000,
            0x100,
            &entry.to_le_bytes(),
            16,
            PF_X,
        );
        let mut backing = planned.memslots.allocate_backing().unwrap();
        let report = load_monitor_elf(&planned, &mut backing, &elf).unwrap();
        assert_eq!(report.entry_ipa, entry);
        assert_eq!(report.entry_offset, 0x1004);
        let image = backing
            .iter()
            .find(|b| b.slot == monitor.slot)
            .unwrap()
            .memory
            .as_slice();
        assert_eq!(read_u64(image, 0x1000).unwrap(), report.entry_ipa);
        assert_eq!(&image[0x1008..0x1010], &[0; 8]);
        let old = synthetic_elf(0x1004, 0x1000, 0x100, b"R86!", 8, PF_X);
        assert!(load_monitor_elf(&planned, &mut backing, &old).is_err());
    }

    #[test]
    fn rejects_segment_outside_monitor_slot() {
        let layout = default_ipa_layout(64 * 1024 * 1024);
        let planned = ProbeConfig {
            ram_size: layout.ram_size,
            monitor_size: 1024 * 1024,
            shared_size: layout.shared_size,
            trace_size: layout.trace_size,
            requested_ipa_bits: None,
            create_vm: false,
            register_memslots: false,
            machine_monitor: false,
        }
        .validate_for_layout_only()
        .unwrap();
        let mut backing = planned.memslots.allocate_backing().unwrap();
        let elf = synthetic_elf(0x100000, 0x100000, 0x100, b"R86!", 4, PF_X);

        assert!(load_monitor_elf(&planned, &mut backing, &elf).is_err());
    }

    fn synthetic_elf(
        entry: u64,
        paddr: u64,
        file_offset: u64,
        payload: &[u8],
        memsz: u64,
        flags: u32,
    ) -> Vec<u8> {
        let mut elf = vec![0_u8; file_offset as usize + payload.len()];
        elf[0..4].copy_from_slice(b"\x7fELF");
        elf[EI_CLASS] = ELFCLASS64;
        elf[EI_DATA] = ELFDATA2LSB;
        write_u16(&mut elf, 16, ET_EXEC);
        write_u16(&mut elf, 18, EM_AARCH64);
        write_u64(&mut elf, 24, entry);
        write_u64(&mut elf, 32, ELF64_EHDR_SIZE as u64);
        write_u16(&mut elf, 54, ELF64_PHDR_SIZE);
        write_u16(&mut elf, 56, 1);
        write_u32(&mut elf, 64, PT_LOAD);
        write_u32(&mut elf, 68, flags);
        write_u64(&mut elf, 72, file_offset);
        write_u64(&mut elf, 80, paddr);
        write_u64(&mut elf, 88, paddr);
        write_u64(&mut elf, 96, payload.len() as u64);
        write_u64(&mut elf, 104, memsz);
        write_u64(&mut elf, 112, 0x1000);
        elf[file_offset as usize..file_offset as usize + payload.len()].copy_from_slice(payload);
        elf
    }

    fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
}
