use std::fmt;

use crate::monitor::abi::{IpaLayoutV1, MIN_ARM_IPA_BITS, RegionEntryV1, RegionTableV1};

use crate::kvm::error::{KvmError, Result};
use crate::kvm::kvm::Kvm;
use crate::kvm::memslot::{
    MemslotPlan, default_ipa_layout, default_pc_region_table, plan_memslots, required_ipa_bits,
};
use crate::kvm::sys;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeConfig {
    pub ram_size: u64,
    pub monitor_size: u64,
    pub shared_size: u64,
    pub trace_size: u64,
    pub requested_ipa_bits: Option<u8>,
    pub create_vm: bool,
    pub register_memslots: bool,
    pub machine_monitor: bool,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        let layout = default_ipa_layout(128 * 1024 * 1024);
        Self {
            ram_size: layout.ram_size,
            monitor_size: layout.monitor_size,
            shared_size: layout.shared_size,
            trace_size: layout.trace_size,
            requested_ipa_bits: None,
            create_vm: false,
            register_memslots: false,
            machine_monitor: false,
        }
    }
}

impl ProbeConfig {
    pub fn layout(&self) -> IpaLayoutV1 {
        IpaLayoutV1::new(
            self.ram_size,
            self.monitor_size,
            self.shared_size,
            self.trace_size,
        )
    }

    pub fn region_table(&self) -> Result<RegionTableV1> {
        if self.machine_monitor {
            // Machine owns guest address decoding, including VGA and ROM.
            // This map describes its complete contiguous storage backing.
            let mut regions = RegionTableV1::new();
            regions.push(RegionEntryV1::ram(0, self.ram_size))?;
            Ok(regions)
        } else {
            default_pc_region_table(self.ram_size)
        }
    }

    pub fn planned_memslots(&self) -> Result<MemslotPlan> {
        let layout = self.layout();
        let regions = self.region_table()?;
        plan_memslots(layout, &regions)
    }

    pub fn validate_for_layout_only(&self) -> Result<PlannedHardMmu> {
        let layout = self.layout();
        let required_bits = required_ipa_bits(layout)?.max(MIN_ARM_IPA_BITS);
        layout.validate(required_bits)?;
        let regions = self.region_table()?;
        let memslots = plan_memslots(layout, &regions)?;
        Ok(PlannedHardMmu {
            machine_monitor: self.machine_monitor,
            layout,
            regions,
            memslots,
            required_ipa_bits: required_bits,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedHardMmu {
    pub machine_monitor: bool,
    pub layout: IpaLayoutV1,
    pub regions: RegionTableV1,
    pub memslots: MemslotPlan,
    pub required_ipa_bits: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeReport {
    pub api_version: i32,
    pub user_memory_cap: i32,
    pub nr_memslots_cap: i32,
    pub arm_ipa_bits_cap: i32,
    pub chosen_ipa_bits: u8,
    pub vcpu_mmap_size: i32,
    pub planned: PlannedHardMmu,
    pub create_vm: Option<CreateVmReport>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateVmReport {
    pub created: bool,
    pub registered_memslots: bool,
    pub registered_slot_count: usize,
}

pub fn run_probe(config: &ProbeConfig) -> Result<ProbeReport> {
    let planned = config.validate_for_layout_only()?;
    let kvm = Kvm::open()?;
    let api_version = kvm.require_api_version()?;
    let user_memory_cap = kvm.require_extension(sys::KVM_CAP_USER_MEMORY, "KVM_CAP_USER_MEMORY")?;
    let nr_memslots_cap = kvm.require_extension(sys::KVM_CAP_NR_MEMSLOTS, "KVM_CAP_NR_MEMSLOTS")?;
    if planned.memslots.len() > nr_memslots_cap as usize {
        return Err(KvmError::CapabilityTooSmall {
            cap: sys::KVM_CAP_NR_MEMSLOTS,
            name: "KVM_CAP_NR_MEMSLOTS",
            found: nr_memslots_cap,
            required: planned.memslots.len() as i32,
        });
    }
    let arm_ipa_bits_cap =
        kvm.require_extension(sys::KVM_CAP_ARM_VM_IPA_SIZE, "KVM_CAP_ARM_VM_IPA_SIZE")?;
    let chosen_ipa_bits = choose_ipa_bits(
        planned.required_ipa_bits,
        arm_ipa_bits_cap,
        config.requested_ipa_bits,
    )?;
    planned.layout.validate(chosen_ipa_bits)?;
    let vcpu_mmap_size = kvm.vcpu_mmap_size()?;

    let create_vm = if config.create_vm {
        let vm = kvm.create_arm_vm(chosen_ipa_bits)?;
        if config.register_memslots {
            let registered = vm.register_memslot_plan(&planned.memslots)?;
            let registered_slot_count = registered.slot_count();
            Some(CreateVmReport {
                created: true,
                registered_memslots: true,
                registered_slot_count,
            })
        } else {
            Some(CreateVmReport {
                created: true,
                registered_memslots: false,
                registered_slot_count: 0,
            })
        }
    } else {
        None
    };

    Ok(ProbeReport {
        api_version,
        user_memory_cap,
        nr_memslots_cap,
        arm_ipa_bits_cap,
        chosen_ipa_bits,
        vcpu_mmap_size,
        planned,
        create_vm,
    })
}

pub fn choose_ipa_bits(required_bits: u8, cap_value: i32, requested: Option<u8>) -> Result<u8> {
    if cap_value <= 0 {
        return Err(KvmError::MissingCapability {
            cap: sys::KVM_CAP_ARM_VM_IPA_SIZE,
            name: "KVM_CAP_ARM_VM_IPA_SIZE",
        });
    }
    let cap = u8::try_from(cap_value).map_err(|_| KvmError::CapabilityTooSmall {
        cap: sys::KVM_CAP_ARM_VM_IPA_SIZE,
        name: "KVM_CAP_ARM_VM_IPA_SIZE",
        found: cap_value,
        required: i32::from(required_bits),
    })?;
    let min = required_bits.max(MIN_ARM_IPA_BITS);
    if cap < min {
        return Err(KvmError::CapabilityTooSmall {
            cap: sys::KVM_CAP_ARM_VM_IPA_SIZE,
            name: "KVM_CAP_ARM_VM_IPA_SIZE",
            found: cap_value,
            required: i32::from(min),
        });
    }
    match requested {
        Some(bits) if bits < min || bits > cap => Err(KvmError::InvalidIpaBits {
            requested: bits,
            min,
            max: cap,
        }),
        Some(bits) => Ok(bits),
        None => Ok(min),
    }
}

impl fmt::Display for PlannedHardMmu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "layout.ram=0x{:x}:0x{:x}",
            self.layout.ram_base, self.layout.ram_size
        )?;
        writeln!(
            f,
            "layout.monitor=0x{:x}:0x{:x}",
            self.layout.monitor_base, self.layout.monitor_size
        )?;
        writeln!(
            f,
            "layout.shared=0x{:x}:0x{:x}",
            self.layout.shared_base, self.layout.shared_size
        )?;
        writeln!(
            f,
            "layout.trace=0x{:x}:0x{:x}",
            self.layout.trace_base, self.layout.trace_size
        )?;
        writeln!(f, "layout.doorbell=0x{:x}", self.layout.doorbell_ipa)?;
        writeln!(f, "layout.required_ipa_bits={}", self.required_ipa_bits)?;
        writeln!(f, "regions.count={}", self.regions.count)?;
        for index in 0..self.regions.count as usize {
            let entry = self.regions.entries[index];
            writeln!(
                f,
                "region.{index}=kind:{} base:0x{:x} len:0x{:x}",
                entry.kind.map_kind_name(),
                entry.base,
                entry.len
            )?;
        }
        writeln!(f, "memslots.count={}", self.memslots.len())?;
        for (index, slot) in self.memslots.slots().iter().enumerate() {
            writeln!(
                f,
                "memslot.{index}=slot:{} kind:{} ipa:0x{:x} size:0x{:x} flags:0x{:x}",
                slot.slot, slot.kind, slot.guest_phys_addr, slot.memory_size, slot.flags
            )?;
        }
        Ok(())
    }
}

impl fmt::Display for ProbeReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "r86-kvm.probe=ok")?;
        writeln!(f, "kvm.api_version={}", self.api_version)?;
        writeln!(f, "kvm.user_memory_cap={}", self.user_memory_cap)?;
        writeln!(f, "kvm.nr_memslots_cap={}", self.nr_memslots_cap)?;
        writeln!(f, "kvm.arm_ipa_bits_cap={}", self.arm_ipa_bits_cap)?;
        writeln!(f, "kvm.chosen_ipa_bits={}", self.chosen_ipa_bits)?;
        writeln!(f, "kvm.vcpu_mmap_size={}", self.vcpu_mmap_size)?;
        write!(f, "{}", self.planned)?;
        match &self.create_vm {
            Some(create) => {
                writeln!(f, "vm.created={}", create.created)?;
                writeln!(f, "vm.registered_memslots={}", create.registered_memslots)?;
                writeln!(
                    f,
                    "vm.registered_slot_count={}",
                    create.registered_slot_count
                )
            }
            None => writeln!(f, "vm.created=false"),
        }
    }
}

trait KindName {
    fn map_kind_name(self) -> &'static str;
}

impl KindName for u32 {
    fn map_kind_name(self) -> &'static str {
        match crate::monitor::abi::RegionKindV1::from_raw(self) {
            Some(crate::monitor::abi::RegionKindV1::Hole) => "hole",
            Some(crate::monitor::abi::RegionKindV1::Ram) => "ram",
            Some(crate::monitor::abi::RegionKindV1::Rom) => "rom",
            Some(crate::monitor::abi::RegionKindV1::Mmio) => "mmio",
            Some(crate::monitor::abi::RegionKindV1::FbDirtyTracked) => "fb-dirty-tracked",
            None => "bad",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_minimum_valid_ipa_size_by_default() {
        assert_eq!(choose_ipa_bits(33, 40, None).unwrap(), MIN_ARM_IPA_BITS);
        assert_eq!(choose_ipa_bits(36, 40, None).unwrap(), 36);
        assert_eq!(choose_ipa_bits(36, 40, Some(39)).unwrap(), 39);
        assert!(choose_ipa_bits(36, 35, None).is_err());
        assert!(choose_ipa_bits(36, 40, Some(35)).is_err());
    }

    #[test]
    fn layout_only_plan_has_doorbell_hole() {
        let config = ProbeConfig::default();
        let planned = config.validate_for_layout_only().unwrap();
        assert!(!planned.memslots.contains_ipa(planned.layout.doorbell_ipa));
        assert!(planned.required_ipa_bits >= MIN_ARM_IPA_BITS);
    }

    #[test]
    fn display_includes_memslot_summary() {
        let config = ProbeConfig::default();
        let planned = config.validate_for_layout_only().unwrap();
        let text = planned.to_string();
        assert!(text.contains("layout.doorbell=0x1f0000000"));
        assert!(text.contains("memslots.count=5"));
        assert!(text.contains("kind:x86-ram"));
        assert!(text.contains("kind:monitor"));
    }
}
