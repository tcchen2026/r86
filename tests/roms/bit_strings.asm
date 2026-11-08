; Standalone bit-string semantic oracle, 64 KiB BIOS at FFFF0000h.
; nasm -f bin tests/roms/bit_strings.asm -o /tmp/bit_strings.bin
; Use Bochs p2_klamath, QEMU pentium2, or r86 firmware boot. Checks signed
; register indices (including ignored high bits at operand16), immediate bit
; masking, address16 displacement wrap, and CF/store atomicity on write faults.
; Expected NEG32/NEG16/IMM16/ADDR16: 00000000 00000001.
; IMM32: 80000000 00000001, followed by unchanged DEADBEEF.
; Six WRITE_FAULT rows: 00000003 000008D4 00000001.
; COM1 ends in R86_BITS_DONE. Native coverage is tested separately on ARM64.
bits 16
org 0xf0000

start:
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7000
    lgdt [cs:gdt_ptr - start]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp dword 8:protected

align 8
gdt:
    dq 0, 0x00cf9a000000ffff, 0x00cf92000000ffff
gdt_ptr:
    dw 23
    dd gdt
idt_ptr:
    dw 0x7ff
    dd 0x3000

bits 32
protected:
    mov ax, 16
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov esp, 0x90000
    mov dx, 0x3fb
    mov al, 0x80
    out dx, al
    mov dx, 0x3f8
    mov al, 1
    out dx, al
    inc dx
    xor al, al
    out dx, al
    mov dx, 0x3fb
    mov al, 3
    out dx, al
    mov eax, page_fault
    mov [0x3070], ax
    mov word [0x3072], 8
    mov word [0x3074], 0x8e00
    shr eax, 16
    mov [0x3076], ax
    lidt [idt_ptr]
    mov edi, 0x1000
    xor eax, eax
    mov ecx, 1024
    rep stosd
    mov eax, 3
    mov ecx, 1024
.map:
    stosd
    add eax, 0x1000
    loop .map
    mov dword [0x1000], 0x2003
    mov eax, 0x1000
    mov cr3, eax
    mov eax, cr0
    or eax, 0x80010000
    mov cr0, eax


    mov dword [0x6000], 0x80000000
    mov ecx, -1
    lock btr dword [0x6004], ecx
    pushfd
    pop edx
    mov esi, neg32_msg
    call puts
    mov eax, [0x6000]
    call result

    mov dword [0x6000], 0x8000
    mov ecx, 0xabcdffff
    lock btc word [0x6002], cx
    pushfd
    pop edx
    mov esi, neg16_msg
    call puts
    mov eax, [0x6000]
    call result

    mov dword [0x6000], 0x80000000
    mov dword [0x601c], 0xdeadbeef
    bts dword [0x6000], 255
    pushfd
    pop edx
    mov esi, imm32_msg
    call puts
    mov eax, [0x6000]
    call result
    mov eax, [0x601c]
    call hex
    call newline

    mov dword [0x6000], 0x8000
    btr word [0x6000], 255
    pushfd
    pop edx
    mov esi, imm16_msg
    call puts
    mov eax, [0x6000]
    call result

    mov dword [0xfffa], 0x80000000
    mov ecx, -33
    a16 btr dword [2], ecx
    pushfd
    pop edx
    mov esi, wrap_msg
    call puts
    mov eax, [0xfffa]
    call result

    mov dword [0x2010], 0x8001
    mov dword [0x8000], 1
%macro fault_test 2
    invlpg [0x4000]
    mov dword [0x5004], %%resume
    push dword 0x8d6
    popfd
    lock %1 %2 [0x4000], 0
%%resume:
    mov esi, fault_msg
    call puts
    mov eax, [0x5000]
    call hex
    mov eax, [0x5008]
    and eax, 0x8d5
    call hex
    mov eax, [0x8000]
    call hex
    call newline
%endmacro
    fault_test bts, word
    fault_test btr, word
    fault_test btc, word
    fault_test bts, dword
    fault_test btr, dword
    fault_test btc, dword
    mov esi, done_msg
    call puts
    cli
.halt:
    hlt
    jmp .halt
result:
    call hex
    mov eax, edx
    and eax, 1
    call hex
    jmp newline

page_fault:
    push eax
    mov eax, [esp + 4]
    mov [0x5000], eax
    mov eax, [esp + 16]
    mov [0x5008], eax
    mov eax, [0x5004]
    mov [esp + 8], eax
    pop eax
    add esp, 4
    iretd

putc:
    push eax
    push edx
    mov dx, 0x3fd
.wait:
    in al, dx
    test al, 0x20
    jz .wait
    mov al, [esp + 4]
    mov dx, 0x3f8
    out dx, al
    pop edx
    pop eax
    ret
puts:
    lodsb
    test al, al
    jz .done
    call putc
    jmp puts
.done:
    ret
newline:
    mov al, 10
    jmp putc
hex:
    pushad
    mov ebx, eax
    mov ecx, 8
.digit:
    rol ebx, 4
    mov eax, ebx
    and al, 15
    add al, '0'
    cmp al, '9'
    jbe .print
    add al, 7
.print:
    call putc
    loop .digit
    mov al, ' '
    call putc
    popad
    ret

neg32_msg: db 'NEG32 ', 0
neg16_msg: db 'NEG16 ', 0
imm32_msg: db 'IMM32 ', 0
imm16_msg: db 'IMM16 ', 0
wrap_msg: db 'ADDR16 ', 0
fault_msg: db 'WRITE_FAULT ', 0
done_msg: db 'R86_BITS_DONE', 10, 0

times 0xfff0 - ($ - $$) db 0xff
bits 16
    jmp 0xf000:0
times 0x10000 - ($ - $$) db 0xff
