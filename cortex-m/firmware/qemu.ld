/* r86 machine firmware on QEMU's MPS3 AN524 (Cortex-M33). The core starts in
   Secure state and reads its vectors from BRAM at 0x10000000. Everything else
   is in DDR at 0x60000000; with the SAU off, all memory is Secure. */
ENTRY(reset)
MEMORY {
    VECTORS (r)   : ORIGIN = 0x10000000, LENGTH = 0x400
    PROGRAM (rwx) : ORIGIN = 0x60000000, LENGTH = 0x01000000
}
SECTIONS {
    .vectors : { KEEP(*(.vectors)) } > VECTORS
    .text : { KEEP(*(.text.entry)) *(.text .text.*) } > PROGRAM
    .rodata : { *(.rodata .rodata.*) } > PROGRAM
    .data : { *(.data .data.*) } > PROGRAM
    .bss (NOLOAD) : {
        . = ALIGN(4); __bss_start = .;
        *(.bss .bss.*) *(COMMON)
        . = ALIGN(4); __bss_end = .;
    } > PROGRAM
    /* 16 MiB of stack below the heap (abi.rs). */
    __stack_top = 0x62000000;
    /DISCARD/ : { *(.ARM.exidx*) *(.ARM.extab*) *(.eh_frame*) }
}
