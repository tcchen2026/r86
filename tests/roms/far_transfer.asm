; Independent far-transfer fault probe (r86 and Bochs p2_klamath).
; nasm -f bin tests/roms/far_transfer.asm -o /tmp/far_transfer.bin
; #PF/#GP/#SS use task gates, so exception entry cannot overwrite the
; partially written CALL frame. The handler observes the saved old task,
; repairs mappings, then resumes at the next probe. No host-internal oracle.
; Rows: case, vector, code, CR2, saved EIP/ESP deltas, saved CS/SS,
; target CS/SS AR bytes, first/second frame slots, descriptor PTE A/D.
; Nine rows followed by R86_FAR_TRANSFER_OK; compare every field.
bits 16
org 0xf0000
%define GDT 0x3fc8
%define IDT 0x3000
%define OLD 0x5000
%define CASE 0x7000
%define EXPECT_IP 0x7004
%define EXPECT_SP 0x7008
%define RESUME 0x700c
%define SAVED_CR2 0x7010
%define SAVED_AD 0x7014
%define COUNT 0x7018
%define SAVED_CODE 0x701c
%define SAVED_VECTOR 0x7020
start:
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x7000
    lgdt [cs:boot_gdt_ptr-start]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp dword 8:protected
align 8
%macro tss_descriptor 1
    dd ((%1 & 0xffff) << 16) | 0x67
    dd 0x00008900 | ((%1 >> 16) & 0xff)
%endmacro
boot_gdt:
    dq 0, 0x00cf9b000000ffff, 0x00cf93000000ffff
    tss_descriptor OLD
    tss_descriptor 0x5100
    tss_descriptor 0x5200
    tss_descriptor 0x5300
    dq 0x00cf9a000000ffff      ; 38: target CS, at 4000h
    dq 0x00cf92000000ffff      ; 40: target SS
    dq 0                       ; 48: call gate
boot_gdt_end:
boot_gdt_ptr:
    dw boot_gdt_end-boot_gdt-1
    dd boot_gdt
ram_gdt_ptr:
    dw boot_gdt_end-boot_gdt-1
    dd GDT
idt_ptr:
    dw 0x7ff
    dd IDT
bits 32
protected:
    mov ax, 16
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    mov esp, 0x9004
    mov dx, 0x3fb
    mov al, 3
    out dx, al
    xor eax, eax
    mov edi, 0x1000
    mov ecx, 0x7000/4
    rep stosd
    mov esi, boot_gdt
    mov edi, GDT
    mov ecx, (boot_gdt_end-boot_gdt)/4
    rep movsd
    lgdt [ram_gdt_ptr]
%macro task 3
    mov dword [%1+0x1c], 0x1000
    mov dword [%1+0x20], %2
    mov dword [%1+0x24], 2
    mov dword [%1+0x38], %3
    mov dword [%1+0x48], 16
    mov dword [%1+0x4c], 8
    mov dword [%1+0x50], 16
    mov dword [%1+0x54], 16
    mov dword [%1+0x58], 16
    mov dword [%1+0x5c], 16
%endmacro
    task 0x5100, pf_entry, 0xa0000
    task 0x5200, gp_entry, 0xb0000
    task 0x5300, ss_entry, 0xc0000
    mov word [IDT+14*8+2], 0x20
    mov word [IDT+13*8+2], 0x28
    mov word [IDT+12*8+2], 0x30
    mov word [IDT+14*8+4], 0x8500
    mov word [IDT+13*8+4], 0x8500
    mov word [IDT+12*8+4], 0x8500
    mov eax, target
    mov word [IDT+0x31*8], ax
    mov word [IDT+0x31*8+2], 8
    mov word [IDT+0x31*8+4], 0x8e00
    shr eax, 16
    mov word [IDT+0x31*8+6], ax
    lidt [idt_ptr]
    mov dword [OLD+0x1c], 0x1000
    mov ax, 0x18
    ltr ax
    mov edi, 0x2000
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

%macro prepare 4
    mov dword [CASE], %1
    mov dword [RESUME], %2
    mov dword [EXPECT_IP], %3
    mov esp, %4
    mov [EXPECT_SP], esp
    mov dword [0x4000], 0xffff
    mov dword [0x4004], 0x00cf9a00
    mov dword [0x4008], 0xffff
    mov dword [0x400c], 0x00cf9200
    mov eax, target
    mov word [0x4010], ax
    mov word [0x4012], 0x38
    mov word [0x4014], 0x8c00
    shr eax, 16
    mov word [0x4016], ax
    mov dword [0x2010], 0x4001 ; target descriptor page read-only, A clear
    mov eax, cr3
    mov cr3, eax
    mov eax, 0xdeadbeef
    mov cr2, eax
    push dword 0x803
    popfd
    mov dword [0x9000], 0xaaaaaaaa
    mov dword [0x8ffc], 0xbbbbbbbb
%endmacro
    prepare 1, after1, jump_ro, 0x9004
jump_ro:
    jmp 0x38:target
after1:
    prepare 2, after2, call_ro, 0x9004
call_ro:
    call 0x38:target
    jmp fail
after2:
    prepare 3, after3, ret_ro, 0x9004
    mov dword [esp], target
    mov dword [esp+4], 0x38
ret_ro:
    retf
after3:
    prepare 4, after4, iret_ro, 0x9004
    mov dword [esp], target
    mov dword [esp+4], 0x38
    mov dword [esp+8], 0x803
iret_ro:
    iretd
after4:
    prepare 5, after5, gate_call_ro, 0x9004
gate_call_ro:
    call 0x48:0
    jmp fail
after5:
    prepare 6, after6, gate_jump_ro, 0x9004
gate_jump_ro:
    jmp 0x48:0
after6:
    prepare 7, after7, partial_call, 0x9004
    mov dword [0x2010], 0x4003
    mov eax, cr3
    mov cr3, eax
    mov dword [0x4000], 0x10
    mov dword [0x4004], 0x00409a00 ; invalid target too: stack #PF wins
    mov dword [0x2020], 0
    mov eax, cr3
    mov cr3, eax
partial_call:
    call 0x38:0x20
    jmp fail
after7:
    prepare 8, after8, partial_gate16, 0x9002
    mov dword [0x2010], 0x4003
    mov eax, cr3
    mov cr3, eax
    mov word [0x4014], 0x8400
    mov dword [0x2020], 0
    mov eax, cr3
    mov cr3, eax
partial_gate16:
    call 0x48:0
    jmp fail
after8:
    prepare 9, after9, partial_int, 0x9004
    mov dword [0x2020], 0
    mov eax, cr3
    mov cr3, eax
partial_int:
    int 0x31
    jmp fail
after9:
    cmp dword [COUNT], 9
    jne fail
    mov esi, done_msg
    call puts
halt:
    hlt
    jmp halt
target:
fail:
    mov esi, fail_msg
    call puts
    jmp halt
pf_entry:
    mov ebx, 14
    jmp fault
gp_entry:
    mov ebx, 13
    jmp fault
ss_entry:
    mov ebx, 12
fault:
    pop eax
    mov [SAVED_CODE], eax
    mov [SAVED_VECTOR], ebx
    mov eax, cr2
    mov [SAVED_CR2], eax
    mov eax, [0x2010]
    and eax, 0x60
    mov [SAVED_AD], eax
    ; Repair mappings so the observer can read both completed/failed slots.
    mov dword [0x2010], 0x4003
    mov dword [0x2020], 0x8003
    mov eax, cr3
    mov cr3, eax
    inc dword [COUNT]
    mov esi, row_msg
    call puts
    mov eax, [CASE]
    call hex
    mov eax, [SAVED_VECTOR]
    call hex
    mov eax, [SAVED_CODE]
    call hex
    mov eax, [SAVED_CR2]
    call hex
    mov eax, [OLD+0x20]
    sub eax, [EXPECT_IP]
    call hex
    mov eax, [OLD+0x38]
    sub eax, [EXPECT_SP]
    call hex
    movzx eax, word [OLD+0x4c]
    call hex
    movzx eax, word [OLD+0x50]
    call hex
    movzx eax, byte [0x4005]
    call hex
    movzx eax, byte [0x400d]
    call hex
    mov eax, [0x9000]
    call hex
    mov eax, [0x8ffc]
    call hex
    mov eax, [SAVED_AD]
    call hex
    mov al, 10
    call putc
    cmp dword [SAVED_VECTOR], 14
    jne fail
    ; Continue the next independent probe in the observed original task.
    mov eax, [RESUME]
    mov [OLD+0x20], eax
    mov dword [OLD+0x4c], 8
    mov dword [OLD+0x50], 16
    mov dword [OLD+0x38], 0x9004
    iretd
    jmp fault                      ; this task resumes here on its next entry

putc:
    push eax
    push edx
    mov dx, 0x3fd
.wait:
    in al, dx
    test al, 0x20
    jz .wait
    mov eax, [esp+4]
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
    jbe .write
    add al, 7
.write:
    call putc
    loop .digit
    mov al, ' '
    call putc
    popad
    ret
row_msg: db 'FAR ',0
done_msg: db 'R86_FAR_TRANSFER_OK',10,0
fail_msg: db 'R86_FAR_TRANSFER_FAIL',10,0
times 0xfff0-($-$$) db 0xff
bits 16
    jmp 0xf000:0
times 0x10000-($-$$) db 0xff
