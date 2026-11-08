unsafe extern "C" {
    fn r86_monitor_main(boot_block_ipa: u64) -> !;
}

// The host enters EL1 with MMU off, a valid stack, and the boot block IPA in x0.
// Even a Rust function prologue may save SIMD registers, so enable FP/SIMD
// before branching to any compiler-generated code. Preserve x0 and the stack.
#[unsafe(no_mangle)]
#[unsafe(naked)]
unsafe extern "C" fn _start(_boot_block_ipa: u64) -> ! {
    core::arch::naked_asm!(
        "mrs x1, cpacr_el1",
        "orr x1, x1, #0x300000",
        "msr cpacr_el1, x1",
        "isb",
        "b {main}",
        main = sym r86_monitor_main,
    );
}
