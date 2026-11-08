; Independent task-trap ROM for r86 and Bochs p2_klamath.
; nasm -f bin tests/roms/task_trap.asm -o /tmp/task_trap.bin
; A CALL enters task B with T=1. B then causes #DF through invalid IDT
; entries; #DF enters task C with T=1. Each #DB must precede the task's first
; instruction and IRET must return to that instruction, without a reset.
; Rows: TR, entry-EIP delta, DR6 status, saved NT/RF, DR7 GD/local enables.
; Expected: TRAP 00000020 00000000 00008000 00004000 00000000
;           TRAP 00000028 00000000 00008000 00004000 00000000
;           R86_TASK_TRAP_OK
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
%macro tss_descriptor 1
    dd ((%1 & 0xffff) << 16) | 0x67
    dd (%1 >> 16) | 0x8900
%endmacro
    tss_descriptor 0x6000       ; 18: current A
    tss_descriptor 0x6100       ; 20: CALL target B
    tss_descriptor 0x6200       ; 28: #DF target C
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
    mov al, 3
    out dx, al
    xor eax, eax
    mov edi, 0x3000
    mov ecx, 0x4000/4
    rep stosd
    mov esi, boot_gdt
    mov edi, 0x4000
    mov ecx, (boot_gdt_end-boot_gdt)/4
    rep movsd
    lgdt [ram_gdt_ptr]
    mov ax, 0x18
    ltr ax
%macro task_image 3
    mov dword [%1+0x20], %2
    mov dword [%1+0x24], 2
    mov dword [%1+0x38], %3
    mov word [%1+0x48], 16
    mov word [%1+0x4c], 8
    mov word [%1+0x50], 16
    mov word [%1+0x54], 16
    mov word [%1+0x64], 1
%endmacro
    task_image 0x6100, task_b, 0x80000
    task_image 0x6200, task_c, 0x70000
    mov eax, db_entry
    mov word [0x3008], ax
    mov word [0x300a], 8
    mov word [0x300c], 0x8e00
    shr eax, 16
    mov word [0x300e], ax
    mov dword [0x3040], 0x00280000
    mov dword [0x3044], 0x00008500 ; #DF task gate
    lidt [idt_ptr]
    mov eax, 0x2500
    mov dr7, eax
    call 0x20:0
    ud2

task_b:
    cmp dword [0x5500], 1
    jne fail
    xor eax, eax
    mov dr6, eax
    mov eax, 0x2500
    mov dr7, eax
    int 0x30                    ; invalid gate -> #GP, invalid #GP -> #DF task C
    ud2
task_c:
    cmp dword [0x5500], 2
    jne fail
    cmp dword [esp], 0           ; original #DF error code remains below #DB frame
    jne fail
    mov esi, done_msg
    call puts
.halt:
    hlt
    jmp .halt
fail:
    mov esi, fail_msg
    call puts
    jmp task_c.halt

db_entry:
    mov ebp, esp
    inc dword [0x5500]
    mov esi, trap_msg
    call puts
    xor eax, eax
    str ax
    call hex
    mov ecx, task_b
    cmp ax, 0x20
    je .entry
    mov ecx, task_c
.entry:
    mov eax, [ebp]
    sub eax, ecx
    call hex
    mov eax, dr6
    and eax, 0xe00f
    call hex
    mov eax, [ebp+8]
    and eax, 0x14000             ; NT and RF
    call hex
    mov eax, dr7
    and eax, 0x2155              ; GD and local enables cleared
    call hex
    mov al, 10
    call putc
    iret

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
trap_msg: db 'TRAP ', 0
done_msg: db 'R86_TASK_TRAP_OK', 10, 0
fail_msg: db 'R86_TASK_TRAP_FAIL', 10, 0
times 0xfff0-($-$$) db 0xff
bits 16
    jmp 0xf000:0
times 0x10000-($-$$) db 0xff
