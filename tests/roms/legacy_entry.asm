; Independent real-mode/VME interrupt-frame probe.
; nasm -f bin tests/roms/legacy_entry.asm -o /tmp/legacy_entry.bin
; Load as a 64 KiB BIOS in r86 or Bochs p2_klamath. Compare all COM1 rows.
; REAL uses the reset IVT (no LIDT) and aliases it with its stack.
; VME faults on the second frame word;
; a task-gate observer sees the first write, repairs the page, and retries INT.
; RETRY observes the completed frame and cleared virtual IF from a second task
; entry. No observer uses the interrupted stack. Ends R86_LEGACY_ENTRY_OK.
bits 16
org 0xf0000
%define GDT 0x5000
%define IDT 0x4000
%define OLD 0x6000
%define FAULT 0x6100

start:
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x9000
    mov dword [0x7000], 0
    mov dword [0x7004], 0
    mov si, alias_stub-start
    mov di, 0x120
    mov cx, alias_end-alias_stub
    cs rep movsb
    mov word [0x1000], 0x30cd
    mov word [0xc0], after_alias-start
    mov word [0xc2], 0xf000
    push word 2
    popf
    mov esp, 0xabcd00c4
    jmp 0x100:0
alias_stub:
    mov [0x7000], cs
    mov [0x7004], esp
    jmp 0xf000:after_alias-start
alias_end:
after_alias:
    mov esp, 0x9000
    lgdt [cs:boot_gdt_ptr-start]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp dword 8:protected
align 8
%macro tss_descriptor 2
    dd ((%1 & 0xffff) << 16) | %2
    dd 0x00008900 | ((%1 >> 16) & 0xff)
%endmacro
boot_gdt:
    dq 0, 0x00cf9b000000ffff, 0x00cf93000000ffff
    tss_descriptor OLD, 0x88
    tss_descriptor FAULT, 0x67
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
    mov esp, 0x80000
    mov dx, 0x3fb
    mov al, 3
    out dx, al
    mov esi, real_msg
    call puts
    mov eax, [0x7000]
    call hex
    mov eax, [0x7004]
    call hex
    movzx eax, word [0xbe]
    call hex
    movzx eax, word [0xc0]
    call hex
    movzx eax, word [0xc2]
    call hex
    call newline
    cmp dword [0x7000], 2
    jne fail
    cmp dword [0x7004], 0xabcd00be
    jne fail

    xor eax, eax
    mov edi, 0x2000
    mov ecx, 0x4800/4
    rep stosd
    mov esi, boot_gdt
    mov edi, GDT
    mov ecx, (boot_gdt_end-boot_gdt)/4
    rep movsd
    lgdt [ram_gdt_ptr]
    mov dword [OLD+0x1c], 0x2000
    mov word [OLD+0x66], 0x88
    mov byte [OLD+0x6c], 4 ; redirect INT 21h; INT 22h must raise #GP
    mov dword [FAULT+0x1c], 0x2000
    mov dword [FAULT+0x20], fault
    mov dword [FAULT+0x24], 2
    mov dword [FAULT+0x38], 0xa0000
    mov dword [FAULT+0x48], 16
    mov dword [FAULT+0x4c], 8
    mov dword [FAULT+0x50], 16
    mov dword [FAULT+0x54], 16
    mov dword [FAULT+0x58], 16
    mov dword [FAULT+0x5c], 16
    mov word [IDT+14*8+2], 0x20
    mov word [IDT+13*8+2], 0x20
    mov word [IDT+14*8+4], 0x8500
    mov word [IDT+13*8+4], 0x8500
    lidt [idt_ptr]
    mov ax, 0x18
    ltr ax
    mov esi, vm_code
    mov edi, 0x90000
    mov ecx, vm_end-vm_code
    rep movsb
    mov dword [0x84], 0x90000010
    mov word [0x9000], 0xaaaa
    mov word [0x8ffe], 0xbbbb
    mov word [0x8ffc], 0xcccc
    mov word [0x702c], 0
    mov edi, 0x3000
    mov eax, 7
    mov ecx, 1024
.map:
    stosd
    add eax, 0x1000
    loop .map
    mov dword [0x2000], 0x3007
    mov dword [0x3020], 0 ; first push lands at 9000h; second faults at 8ffeh
    mov eax, 0x2000
    mov cr3, eax
    mov eax, 1
    mov cr4, eax
    mov eax, cr0
    or eax, 0x80010000
    mov cr0, eax
    push dword 0 ; GS/FS/DS/ES
    push dword 0
    push dword 0
    push dword 0
    push dword 0x800
    push dword 0xabcd1002
    push dword 0xa0803 ; VM, VIF, OF, CF
    push dword 0x9000
    push dword 0
    iretd
    jmp fail
fault:
    pop ebp
    mov esi, vme_msg
    call puts
    mov eax, ebp
    call hex
    mov eax, cr2
    call hex
    mov eax, [OLD+0x20]
    call hex
    mov eax, [OLD+0x38]
    call hex
    movzx eax, word [0x9000]
    call hex
    call newline
    cmp ebp, 6
    jne fail
    mov eax, cr2
    cmp eax, 0x8ffe
    jne fail
    cmp dword [OLD+0x20], 0
    jne fail
    cmp dword [OLD+0x38], 0xabcd1002
    jne fail
    cmp word [0x9000], 0x3a03
    jne fail
    mov dword [0x3020], 0x8007
    mov eax, cr3
    mov cr3, eax
    iretd
    ; The same task is available again; its second entry resumes here.
    pop ebp
    mov esi, retry_msg
    call puts
    mov eax, ebp
    call hex
    mov eax, [OLD+0x38]
    call hex
    mov eax, [OLD+0x24]
    and eax, 0x80000
    call hex
    movzx eax, word [0x8ffc]
    call hex
    movzx eax, word [0x8ffe]
    call hex
    movzx eax, word [0x9000]
    call hex
    call newline
    test ebp, ebp
    jnz fail
    cmp word [0x702c], 0x1dea
    jne fail
    cmp dword [OLD+0x38], 0xabcd0ffc
    jne fail
    test dword [OLD+0x24], 0x80000
    jnz fail
    cmp word [0x8ffc], 2
    jne fail
    cmp word [0x8ffe], 0x9000
    jne fail
    cmp word [0x9000], 0x3a03
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
newline:
    mov al, 10
    jmp putc
real_msg: db 'REAL ',0
vme_msg: db 'VME ',0
retry_msg: db 'RETRY ',0
done_msg: db 'R86_LEGACY_ENTRY_OK',10,0
fail_msg: db 'R86_LEGACY_ENTRY_FAIL',10,0
bits 16
vm_code:
    int 0x21
    hlt
    times 16-($-vm_code) db 0x90
    mov word [0x702c], 0x1dea
    int 0x22
    hlt
vm_end:
times 0xfff0-($-$$) db 0xff
    jmp 0xf000:0
times 0x10000-($-$$) db 0xff
