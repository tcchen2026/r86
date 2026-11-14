    .syntax unified
    .thumb

    .section .vectors, "a", %progbits
    .global __vectors
__vectors:
    .word __stack_top
    .word reset
    .rept 13
    .word fault
    .endr
    .word systick

    .section .text.entry, "ax", %progbits
    .thumb_func
    .global reset
    .type reset, %function
reset:
    ldr r0, =__bss_start
    ldr r1, =__bss_end
    movs r2, #0
1:  cmp r0, r1
    bhs 2f
    str r2, [r0], #4
    b 1b
2:  ldr r0, =0xe000ed88         @ CPACR: full access to CP10 and CP11 (FPU)
    ldr r1, [r0]
    orr r1, r1, #0x00f00000
    str r1, [r0]
    dsb
    isb
    bl machine_main
3:  b 3b

    @ SysTick: pass the exception frame, which holds the interrupted PC.
    .thumb_func
    .global systick
    .type systick, %function
systick:
    mov r0, sp
    b systick_sample

    @ Every fault escalates to HardFault. Pass the exception frame on.
    .thumb_func
    .global fault
    .type fault, %function
fault:
    tst lr, #4
    ite eq
    mrseq r0, msp
    mrsne r0, psp
    mov r1, lr
    b fault_report
    .ltorg
