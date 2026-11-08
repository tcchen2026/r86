; Independent instruction-fault restart probe: r86 and Bochs p2_klamath.
; nasm -f bin tests/roms/fault_restart.asm -o /tmp/fault_restart.bin
; A guest handler repairs segment state and retries reads, a write, x87,
; a partly completed REP, and a partly fetched immediate. No skipped opcodes.
; Rows: case, vector, code, saved EIP delta, saved ESP delta, saved CF/OF.
; Deltas/code must be zero; CF/OF must remain 00000801. Seven rows then OK.
bits 16
org 0xf0000
%define GDT 0x4000
%define IDT 0x3000
%define EXPECT_IP 0x7000
%define EXPECT_SP 0x7004
%define EXPECT_VECTOR 0x7008
%define COUNT 0x700c
%define CASE 0x7010
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
    dq 0x00cf91000000ffff      ; 18: read-only data
    dq 0x004993000000ffff      ; 20: writable stack, limit 9ffffh
    dq 0x0040930000008100      ; 28: source ends after first REP byte
    dq 0x00cf9b000000ffff      ; 30: code; limit set before fetch probe
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
    mov esp, 0x90000
    mov dx, 0x3fb
    mov al, 3
    out dx, al
    xor eax, eax
    mov edi, IDT
    mov ecx, 0x6000/4
    rep stosd
    mov esi, boot_gdt
    mov edi, GDT
    mov ecx, (boot_gdt_end-boot_gdt)/4
    rep movsd
    lgdt [ram_gdt_ptr]
%macro gate 2
    mov eax, %2
    mov word [IDT+%1*8], ax
    mov word [IDT+%1*8+2], 8
    mov word [IDT+%1*8+4], 0x8e00
    shr eax, 16
    mov word [IDT+%1*8+6], ax
%endmacro
    gate 12, ss_entry
    gate 13, gp_entry
    lidt [idt_ptr]
    mov dword [0x8000], 0x11223344
    mov dword [0x8010], 0x3f800000
    mov dword [0x100000], 0x99aabbcc
    mov word [0x8100], 0xbbaa
%macro expect 3
    mov dword [EXPECT_IP], %1
    mov [EXPECT_SP], esp
    mov dword [EXPECT_VECTOR], %2
    mov dword [CASE], %3
    push dword 0x803
    popfd
%endmacro
    expect read_null_ds, 13, 1
    mov eax, 0
    mov ds, ax
    mov edx, 0xdeadbeef
read_null_ds:
    mov edx, [0x8000]           ; ModRM form, not the EAX moffs opcode
    cmp edx, 0x11223344
    jne fail

    expect read_prefixed_fs, 13, 2
    mov eax, 0
    mov fs, ax
    mov ebx, 0xcafebabe
read_prefixed_fs:
    mov bx, [fs:0x8000]
    cmp ebx, 0xcafe3344
    jne fail

    expect write_readonly_ds, 13, 3
    mov eax, 0x18
    mov ds, ax
    mov ebx, 0x12345678
write_readonly_ds:
    mov [0x8004], ebx
    cmp dword [0x8004], 0x12345678
    jne fail

    expect read_limited_ss, 12, 4
    mov eax, 0x20
    mov ss, ax
    mov eax, 0xdeadbeef
    mov ebp, 0x100000
read_limited_ss:
    mov eax, [ebp]
    cmp eax, 0x99aabbcc
    jne fail

    fninit
    expect read_x87, 13, 5
    mov eax, 0
    mov ds, ax
read_x87:
    fld dword [0x8010]
    fstp dword [0x8008]
    cmp dword [0x8008], 0x3f800000
    jne fail

    expect partial_repeat, 13, 6
    mov eax, 0x28
    mov ds, ax
    mov esi, 0x8100
    mov edi, 0x8200
    mov ecx, 2
partial_repeat:
    rep movsb
    cmp ecx, 0
    jne fail
    cmp esi, 0x8102
    jne fail
    cmp edi, 0x8202
    jne fail
    cmp word [0x8200], 0xbbaa
    jne fail

    mov eax, partial_fetch+2
    mov word [GDT+0x30], ax
    and eax, 0xf0000
    or eax, 0x00409b00
    mov dword [GDT+0x34], eax
    expect partial_fetch, 13, 7
    mov eax, 0xabcdffff
    jmp 0x30:partial_fetch
partial_fetch:
    mov ax, 0x1234             ; 66 B8 34 12: last byte exceeds old CS limit
    cmp eax, 0xabcd1234
    jne fail
    jmp 8:after_fetch
after_fetch:
    cmp dword [COUNT], 7
    jne fail
    mov esi, done_msg
    call puts
halt:
    hlt
    jmp halt
fail:
    mov esi, fail_msg
    call puts
    jmp halt

ss_entry:
    push dword 12
    jmp fault
gp_entry:
    push dword 13
fault:
    pushad
    ; Repair hidden data/stack caches in the guest. The interrupt's stack is
    ; inside the old SS limit, even when the faulting [EBP] operand is not.
    mov ax, 16
    mov ds, ax
    mov fs, ax
    mov ss, ax
    lea ebp, [esp+32]
    inc dword [COUNT]
    mov esi, row_msg
    call puts
    mov eax, [CASE]
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
    mov eax, [ebp+16]
    and eax, 0x801
    call hex
    cmp eax, 0x801
    jne fail
    mov al, 10
    call putc
    mov eax, [ebp]
    cmp eax, [EXPECT_VECTOR]
    jne fail
    cmp dword [ebp+4], 0
    jne fail
    mov eax, [ebp+8]
    cmp eax, [EXPECT_IP]
    jne fail
    lea eax, [ebp+20]
    cmp eax, [EXPECT_SP]
    jne fail
    cmp dword [CASE], 6
    jne .fetch
    cmp dword [esp+4], 0x8101  ; ESI: first element already completed
    jne fail
    cmp dword [esp], 0x8201    ; EDI
    jne fail
    cmp dword [esp+24], 1      ; ECX
    jne fail
    cmp word [0x8200], 0x00aa
    jne fail
.fetch:
    cmp dword [CASE], 7
    jne .return
    cmp dword [esp+28], 0xabcdffff
    jne fail
    mov dword [GDT+0x30], 0xffff
    mov dword [GDT+0x34], 0x00cf9b00 ; IRET reloads the repaired code descriptor
.return:
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
row_msg: db 'RESTART ', 0
done_msg: db 'R86_FAULT_RESTART_OK', 10, 0
fail_msg: db 'R86_FAULT_RESTART_FAIL', 10, 0
times 0xfff0-($-$$) db 0xff
bits 16
    jmp 0xf000:0
times 0x10000-($-$$) db 0xff
