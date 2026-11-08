//! ARM64 KVM host-policy inheritance and SSBS exception-return regression.
#[cfg(target_arch = "aarch64")]
mod arm {
    use r86::kvm::backing::SlotBacking;
    use r86::kvm::kvm::Kvm;
    use r86::kvm::memslot::{MemslotKind, MemslotPlan, PlannedMemslot};
    use r86::kvm::sys;
    use r86::monitor::abi::DOORBELL_IPA;
    use std::time::Duration;

    core::arch::global_asm!(
        r#"
        .pushsection .rodata.r86_policy_guest, "a"
        .balign 2048
        .global __r86_policy_guest_start
        .global __r86_policy_guest_end
    __r86_policy_guest_start:
        adr x0, 1f
        msr vbar_el1, x0
        isb
        brk #0
        mov w0, #0x1234
        ldr x1, 3f
        str w0, [x1]
        b .
        .balign 2048
    1:
        .space 0x200
        mrs x0, spsr_el1
        ldr x1, 3f
        str w0, [x1]
        mrs x0, elr_el1
        add x0, x0, #4
        msr elr_el1, x0
        eret
        .balign 8
    3:
        .quad {doorbell}
    __r86_policy_guest_end:
        .popsection
        "#,
        doorbell = const DOORBELL_IPA,
    );

    fn host_policy() -> i32 {
        unsafe {
            libc::prctl(
                sys::PR_GET_SPECULATION_CTRL,
                sys::PR_SPEC_STORE_BYPASS,
                0_usize,
                0_usize,
                0_usize,
            )
        }
    }

    fn check_vm(label: &str, hide_ssbs: bool) {
        let base = 0x1_0000_0000;
        let kvm = Kvm::open().unwrap();
        let vm = kvm.create_arm_vm(34).unwrap();
        let mut init = vm.arm_preferred_target().unwrap();
        init.features[0] &= !(1 << sys::KVM_ARM_VCPU_POWER_OFF);
        let mut plan = MemslotPlan::new();
        plan.push(PlannedMemslot::new(0, MemslotKind::Monitor, base, 65536))
            .unwrap();
        let mut backing = SlotBacking::new(0, 65536).unwrap();
        unsafe extern "C" {
            static __r86_policy_guest_start: u8;
            static __r86_policy_guest_end: u8;
        }
        // The two assembly labels bound one immutable embedded guest image.
        let start = &raw const __r86_policy_guest_start;
        let len = (&raw const __r86_policy_guest_end as usize) - start as usize;
        let code = unsafe { core::slice::from_raw_parts(start, len) };
        backing.write_at(0, code).unwrap();
        backing.flush_instruction_cache_at(0, len).unwrap();
        let _registered = vm.register_backing(&plan, vec![backing]).unwrap();
        let vcpu = vm.create_vcpu(0).unwrap();
        vcpu.arm_init(&init).unwrap();
        let mut pfr1 = vcpu
            .get_one_reg_u64(sys::KVM_ARM64_REG_ID_AA64PFR1_EL1)
            .unwrap();
        if hide_ssbs {
            pfr1 &= !0xf0;
            vcpu.set_one_reg_u64(sys::KVM_ARM64_REG_ID_AA64PFR1_EL1, pfr1)
                .unwrap();
        }
        let policy = host_policy();
        let expected_ssbs = if matches!((pfr1 >> 4) & 15, 1 | 2) && matches!(policy, 2 | 3) {
            sys::PSR_SSBS_BIT
        } else {
            0
        };
        vcpu.set_arm_entry_state(base, base + 65520).unwrap();
        let pstate = vcpu
            .get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PSTATE))
            .unwrap();
        assert_eq!(pstate, sys::PSR_DAIF_MASKED_EL1H | expected_ssbs);
        let map = vcpu
            .map_run(kvm.vcpu_mmap_size().unwrap() as usize)
            .unwrap();
        assert!(vcpu.run_with_timeout(&map, Duration::from_secs(2)).unwrap());
        let saved = map.mmio().unwrap();
        assert_eq!(
            (saved.phys_addr, saved.len, saved.is_write),
            (DOORBELL_IPA, 4, 1)
        );
        assert_eq!(
            u32::from_le_bytes(saved.data[..4].try_into().unwrap()) as u64,
            pstate
        );
        assert!(vcpu.run_with_timeout(&map, Duration::from_secs(2)).unwrap());
        let resumed = map.mmio().unwrap();
        assert_eq!(
            (resumed.phys_addr, resumed.len, resumed.is_write),
            (DOORBELL_IPA, 4, 1)
        );
        assert_eq!(
            u32::from_le_bytes(resumed.data[..4].try_into().unwrap()),
            0x1234
        );
        let restored = vcpu
            .get_one_reg_u64(sys::arm_core_reg_id(sys::KVM_ARM_CORE_REG_PSTATE))
            .unwrap();
        assert_eq!(restored, pstate);
        assert_eq!(host_policy(), policy);
        println!(
            "policy.case={label} host={policy} pfr1={pfr1:#x} entry={pstate:#x} restored={restored:#x}"
        );
    }

    pub fn run() {
        check_vm("inherited", false);
        check_vm("no_guest_ssbs", true);
        // These prctls affect only this regression process. Force-disable is
        // last because that policy intentionally cannot be undone.
        for (label, setting) in [
            ("disabled", 4_usize),
            ("noexec", 16),
            ("enabled", 2),
            ("forced", 8),
        ] {
            assert_eq!(
                unsafe { libc::prctl(53, sys::PR_SPEC_STORE_BYPASS, setting, 0_usize, 0_usize) },
                0
            );
            assert_eq!(host_policy(), sys::PR_SPEC_PRCTL | setting as i32);
            check_vm(label, false);
        }
    }
}

fn main() {
    #[cfg(target_arch = "aarch64")]
    arm::run();
    #[cfg(not(target_arch = "aarch64"))]
    panic!("This hardware regression requires ARM64 KVM");
}
