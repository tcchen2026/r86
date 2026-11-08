; Mixed 386/286 TSS transitions, checked with the same ROM in Bochs and r86.
; The 286 tasks deliberately use a 32-bit CS and a 16-bit stack: TSS format
; controls the saved image/error width independently of instruction size.
; Paging stays enabled throughout. Rows expose registers, flags, CR3, FS/GS,
; TR and backlinks, plus the error word and the saved fault instruction.
; nasm -f bin tests/roms/task16.asm -o /tmp/task16.bin
bits 16
org 0xf0000
start:
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x7000
    lgdt [cs:gdt_ptr-start]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp dword 8:protected
align 8
gdt:
    dq 0, 0x00cf9b000000ffff, 0x00cf93000000ffff
%macro descriptor 4
    dd ((%1 & 0xffff) << 16) | (%2 & 0xffff)
    dd (%1 & 0xff000000) | ((%1 >> 16) & 0xff) | (%3 << 8) | (%4 << 20)
%endmacro
    descriptor 0x6000, 0x67, 0x89, 0 ; 18: 386 task A
    descriptor 0x6100, 0x2b, 0x81, 0 ; 20: 286 task B
    descriptor 0x6200, 0x2b, 0x81, 0 ; 28: 286 task C
    descriptor 0xf0000, 0xffff, 0x9b, 4 ; 30: 32-bit code, IP fits a word
    descriptor 0, 0xffff, 0x93, 0      ; 38: 16-bit stack
    dd 0x00280000, 0x00008500          ; 40: task gate to C
gdt_end:
gdt_ptr:
    dw gdt_end-gdt-1
    dd gdt
ram_gdt_ptr:
    dw gdt_end-gdt-1
    dd 0x4000
idt_ptr:
    dw 0x7ff
    dd 0x3000

%macro task16_image 4
    mov word [%1+0x0e], %2-start
    mov word [%1+0x10], 3
    mov word [%1+0x12], %4
    mov word [%1+0x14], 0x2222
    mov word [%1+0x16], 0x3333
    mov word [%1+0x18], 0x4444
    mov word [%1+0x1a], %3
    mov word [%1+0x1c], 0x5555
    mov word [%1+0x1e], 0x6666
    mov word [%1+0x20], 0x7777
    mov word [%1+0x22], 0x10
    mov word [%1+0x24], 0x30
    mov word [%1+0x26], 0x38
    mov word [%1+0x28], 0x10
    mov word [%1+0x2a], 0
    mov word [%1+0x2c], 0xa5a5         ; outside this TSS, must not be saved
    mov word [%1+0x64], 1              ; not a trap field in this format
%endmacro

%macro dump_task 2
    mov [0x5500], eax
    mov [0x5504], esp
    pushfd
    pop dword [0x5508]
    and dword [0x5508], 0x00234001
    mov eax, cr3
    mov [0x550c], eax
    xor eax, eax
    mov ax, fs
    mov [0x5510], eax
    mov ax, gs
    mov [0x5514], eax
    str ax
    mov [0x5518], eax
    mov ax, [%2]
    mov [0x551c], eax
    mov esi, %1
    call puts
    mov edi, 0x5500
    mov ecx, 8
%%word:
    mov eax, [edi]
    call hex
    add edi, 4
    loop %%word
    mov al, 10
    call putc
%endmacro

bits 32
protected:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    mov esp, 0x90000
    mov dx, 0x3fb
    mov al, 3
    out dx, al
    mov edi, 0x1000
    xor eax, eax
    mov ecx, 0x6000/4
    rep stosd
    mov esi, gdt
    mov edi, 0x4000
    mov ecx, (gdt_end-gdt)/4
    rep movsd
    lgdt [ram_gdt_ptr]
    lidt [idt_ptr]
    mov dword [0x1000], 0x2003
    mov edi, 0x2000
    mov eax, 3
    mov ecx, 256
.page:
    stosd
    add eax, 0x1000
    loop .page
    mov eax, 0x1000
    mov cr3, eax
    mov dword [0x601c], eax
    mov eax, cr0
    or eax, 0x80010000
    mov cr0, eax
    mov ax, 0x18
    ltr ax
    task16_image 0x6100, task_b, 0x8000, 0x1234
    task16_image 0x6200, task_c, 0x7000, 0x5678
    mov esi, begin_msg
    call puts
    push dword 0x00200003
    popfd
    mov eax, 0xcafebabe
    call 0x20:0
after_call:
    dump_task ret_msg, 0x6000
    cmp dword [0x5500], 0xcafebabe
    jne fail
    cmp dword [0x5504], 0x90000
    jne fail
    cmp dword [0x550c], 0x1000
    jne fail
    cmp word [0x610e], after_b_iret-start
    jne fail
    cmp byte [0x4025], 0x81
    jne fail
    cmp byte [0x402d], 0x81
    jne fail
    cmp word [0x612c], 0xa5a5
    jne fail
    task16_image 0x6100, jump_b, 0x8000, 0x1234
    mov word [0x6100], 0x7777
    mov eax, 0xabcdef01
    jmp 0x20:0
after_jump:
    cmp eax, 0xabcdef01
    jne fail
    cmp word [0x610e], after_b_jump-start
    jne fail
    cmp byte [0x4025], 0x81
    jne fail
    task16_image 0x6200, error_c, 0x7000, 0x5678
    mov dword [0x6ffc], 0xa5a5a5a5
    mov dword [0x3068], 0x00280000
    mov dword [0x306c], 0x00008500
    mov ax, 0x80
fault:
    mov ds, ax
after_fault:
    mov esi, done_msg
    call puts
finish:
    mov dx, 0x3fd
.drain:
    in al, dx
    test al, 0x40
    jz .drain
    xchg bx, bx                     ; Bochs magic breakpoint, then debugger quit
    cli
.halt:
    hlt
    jmp .halt

task_b:
    dump_task b_msg, 0x6100
    cmp dword [0x5500], 0xffff1234
    jne fail
    cmp dword [0x5504], 0xffff8000
    jne fail
    cmp dword [0x5508], 0x4001
    jne fail
    cmp dword [0x550c], 0x1000
    jne fail
    cmp dword [0x5510], 0
    jne fail
    cmp dword [0x5514], 0
    jne fail
    cmp word [0x6100], 0x18
    jne fail
    mov eax, 0xcafe1111
    call 0x40:0                     ; task gate to another 286 task
    cmp eax, 0xffff1111              ; only AX was saved in the outgoing TSS
    jne fail
    cmp word [0x620e], after_c_iret-start
    jne fail
    iret
after_b_iret:
    ud2

task_c:
    dump_task c_msg, 0x6200
    cmp dword [0x5500], 0xffff5678
    jne fail
    cmp word [0x6200], 0x20
    jne fail
    iret
after_c_iret:
    ud2

jump_b:
    dump_task j_msg, 0x6100
    cmp dword [0x5508], 1
    jne fail
    cmp byte [0x401d], 0x89
    jne fail
    cmp word [0x6100], 0x7777
    jne fail
    jmp 0x18:0
after_b_jump:
    ud2

error_c:
    ; Capture the entry stack before diagnostic calls overwrite its free area.
    mov [0x5520], eax
    movzx eax, word [0x6ffe]
    mov [0x5524], eax
    movzx eax, word [0x6ffc]
    mov [0x5528], eax
    mov eax, [0x5520]
    dump_task e_msg, 0x6200
    cmp dword [0x5504], 0xffff6ffe
    jne fail
    mov esi, error_msg
    call puts
    mov eax, [0x5524]
    call hex
    cmp eax, 0x80
    jne fail
    mov eax, [0x5528]
    call hex
    cmp eax, 0xa5a5
    jne fail
    mov eax, [0x6020]
    sub eax, fault
    call hex
    test eax, eax
    jnz fail
    mov al, 10
    call putc
    add sp, 2
    mov dword [0x6020], after_fault
    iret
    ud2

fail:
    mov esi, fail_msg
    call puts
    jmp finish
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
begin_msg: db 'R86_TASK16_BEGIN', 10, 0
b_msg: db 'B16 ', 0
c_msg: db 'C16 ', 0
ret_msg: db 'RET32 ', 0
j_msg: db 'J16 ', 0
e_msg: db 'ERR16 ', 0
error_msg: db 'ERROR ', 0
done_msg: db 'R86_TASK16_OK', 10, 0
fail_msg: db 'R86_TASK16_FAIL', 10, 0
times 0xfff0-($-$$) db 0xff
bits 16
    jmp 0xf000:0
times 0x10000-($-$$) db 0xff
