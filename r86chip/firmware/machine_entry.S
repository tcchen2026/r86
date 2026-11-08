.section .text.entry
.global _start
_start:
    .option push
    .option norelax
    .irp reg, x1,x2,x3,x4,x5,x6,x7,x8,x9,x10,x11,x12,x13,x14,x15,x16,x17,x18,x19,x20,x21,x22,x23,x24,x25,x26,x27,x28,x29,x30,x31
    li \reg, 0
    .endr
    la gp, __global_pointer$
    la sp, __stack_top
    la t0, __bss_start
    la t1, __bss_end
1:  bgeu t0, t1, 2f
    sw zero, 0(t0)
    addi t0, t0, 4
    j 1b
2:  call machine_main
    ebreak
3:  j 3b
    .option pop
