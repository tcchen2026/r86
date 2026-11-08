#![cfg_attr(all(target_arch = "aarch64", target_os = "none"), no_std)]
#![cfg_attr(all(target_arch = "aarch64", target_os = "none"), no_main)]

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn main() {}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod aarch64_monitor {
    use core::panic::PanicInfo;

    use r86::monitor::abi::{CpuStateV1, DOORBELL_IPA, MIN_ARM_IPA_BITS, MailboxV1};
    use r86::monitor::{
        MailboxClient, MmioDoorbell, MonitorBoot, MonitorRam, MonitorStage1Tables,
        X86_MONITOR_SMOKE_MAX_STEPS, enable_identity_stage1_from_boot,
        install_exception_panic_transport, monitor_panic_halt, run_x86_monitor_smoke,
    };

    static mut STAGE1_TABLES: MonitorStage1Tables = MonitorStage1Tables::new();

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn r86_monitor_main(boot_block_ipa: u64) -> ! {
        unsafe {
            run(boot_block_ipa);
        }
        idle_forever()
    }

    unsafe fn run(boot_block_ipa: u64) {
        let boot = unsafe { &*(boot_block_ipa as *const r86::monitor::abi::BootBlockV1) };
        let Ok(parsed) = MonitorBoot::from_boot_block(boot_block_ipa, boot, MIN_ARM_IPA_BITS)
        else {
            return;
        };
        if unsafe {
            enable_identity_stage1_from_boot(
                core::ptr::addr_of_mut!(STAGE1_TABLES),
                boot,
                MIN_ARM_IPA_BITS,
            )
        }
        .is_err()
        {
            return;
        }
        let mailbox = unsafe {
            core::slice::from_raw_parts_mut(
                parsed.mailbox_ipa as *mut u8,
                core::mem::size_of::<MailboxV1>(),
            )
        };
        install_exception_panic_transport(mailbox.as_mut_ptr(), DOORBELL_IPA as *mut u32);
        let Ok(mut client) = MailboxClient::new(mailbox) else {
            return;
        };
        let mut doorbell = MmioDoorbell::new(DOORBELL_IPA as *mut u32);
        if client.hello(&mut doorbell, parsed.feature_bits, 0).is_err() {
            return;
        }

        let initial = unsafe { &*(parsed.cpu_state_ipa as *const CpuStateV1) };
        let Ok(mut ram) = (unsafe { MonitorRam::from_raw_parts(parsed.ram_base, parsed.ram_size) })
        else {
            return;
        };
        if run_x86_monitor_smoke(
            &mut client,
            &mut doorbell,
            &mut ram,
            parsed.regions,
            initial,
            X86_MONITOR_SMOKE_MAX_STEPS,
        )
        .is_err()
        {
            return;
        }

        let _ = client.halt(&mut doorbell, 0);
    }

    fn idle_forever() -> ! {
        loop {
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
            }
        }
    }

    #[panic_handler]
    fn panic(_: &PanicInfo<'_>) -> ! {
        monitor_panic_halt(0xffff_0000, 0, 0, 0)
    }
}
