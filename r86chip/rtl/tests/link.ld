ENTRY(_start)
SECTIONS {
    . = 0x1000;
    .text : { KEEP(*(.text.entry)) *(.text .text.*) }
    .rodata : { *(.rodata .rodata.*) }
    .data : { *(.data .data.*) *(.sdata .sdata.*) }
    .bss : { *(.bss .bss.*) *(.sbss .sbss.*) }
    /DISCARD/ : { *(.eh_frame*) *(.comment*) }
}
