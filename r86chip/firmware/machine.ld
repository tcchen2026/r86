OUTPUT_ARCH(riscv)
ENTRY(_start)
MEMORY { ROMRAM (rwx) : ORIGIN = 0x1000, LENGTH = 0x7ff000 }
SECTIONS {
    .text : { KEEP(*(.text.entry)) *(.text .text.*) } > ROMRAM
    .rodata : { *(.rodata .rodata.*) } > ROMRAM
    .data : { *(.data .data.*) *(.sdata .sdata.*) } > ROMRAM
    .bss (NOLOAD) : {
        . = ALIGN(4); __bss_start = .;
        *(.bss .bss.*) *(.sbss .sbss.*) *(COMMON)
        . = ALIGN(4); __bss_end = .;
    } > ROMRAM
    __global_pointer$ = ADDR(.data) + 0x800;
    __stack_top = 0x08000000;
    /DISCARD/ : { *(.eh_frame*) }
}
