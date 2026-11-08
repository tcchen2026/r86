//! ARM64 KVM timeout, rearm, and MMIO-completion regression.
use r86::kvm::backing::SlotBacking;
use r86::kvm::kvm::Kvm;
use r86::kvm::memslot::{MemslotKind, MemslotPlan, PlannedMemslot};
use r86::kvm::sys;
use r86::monitor::abi::DOORBELL_IPA;
use std::time::{Duration, Instant};

fn main() {
    let base = 0x1_0000_0000_u64;
    let kvm = Kvm::open().unwrap();
    kvm.require_api_version().unwrap();
    let vm = kvm.create_arm_vm(34).unwrap();
    let mut init = vm.arm_preferred_target().unwrap();
    init.features[0] &= !(1 << sys::KVM_ARM_VCPU_POWER_OFF);
    let mut plan = MemslotPlan::new();
    plan.push(PlannedMemslot::new(0, MemslotKind::Monitor, base, 65536))
        .unwrap();
    let mut backing = SlotBacking::new(0, 65536).unwrap();
    let mut code = Vec::new();
    for op in [
        0x5282_4680_u32, // mov w0, #0x1234
        0x5800_00e1,     // ldr x1, literal at +32
        0xb900_0020,     // str w0, [x1]
        0x1100_0400,     // add w0, w0, #1
        0xb900_0020,     // str w0, [x1]
        0x1400_0000,     // b .
        0xd503_201f,
        0xd503_201f,
    ] {
        code.extend_from_slice(&op.to_le_bytes());
    }
    code.extend_from_slice(&DOORBELL_IPA.to_le_bytes());
    backing.write_at(0, &code).unwrap();
    backing.flush_instruction_cache_at(0, code.len()).unwrap();
    let _registered = vm.register_backing(&plan, vec![backing]).unwrap();
    let vcpu = vm.create_vcpu(0).unwrap();
    vcpu.arm_init(&init).unwrap();
    vcpu.set_arm_entry_state(base + 20, base + 65520).unwrap();
    let map = vcpu
        .map_run(kvm.vcpu_mmap_size().unwrap() as usize)
        .unwrap();
    for timeout in [20, 40, 10] {
        let start = Instant::now();
        assert!(
            !vcpu
                .run_with_timeout(&map, Duration::from_millis(timeout))
                .unwrap()
        );
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(timeout));
        assert!(elapsed < Duration::from_secs(2));
        assert_eq!(map.as_run().immediate_exit, 0);
        println!("timeout.ms={timeout} elapsed={elapsed:?}");
    }
    let mut race_completed = 0;
    for trial in 0..100 {
        vcpu.set_arm_core_reg(sys::KVM_ARM_CORE_REG_PC, base)
            .unwrap();
        let completed = vcpu
            .run_with_timeout(
                &map,
                if trial == 0 {
                    Duration::from_secs(1)
                } else {
                    Duration::ZERO
                },
            )
            .unwrap();
        race_completed += usize::from(completed);
        if !completed {
            assert!(vcpu.run_with_timeout(&map, Duration::from_secs(1)).unwrap());
        }
        let first = map.mmio().unwrap();
        assert_eq!(
            (first.phys_addr, first.len, first.is_write),
            (DOORBELL_IPA, 4, 1)
        );
        assert_eq!(
            u32::from_le_bytes(first.data[..4].try_into().unwrap()),
            0x1234
        );
        assert!(vcpu.run_with_timeout(&map, Duration::from_secs(1)).unwrap());
        let second = map.mmio().unwrap();
        assert_eq!(
            u32::from_le_bytes(second.data[..4].try_into().unwrap()),
            0x1235
        );
        // Complete the second pending MMIO before rewinding PC for the next trial.
        assert!(
            !vcpu
                .run_with_timeout(&map, Duration::from_millis(1))
                .unwrap()
        );
    }
    println!("timeout.mmio_trials=100 race_completed={race_completed}");
}
