; Independent 32-bit task-entry BIOS, for r86/Bochs p2_klamath comparison.
; nasm -f bin tests/roms/task_entry.asm -o /tmp/task_entry.bin
; Boot at FFFF0000h in Bochs, or r86 --bios FILE.
; Task B uses a new LDT and an invalid DS. Its #TS enters task C. COM1 reports
; C's backlink/error code, saved B EIP delta, saved A EIP delta, CR3 and saved
; B selectors. This distinguishes a fault before vs after task commitment.
; A's GDT TSS base is changed after LTR, testing the hidden TR cache too.
; Expected row (Bochs 3.0, r86 interpreter and RPi JIT):
; TASK 00000020 0000001C 00000000 00000000 00001000 0000000C 00000014 0000001C
; R86_TASK_ORACLE_DONE
bits 16
org 0xf0000

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
boot_gdt:
    dq 0, 0x00cf9b000000ffff, 0x00cf93000000ffff
%macro system_descriptor 3
    dd ((%1 & 0xffff) << 16) | %2
    dd (%1 & 0xff000000) | ((%1 >> 16) & 0xff) | (%3 << 8)
%endmacro
    system_descriptor 0x6000, 0x67, 0x89 ; 18: A
    system_descriptor 0x6100, 0x67, 0x89 ; 20: B
    system_descriptor 0x6200, 0x67, 0x89 ; 28: C
    system_descriptor 0x5000, 0x17, 0x82 ; 30: LDT
boot_gdt_end:
boot_gdt_ptr:
    dw boot_gdt_end-boot_gdt-1
    dd boot_gdt
ram_gdt_ptr:
    dw boot_gdt_end-boot_gdt-1
    dd 0x4000
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
    mov edi, 0x3000
    xor eax, eax
    mov ecx, 0x4000/4
    rep stosd
    mov esi, boot_gdt
    mov edi, 0x4000
    mov ecx, (boot_gdt_end-boot_gdt)/4
    rep movsd
    mov esi, boot_gdt
    mov edi, 0x5000
    mov ecx, 6
    rep movsd
    lgdt [ram_gdt_ptr]
    mov ax, 0x18
    ltr ax
    mov word [0x401a], 0x6400     ; Shadow TR must still name 6000h.
    mov eax, 0x1000
    mov cr3, eax                 ; PG=0: observe whether task switching changes it.

%macro initialize_task 5
    mov dword [%1+0x1c], %5
    mov dword [%1+0x20], %2
    mov dword [%1+0x24], 2
    mov dword [%1+0x38], 0x80000
    mov word [%1+0x48], %4
    mov word [%1+0x4c], %3
    mov word [%1+0x50], %4
    mov word [%1+0x54], %4
    mov word [%1+0x58], %4
    mov word [%1+0x5c], %4
%endmacro
    initialize_task 0x6100, task_b, 0x0c, 0x14, 0x2000
    mov word [0x6160], 0x30      ; Incoming LDTR.
    mov word [0x6154], 0x1c      ; Invalid DS: beyond the new LDT limit.
    initialize_task 0x6200, task_c, 8, 16, 0x3000
    mov dword [0x3180], 0x00200000
    mov dword [0x3184], 0x00008500
    mov dword [0x3050], 0x00280000
    mov dword [0x3054], 0x00008500
    lidt [idt_ptr]
    int 0x30
after_int:
    ud2

task_b:
    ud2                         ; DS qualification must fault before execution.

task_c:
    mov ebp, [esp]
    mov esi, result_msg
    call puts
    movzx eax, word [0x6200]
    call hex
    mov eax, ebp
    call hex
    mov eax, [0x6120]
    sub eax, task_b
    call hex
    mov eax, [0x6020]
    sub eax, after_int
    call hex
    mov eax, cr3
    call hex
    movzx eax, word [0x614c]
    call hex
    movzx eax, word [0x6150]
    call hex
    movzx eax, word [0x6154]
    call hex
    mov esi, done_msg
    call puts
.halt:
    hlt
    jmp .halt

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
result_msg: db 'TASK ', 0
done_msg: db 10, 'R86_TASK_ORACLE_DONE', 10, 0

times 0xfff0-($-$$) db 0xff
bits 16
    jmp 0xf000:0
times 0x10000-($-$$) db 0xff
