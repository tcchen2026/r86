; ENTER/LEAVE with a 32-bit CS and 16-bit SS, independent of TSS switching.
; nasm -f bin tests/roms/word_stack.asm -o /tmp/word_stack.bin
; Compare the same BIOS at F0000h in r86 and Bochs p2_klamath.
; Each row: entered ESP/EBP, saved BP, copied outer link, frame link,
; returned ESP/EBP. Distinct upper halves expose address/operand-size mixing.
; The final allocation wraps SP across zero without changing ESP's high half.
bits 16
org 0xf0000
start:
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x9000
    lgdt [cs:gdt_ptr-start]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp dword 8:protected
align 8
gdt:
    dq 0, 0x00cf9b000000ffff, 0x00cf93000000ffff, 0x000093000000ffff
gdt_ptr:
    dw gdt_ptr-gdt-1
    dd gdt

%macro capture_frame 2
    mov [0x5000], esp
    mov [0x5004], ebp
    movzx edi, bp
%if %1 == 2
    movzx eax, word [edi]
%else
    mov eax, [edi]
%endif
    mov [0x5008], eax
    mov dword [0x500c], 0
    mov dword [0x5010], 0
%if %2 == 2
%if %1 == 2
    movzx eax, word [edi-2]
    movzx edx, word [edi-4]
%else
    mov eax, [edi-4]
    mov edx, [edi-8]
%endif
    mov [0x500c], eax
    mov [0x5010], edx
%endif
%endmacro

%macro report 8
    mov [0x5014], esp
    mov [0x5018], ebp
    mov esi, %1
    call puts
    mov edi, 0x5000
    mov ecx, 7
%%next:
    mov eax, [edi]
    call hex
    add edi, 4
    loop %%next
    mov al, 10
    call putc
    cmp dword [0x5000], %2
    jne fail
    cmp dword [0x5004], %3
    jne fail
    cmp dword [0x5008], %4
    jne fail
    cmp dword [0x500c], %5
    jne fail
    cmp dword [0x5010], %6
    jne fail
    cmp dword [0x5014], %7
    jne fail
    cmp dword [0x5018], %8
    jne fail
%endmacro

bits 32
protected:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ax, 0x18
    mov ss, ax
    mov esp, 0x9000
    mov dx, 0x3fb
    mov al, 3
    out dx, al
    mov esi, begin_msg
    call puts

    mov esp, 0xabcd8000
    mov ebp, 0x12349000
    mov dword [0x8ffc], 0xdeadbeef
    enter 8, 2
    capture_frame 4, 2
    leave
    report dword_msg, 0xabcd7fec, 0xabcd7ffc, 0x12349000, 0xdeadbeef, 0xabcd7ffc, 0xabcd8000, 0x12349000

    mov esp, 0xcafe7000
    mov ebp, 0x98766000
    mov word [0x5ffe], 0xbeef
    o16 enter 8, 2
    capture_frame 2, 2
    o16 leave
    report word_msg, 0xcafe6ff2, 0x98766ffe, 0x6000, 0xbeef, 0x6ffe, 0xcafe7000, 0x98766000

    mov esp, 0xface0020
    mov ebp, 0xfeed0040
    enter 0x40, 0
    capture_frame 4, 0
    leave
    report wrap_msg, 0xfaceffdc, 0xface001c, 0xfeed0040, 0, 0, 0xface0020, 0xfeed0040
    mov esi, done_msg
    call puts
    jmp finish
fail:
    mov esi, fail_msg
    call puts
finish:
    mov dx, 0x3fd
.drain:
    in al, dx
    test al, 0x40
    jz .drain
    cli
.halt:
    hlt
    jmp .halt
putc:
    push eax
    push edx
    mov ah, al
    mov dx, 0x3fd
.wait:
    in al, dx
    test al, 0x20
    jz .wait
    mov al, ah
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
    mov al, bl
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
begin_msg: db 'R86_WORD_STACK_BEGIN', 10, 0
dword_msg: db 'DWORD ', 0
word_msg: db 'WORD ', 0
wrap_msg: db 'WRAP ', 0
done_msg: db 'R86_WORD_STACK_OK', 10, 0
fail_msg: db 'R86_WORD_STACK_FAIL', 10, 0
times 0xfff0-($-$$) db 0xff
bits 16
    jmp 0xf000:0
times 0x10000-($-$$) db 0xff
