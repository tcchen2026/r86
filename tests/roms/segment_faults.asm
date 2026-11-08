; Guest exception/retry probe for r86 and Bochs p2_klamath.
; nasm -f bin tests/roms/segment_faults.asm -o /tmp/segment_faults.bin
; Boot as a 64-KiB reset ROM, no BIOS services or disk needed.
; A handler repairs each absent/invalid descriptor and IRET retries the
; actual instruction. Check vector, selector TI, saved EIP, unchanged stack
; and destinations, including POP/LDS and CALL/RETF/IRET frames.
; Rows: case number, vector, error code, saved-EIP delta, saved-ESP delta.
; Both deltas must be zero. Completion: R86_SEGMENT_FAULTS_OK.
bits 16
org 0xf0000
%define GDT 0x4000
%define LDT 0x5000
%define IDT 0x3000
%define STATE 0x7000
%define EXPECT_IP STATE
%define EXPECT_SP (STATE+4)
%define EXPECT_VECTOR (STATE+8)
%define EXPECT_CODE (STATE+12)
%define FIX_ADDR (STATE+16)
%define FIX_ACCESS (STATE+20)
%define COUNT (STATE+24)
%define FAR_POINTER (STATE+32)
start:
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x9000
    lgdt [cs:boot_gdt_ptr-start]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp dword 8:protected
align 8
boot_gdt:
    dq 0, 0x00cf9b000000ffff, 0x00cf93000000ffff
    dq 0x000082005000001f      ; 18: LDT at 5000h
    dq 0x0000090060000067      ; 20: absent available 32-bit TSS
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
    mov esp, 0x90000
    mov dx, 0x3fb
    mov al, 3
    out dx, al
    xor eax, eax
    mov edi, IDT
    mov ecx, 0x5000/4
    rep stosd
    mov esi, boot_gdt
    mov edi, GDT
    mov ecx, (boot_gdt_end-boot_gdt)/4
    rep movsd
    lgdt [ram_gdt_ptr]
    mov ax, 0x18
    lldt ax
    mov dword [LDT+8], 0xffff
    mov dword [LDT+12], 0x00cf1300 ; 0c: absent flat data
    mov dword [LDT+16], 0xffff
    mov dword [LDT+20], 0x00cf1b00 ; 14: absent flat code
%macro gate 2
    mov eax, %2
    mov word [IDT+%1*8], ax
    mov word [IDT+%1*8+2], 8
    mov word [IDT+%1*8+4], 0x8e00
    shr eax, 16
    mov word [IDT+%1*8+6], ax
%endmacro
    gate 11, np_entry
    gate 12, ss_entry
    gate 13, gp_entry
    lidt [idt_ptr]
%macro expect 5
    mov dword [EXPECT_IP], %1
    mov [EXPECT_SP], esp
    mov dword [EXPECT_VECTOR], %2
    mov dword [EXPECT_CODE], %3
    mov dword [FIX_ADDR], %4
    mov dword [FIX_ACCESS], %5
%endmacro
    expect mov_ds, 11, 0x0c, LDT+13, 0x93
    mov eax, 0x0c
mov_ds:
    mov ds, ax
    mov ax, ds
    cmp ax, 0x0c
    jne fail
    mov ax, 16
    mov ds, ax

    and byte [LDT+13], 0x7f
    push dword 0x0c
    expect pop_ds, 11, 0x0c, LDT+13, 0x93
pop_ds:
    pop ds
    cmp esp, 0x90000
    jne fail
    mov ax, ds
    cmp ax, 0x0c
    jne fail
    mov ax, 16
    mov ds, ax

    and byte [LDT+13], 0x7f
    expect mov_ss, 12, 0x0c, LDT+13, 0x93
    mov eax, 0x0c
mov_ss:
    mov ss, ax
    mov ax, ss
    cmp ax, 0x0c
    jne fail
    mov ax, 16
    mov ss, ax

    and byte [LDT+13], 0x7f
    mov dword [FAR_POINTER], 0x12345678
    mov word [FAR_POINTER+4], 0x0c
    expect lds_eax, 11, 0x0c, LDT+13, 0x93
    mov eax, 0xdeadbeef
lds_eax:
    lds eax, [FAR_POINTER]
    cmp eax, 0x12345678
    jne fail
    mov ax, 16
    mov ds, ax

    expect call_far, 11, 0x14, LDT+21, 0x9b
call_far:
    call 0x14:far_subroutine
    cmp esp, 0x90000
    jne fail

    and byte [LDT+21], 0x7f
    expect jump_far, 11, 0x14, LDT+21, 0x9b
jump_far:
    jmp 0x14:jump_target
jump_target:
    jmp 8:after_jump
after_jump:
    and byte [LDT+21], 0x7f
    push dword 0x14
    push dword ret_target
    expect return_far, 11, 0x14, LDT+21, 0x9b
return_far:
    retf
ret_target:
    cmp esp, 0x90000
    jne fail
    jmp 8:after_return
after_return:
    and byte [LDT+21], 0x7f
    push dword 2
    push dword 0x14
    push dword iret_target
    expect return_interrupt, 11, 0x14, LDT+21, 0x9b
return_interrupt:
    iretd
iret_target:
    cmp esp, 0x90000
    jne fail
    jmp 8:after_iret
after_iret:
    ; A present data descriptor is invalid for a branch. Repair its type,
    ; not the instruction, so a wrong #NP or bad restart EIP cannot pass.
    mov byte [LDT+21], 0x93
    expect bad_code_type, 13, 0x14, LDT+21, 0x9b
bad_code_type:
    jmp 0x14:fixed_code_type
fixed_code_type:
    jmp 8:after_type
after_type:
    and byte [GDT+0x18+5], 0x7f
    expect load_ldt, 11, 0x18, GDT+0x18+5, 0x82
    mov eax, 0x18
load_ldt:
    lldt ax
    expect load_tr, 11, 0x20, GDT+0x20+5, 0x89
    mov eax, 0x20
load_tr:
    ltr ax
    cmp byte [GDT+0x20+5], 0x8b
    jne fail
    cmp dword [COUNT], 11
    jne fail
    mov esi, done_msg
    call puts
halt:
    hlt
    jmp halt
far_subroutine:
    retf
fail:
    mov esi, fail_msg
    call puts
    jmp halt

np_entry:
    push dword 11
    jmp fault
ss_entry:
    push dword 12
    jmp fault
gp_entry:
    push dword 13
fault:
    pushad
    lea ebp, [esp+32]
    inc dword [COUNT]
    mov esi, row_msg
    call puts
    mov eax, [COUNT]
    call hex
    mov eax, [ebp]
    call hex
    mov eax, [ebp+4]
    call hex
    mov eax, [ebp+8]
    sub eax, [EXPECT_IP]
    call hex
    lea eax, [ebp+20]
    sub eax, [EXPECT_SP]
    call hex
    mov al, 10
    call putc
    mov eax, [ebp]
    cmp eax, [EXPECT_VECTOR]
    jne fail
    mov eax, [ebp+4]
    cmp eax, [EXPECT_CODE]
    jne fail
    mov eax, [ebp+8]
    cmp eax, [EXPECT_IP]
    jne fail
    lea eax, [ebp+20]
    cmp eax, [EXPECT_SP]
    jne fail
    mov ax, ds
    cmp ax, 16
    jne fail
    mov ax, ss
    cmp ax, 16
    jne fail
    cmp dword [EXPECT_IP], lds_eax
    jne .repair
    cmp dword [esp+28], 0xdeadbeef ; saved EAX, not the uncommitted LDS offset
    jne fail
.repair:
    mov edi, [FIX_ADDR]
    mov eax, [FIX_ACCESS]
    mov [edi], al
    popad
    add esp, 8
    iretd

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
row_msg: db 'SEG ', 0
done_msg: db 'R86_SEGMENT_FAULTS_OK', 10, 0
fail_msg: db 'R86_SEGMENT_FAULTS_FAIL', 10, 0
times 0xfff0-($-$$) db 0xff
bits 16
    jmp 0xf000:0
times 0x10000-($-$$) db 0xff
