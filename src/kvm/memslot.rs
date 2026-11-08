use std::fmt;

use crate::monitor::abi::{
    IpaLayoutV1, PAGE_SIZE_4K, RegionEntryV1, RegionKindV1, RegionPolicyV1, RegionTableV1,
};

use crate::kvm::backing::SlotBacking;
use crate::kvm::error::{KvmError, Result};
use crate::kvm::sys;

pub const LEGACY_VGA_BASE: u64 = 0x000a_0000;
pub const LEGACY_VGA_SIZE: u64 = 0x0002_0000;
pub const LEGACY_ROM_END: u64 = 0x0010_0000;
pub const DEFAULT_MONITOR_SIZE: u64 = 16 * 1024 * 1024;
pub const DEFAULT_SHARED_SIZE: u64 = 2 * 1024 * 1024;
pub const DEFAULT_TRACE_SIZE: u64 = 4 * 1024 * 1024;
pub const MAX_PLANNED_MEMSLOTS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemslotKind {
    X86Ram,
    Monitor,
    Shared,
    Trace,
}

impl MemslotKind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::X86Ram => "x86-ram",
            Self::Monitor => "monitor",
            Self::Shared => "shared",
            Self::Trace => "trace",
        }
    }
}

impl fmt::Display for MemslotKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedMemslot {
    pub slot: u32,
    pub kind: MemslotKind,
    pub guest_phys_addr: u64,
    pub memory_size: u64,
    pub flags: u32,
}

impl PlannedMemslot {
    pub fn new(slot: u32, kind: MemslotKind, guest_phys_addr: u64, memory_size: u64) -> Self {
        Self {
            slot,
            kind,
            guest_phys_addr,
            memory_size,
            flags: 0,
        }
    }

    pub fn end_exclusive(&self) -> Option<u64> {
        self.guest_phys_addr.checked_add(self.memory_size)
    }

    pub fn validate(&self) -> Result<()> {
        if self.memory_size == 0 {
            return Err(KvmError::MemslotPlan("zero-length memslot"));
        }
        if !is_page_aligned(self.guest_phys_addr) || !is_page_aligned(self.memory_size) {
            return Err(KvmError::MemslotPlan("memslot is not page aligned"));
        }
        if self.end_exclusive().is_none() {
            return Err(KvmError::MemslotPlan("memslot end overflows"));
        }
        if self.flags & !(sys::KVM_MEM_LOG_DIRTY_PAGES | sys::KVM_MEM_READONLY) != 0 {
            return Err(KvmError::MemslotPlan("unsupported memslot flags"));
        }
        Ok(())
    }

    pub fn with_readonly(mut self) -> Self {
        self.flags |= sys::KVM_MEM_READONLY;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemslotPlan {
    slots: Vec<PlannedMemslot>,
}

impl MemslotPlan {
    pub fn new() -> Self {
        Self { slots: Vec::new() }
    }

    pub fn push(&mut self, slot: PlannedMemslot) -> Result<()> {
        slot.validate()?;
        if self.slots.len() == MAX_PLANNED_MEMSLOTS {
            return Err(KvmError::MemslotPlan("too many memslots"));
        }
        if self.slots.iter().any(|existing| existing.slot == slot.slot) {
            return Err(KvmError::MemslotPlan("duplicate memslot id"));
        }
        if self.slots.iter().any(|existing| {
            ranges_overlap(
                existing.guest_phys_addr,
                existing.memory_size,
                slot.guest_phys_addr,
                slot.memory_size,
            )
        }) {
            return Err(KvmError::MemslotPlan("overlapping memslots"));
        }
        self.slots.push(slot);
        self.slots.sort_by_key(|slot| slot.guest_phys_addr);
        Ok(())
    }

    pub fn slots(&self) -> &[PlannedMemslot] {
        &self.slots
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn contains_ipa(&self, ipa: u64) -> bool {
        self.slots.iter().any(|slot| {
            let Some(end) = slot.end_exclusive() else {
                return false;
            };
            ipa >= slot.guest_phys_addr && ipa < end
        })
    }

    pub fn total_bytes(&self) -> u64 {
        self.slots
            .iter()
            .fold(0_u64, |sum, slot| sum.saturating_add(slot.memory_size))
    }

    pub fn allocate_backing(&self) -> Result<Vec<SlotBacking>> {
        let mut backing = Vec::with_capacity(self.slots.len());
        for slot in &self.slots {
            backing.push(SlotBacking::new(slot.slot, slot.memory_size)?);
        }
        Ok(backing)
    }
}

impl Default for MemslotPlan {
    fn default() -> Self {
        Self::new()
    }
}

pub fn default_ipa_layout(ram_size: u64) -> IpaLayoutV1 {
    IpaLayoutV1::new(
        round_up_4k(ram_size),
        DEFAULT_MONITOR_SIZE,
        DEFAULT_SHARED_SIZE,
        DEFAULT_TRACE_SIZE,
    )
}

pub fn default_pc_region_table(ram_size: u64) -> Result<RegionTableV1> {
    let ram_size = round_up_4k(ram_size);
    if ram_size == 0 {
        return Err(KvmError::MemslotPlan("x86 RAM size is zero"));
    }

    let mut table = RegionTableV1::new();
    if ram_size <= LEGACY_VGA_BASE {
        table.push(RegionEntryV1::ram(0, ram_size))?;
        return Ok(table);
    }

    table.push(RegionEntryV1::ram(0, LEGACY_VGA_BASE))?;

    let vga_end = LEGACY_VGA_BASE + LEGACY_VGA_SIZE;
    if ram_size > LEGACY_VGA_BASE {
        table.push(RegionEntryV1::new(
            LEGACY_VGA_BASE,
            LEGACY_VGA_SIZE,
            RegionKindV1::Mmio,
            RegionPolicyV1::Trap,
        ))?;
    }

    if ram_size > LEGACY_ROM_END {
        table.push(RegionEntryV1::ram(
            LEGACY_ROM_END,
            ram_size - LEGACY_ROM_END,
        ))?;
    } else if ram_size > vga_end {
        table.push(RegionEntryV1::hole(vga_end, ram_size - vga_end))?;
    }
    Ok(table)
}

pub fn plan_memslots(layout: IpaLayoutV1, regions: &RegionTableV1) -> Result<MemslotPlan> {
    regions.validate()?;
    validate_layout_ranges(layout)?;

    let mut plan = MemslotPlan::new();
    let mut next_slot = 0_u32;

    for index in 0..regions.count as usize {
        let entry = regions.entries[index];
        let kind = entry.kind()?;
        match kind {
            RegionKindV1::Ram => {
                if entry
                    .base
                    .checked_add(entry.len)
                    .ok_or(KvmError::MemslotPlan("RAM region end overflows"))?
                    > layout.ram_size
                {
                    return Err(KvmError::MemslotPlan("RAM region exceeds layout RAM size"));
                }
                plan.push(PlannedMemslot::new(
                    next_slot,
                    MemslotKind::X86Ram,
                    layout.ram_base + entry.base,
                    entry.len,
                ))?;
                next_slot += 1;
            }
            RegionKindV1::Rom
            | RegionKindV1::Mmio
            | RegionKindV1::FbDirtyTracked
            | RegionKindV1::Hole => {}
        }
    }

    if layout.monitor_size != 0 {
        plan.push(PlannedMemslot::new(
            next_slot,
            MemslotKind::Monitor,
            layout.monitor_base,
            layout.monitor_size,
        ))?;
        next_slot += 1;
    }
    if layout.shared_size != 0 {
        plan.push(PlannedMemslot::new(
            next_slot,
            MemslotKind::Shared,
            layout.shared_base,
            layout.shared_size,
        ))?;
        next_slot += 1;
    }
    if layout.trace_size != 0 {
        plan.push(PlannedMemslot::new(
            next_slot,
            MemslotKind::Trace,
            layout.trace_base,
            layout.trace_size,
        ))?;
    }

    if plan.contains_ipa(layout.doorbell_ipa) {
        return Err(KvmError::MemslotPlan("doorbell IPA must be a memslot hole"));
    }
    Ok(plan)
}

pub fn required_ipa_bits(layout: IpaLayoutV1) -> Result<u8> {
    let end = layout
        .required_end()
        .ok_or(KvmError::MemslotPlan("layout end overflows"))?;
    if end <= 1 {
        return Ok(1);
    }
    let last = end - 1;
    let bits = u64::BITS - last.leading_zeros();
    u8::try_from(bits).map_err(|_| KvmError::MemslotPlan("required IPA bits overflow"))
}

pub fn round_up_4k(value: u64) -> u64 {
    if value & (PAGE_SIZE_4K - 1) == 0 {
        value
    } else {
        (value + PAGE_SIZE_4K - 1) & !(PAGE_SIZE_4K - 1)
    }
}

pub const fn is_page_aligned(value: u64) -> bool {
    value & (PAGE_SIZE_4K - 1) == 0
}

fn validate_layout_ranges(layout: IpaLayoutV1) -> Result<()> {
    for (base, size) in [
        (layout.ram_base, layout.ram_size),
        (layout.monitor_base, layout.monitor_size),
        (layout.shared_base, layout.shared_size),
        (layout.trace_base, layout.trace_size),
    ] {
        if size != 0 && (!is_page_aligned(base) || !is_page_aligned(size)) {
            return Err(KvmError::MemslotPlan("layout range is not page aligned"));
        }
        if base.checked_add(size).is_none() {
            return Err(KvmError::MemslotPlan("layout range overflows"));
        }
    }
    if !is_page_aligned(layout.doorbell_ipa) {
        return Err(KvmError::MemslotPlan("doorbell is not page aligned"));
    }
    Ok(())
}

fn ranges_overlap(a_base: u64, a_len: u64, b_base: u64, b_len: u64) -> bool {
    let Some(a_end) = a_base.checked_add(a_len) else {
        return true;
    };
    let Some(b_end) = b_base.checked_add(b_len) else {
        return true;
    };
    a_base < b_end && b_base < a_end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::abi::{DOORBELL_IPA, MIN_ARM_IPA_BITS};

    #[test]
    fn default_region_table_excludes_legacy_vga_and_rom_hole() {
        let table = default_pc_region_table(64 * 1024 * 1024).unwrap();
        assert_eq!(table.count, 3);
        assert_eq!(table.entries[0], RegionEntryV1::ram(0, LEGACY_VGA_BASE));
        assert_eq!(
            table.entries[1],
            RegionEntryV1::mmio(LEGACY_VGA_BASE, LEGACY_VGA_SIZE)
        );
        assert_eq!(
            table.entries[2],
            RegionEntryV1::ram(LEGACY_ROM_END, 63 * 1024 * 1024)
        );
    }

    #[test]
    fn memslot_plan_maps_ram_and_internal_regions_but_not_mmio_or_doorbell() {
        let layout = default_ipa_layout(64 * 1024 * 1024);
        let table = default_pc_region_table(layout.ram_size).unwrap();
        let plan = plan_memslots(layout, &table).unwrap();

        assert_eq!(plan.len(), 5);
        let ram: Vec<_> = plan
            .slots()
            .iter()
            .filter(|s| s.kind == MemslotKind::X86Ram)
            .collect();
        assert_eq!(ram.len(), 2);
        assert_eq!(ram[0].guest_phys_addr, layout.ram_base);
        assert_eq!(ram[0].memory_size, LEGACY_VGA_BASE);
        assert_eq!(ram[1].guest_phys_addr, layout.ram_base + LEGACY_ROM_END);
        assert!(!plan.contains_ipa(0));
        assert!(!plan.contains_ipa(layout.ram_base + LEGACY_VGA_BASE));
        assert!(!plan.contains_ipa(DOORBELL_IPA));
        assert!(plan.contains_ipa(layout.monitor_base));
        assert!(plan.contains_ipa(layout.shared_base));
        assert!(plan.contains_ipa(layout.trace_base));
    }

    #[test]
    fn required_ipa_bits_cover_doorbell_hole() {
        let layout = default_ipa_layout(128 * 1024 * 1024);
        let bits = required_ipa_bits(layout).unwrap();
        assert!(bits <= MIN_ARM_IPA_BITS);
    }

    #[test]
    fn overlap_detection_rejects_bad_internal_layout() {
        let mut layout = default_ipa_layout(64 * 1024 * 1024);
        layout.monitor_base = layout.ram_base;
        let table = default_pc_region_table(layout.ram_size).unwrap();
        assert!(plan_memslots(layout, &table).is_err());
    }
}
