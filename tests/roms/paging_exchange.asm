; Standalone 64 KiB diagnostic BIOS for Bochs/r86 comparisons.
; Build: nasm -f bin tests/roms/paging_exchange.asm -o /tmp/paging_exchange.bin
; Load as the BIOS at F0000h. COM1 prints values and defined status flags.
; Tests XADD operand aliasing, CMPXCHG write faults (both compare outcomes),
; a protected directory entry with a present/missing leaf, and REP SCAS
; partial progress before a page fault. No OS is used.
; Compare the full serial transcript, ending with R86_ORACLE_DONE.
; Bochs romimage address is FFFF0000h (its low-memory alias is F0000h).
; QEMU -bios and r86 --bios infer the reset ROM mapping.
; Expected XADD results/status: 123456F0/894, 1234FFFE/91, FFFFFFFE/890.
; All six CMPXCHGs fault with PFEC=3, retain EAX, flags=8D5 and memory=12345678.
; The two PDE cases give PFEC=3 then PFEC=2. REP SCAS gives PFEC=0, ECX=2,
; EDI=9000h and defined status=44h. Full transcripts matched Bochs
; 3.0 p2_klamath, QEMU pentium2 and r86 interpreter.
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

%macro alias_test 3
    mov esi, %1
    call puts
    mov eax, %2
    xadd %3, %3
    pushfd
    pop edx
    call hex
    mov eax, edx
    and eax, 0x8d5
    call hex
    call newline
%endmacro
    alias_test byte_msg, 0x12345678, al
    alias_test word_msg, 0x1234ffff, ax
    alias_test long_msg, 0x7fffffff, eax

    mov dword [0x8000], 0x12345678
    mov dword [0x2020], 0x8001
    mov eax, cr3
    mov cr3, eax
%macro compare_test 2
    mov esi, compare_msg
    call puts
    mov dword [0x5000], -1
    mov dword [0x5004], %%resume
    mov eax, 0x12345678 ^ %2
    mov ecx, 0xfedcba98
    push dword 0x8d7
    popfd
    lock cmpxchg [0x8000], %1
%%resume:
    mov ebx, eax
    mov eax, [0x5000]
    call hex
    mov eax, ebx
    call hex
    mov eax, [0x5008]
    and eax, 0x8d5
    call hex
    mov eax, [0x8000]
    call hex
    call newline
%endmacro
    compare_test cl, 1
    compare_test cl, 0
    compare_test cx, 1
    compare_test cx, 0
    compare_test ecx, 1
    compare_test ecx, 0

    mov esi, directory_msg
    call puts
    mov dword [0x1004], 0x4001
    mov dword [0x4000], 0x8003
    mov eax, cr3
    mov cr3, eax
    mov dword [0x5004], .present
    mov dword [0x400000], 0
.present:
    mov eax, [0x5000]
    call hex
    mov dword [0x4000], 0
    mov eax, cr3
    mov cr3, eax
    mov dword [0x5004], .missing
    mov dword [0x400000], 0
.missing:
    mov eax, [0x5000]
    call hex
    call newline

    ; Two SCAS elements commit before the next page faults. Report the
    ; restart count/index and the saved exception-frame status flags.
    mov dword [0x2020], 0x8003
    mov dword [0x2024], 0
    mov eax, cr3
    mov cr3, eax
    mov dword [0x8ff8], 0x7f7f7f7f
    mov dword [0x8ffc], 0x7f7f7f7f
    mov esi, scan_msg
    call puts
    mov dword [0x5004], .scan_fault
    mov eax, 0x7f7f7f7f
    mov edi, 0x8ff8
    mov ecx, 4
    push dword 0x8d7
    popfd
    repe scasd
.scan_fault:
    mov eax, [0x5000]
    call hex
    mov eax, ecx
    call hex
    mov eax, edi
    call hex
    mov eax, [0x5008]
    and eax, 0x8d5
    call hex
    call newline
    mov esi, done_msg
    call puts
.halt:
    hlt
    jmp .halt

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

byte_msg: db 'XADD8 ', 0
word_msg: db 'XADD16 ', 0
long_msg: db 'XADD32 ', 0
compare_msg: db 'CMPXCHG ', 0
directory_msg: db 'PDE ', 0
scan_msg: db 'REP_SCAS ', 0
done_msg: db 'R86_ORACLE_DONE', 10, 0

times 0xfff0 - ($ - $$) db 0xff
bits 16
    jmp 0xf000:0
times 0x10000 - ($ - $$) db 0xff
