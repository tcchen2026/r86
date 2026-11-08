; 64 KiB diagnostic BIOS: compare r86 with Bochs p2_klamath.
; nasm -f bin tests/roms/exception_delivery.asm -o /tmp/exception_delivery.bin
; Load at FFFF0000h in Bochs (low alias F0000h); r86 --bios
; infers the same mapping. COM1 must print:
;   NP 00000033 00010000
;   NP 00000042 00010000
;   DF 00000000
;   R86_EXCEPTION_RESET_OK
; #UD + non-present gate -> #NP with EXT; INT 8 + non-present gate -> #NP
; without EXT; #DE + non-present gate -> #DF. Finally an unusable IDT causes
; CPU shutdown and a board reset. A RAM marker distinguishes the second boot.
; No guest OS, IDT fault recovery shortcut, or persisted emulator state.
bits 16
org 0xf0000

start:
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x7000
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
    cmp dword [0x600], 0x8657a112
    jne .first
    mov si, reset_msg - start
    mov dx, 0x3f8
.print:
    cs lodsb
    test al, al
    jz .done
    push ax
    mov dx, 0x3fd
.wait:
    in al, dx
    test al, 0x20
    jz .wait
    mov dx, 0x3f8
    pop ax
    out dx, al
    jmp .print
.done:
    hlt
    jmp .done
.first:
    lgdt [cs:gdt_ptr - start]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp dword 8:protected

align 8
gdt:
    dq 0, 0x00cf9b000000ffff, 0x00cf93000000ffff
gdt_ptr:
    dw 23
    dd gdt
idt_ptr:
    dw 0x7ff
    dd 0x3000
empty_idt:
    dw 0
    dd 0
reset_msg: db 'R86_EXCEPTION_RESET_OK', 10, 0

bits 32
protected:
    mov ax, 16
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov esp, 0x90000
    mov edi, 0x3000
    xor eax, eax
    mov ecx, 512
    rep stosd
    ; Well-formed interrupt gates with P=0, rather than invalid gate types.
    mov dword [0x3004], 0x00000e00
    mov dword [0x3034], 0x00000e00
    mov dword [0x3044], 0x00000e00
    mov eax, np_handler
    mov [0x3058], ax
    mov word [0x305a], 8
    mov word [0x305c], 0x8e00
    shr eax, 16
    mov [0x305e], ax
    lidt [idt_ptr]
    mov dword [0x604], .after_ud
    ud2
.after_ud:
    mov dword [0x604], .after_int
    int 8
.after_int:
    mov eax, df_handler
    mov [0x3040], ax
    mov word [0x3042], 8
    mov word [0x3044], 0x8e00
    shr eax, 16
    mov [0x3046], ax
    xor ecx, ecx
    div ecx
    ud2                         ; Unreachable if #DF was entered.

np_handler:
    mov esi, np_msg
    call puts
    mov eax, [esp]
    call hex
    mov eax, [esp+12]
    and eax, 0x10000             ; RF in the saved fault frame.
    call hex
    call newline
    mov eax, [0x604]
    mov [esp+4], eax
    add esp, 4
    iretd

df_handler:
    mov esi, df_msg
    call puts
    mov eax, [esp]
    call hex
    call newline
    mov dx, 0x3fd
.drain:
    in al, dx
    test al, 0x40
    jz .drain
    mov dword [0x600], 0x8657a112
    lidt [empty_idt]
    ud2                         ; #UD -> #GP -> #DF -> CPU shutdown.

puts:
    push eax
    push edx
    mov dx, 0x3f8
.next:
    lodsb
    test al, al
    jz .done
    call putc
    jmp .next
.done:
    pop edx
    pop eax
    ret
hex:
    pushad
    mov ebx, eax
    mov ecx, 8
    mov dx, 0x3f8
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
    push eax
    push edx
    mov dx, 0x3f8
    mov al, 10
    call putc
    pop edx
    pop eax
    ret
putc:
    push eax
    push edx
    mov dx, 0x3fd
.wait:
    in al, dx
    test al, 0x20
    jz .wait
    mov dx, 0x3f8
    mov eax, [esp+4]
    out dx, al
    pop edx
    pop eax
    ret
np_msg: db 'NP ', 0
df_msg: db 'DF ', 0

times 0xfff0-($-$$) db 0xff
bits 16
    jmp 0xf000:0
times 0x10000-($-$$) db 0xff
