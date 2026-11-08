; Standalone 64 KiB BIOS: cached translations across ordinary page-table writes.
; nasm -f bin tests/roms/tlb_lifetime.asm -o /tmp/tlb_lifetime.bin
; Use Bochs p2_klamath (ROM at FFFF0000h), QEMU pentium2, or r86 --bios.
; Retaining a translation before architectural invalidation is permitted, not
; mandatory: eviction may make an emulator observe the new PTE earlier. These
; deliberately small sequences exercise the retained entries of our models.
; MAP: 11111111 11111111 22222222; WRITE: 00000003 44444444;
; ACCESSED: 00000000 00000020; CR3: 11111111.
; CONTROL: 11111111 11111111 44444444 11111111 (TS, TSD, WP, PGE).
; GLOBAL: 11111111 55555555 44444444 11111111 (CR3, nonglobal, INVLPG, PGE clear).
; Bochs and r86 retain GLOBAL's first mapping. QEMU TCG may instead evict it
; on CR3 and print 44444444 there; extra TLB eviction is permitted.
; COM1 ends with R86_TLB_DONE. The ordinary paging/exchange ROM covers precise
; page faults separately; this ROM checks the invalidation contract itself.
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
    mov dword [0x2010], 0x8003
    mov dword [0x8000], 0x11111111
    mov dword [0x9000], 0x22222222
    mov eax, 0x1000
    mov cr3, eax
    mov eax, cr0
    or eax, 0x80010000
    mov cr0, eax

    mov eax, [0x4000]
    mov [0x5020], eax
    mov dword [0x2010], 0x9003
    mov eax, [0x4000]
    mov [0x5024], eax
    invlpg [0x4000]
    mov eax, [0x4000]
    mov [0x5028], eax
    mov esi, map_msg
    call puts
    mov eax, [0x5020]
    call hex
    mov eax, [0x5024]
    call hex
    mov eax, [0x5028]
    call hex
    call newline

    mov dword [0x4000], 0x33333333
    mov dword [0x2010], 0x9061
    mov dword [0x4000], 0x44444444
    invlpg [0x4000]
    mov dword [0x5000], -1
    mov dword [0x5004], .write_fault
    mov dword [0x4000], 0x55555555
.write_fault:
    mov esi, write_msg
    call puts
    mov eax, [0x5000]
    call hex
    mov eax, [0x9000]
    call hex
    call newline

    mov dword [0x2010], 0x9063
    invlpg [0x4000]
    mov eax, [0x4000]
    and dword [0x2010], ~0x20
    mov eax, [0x4000]
    mov eax, [0x2010]
    and eax, 0x20
    mov [0x5020], eax
    invlpg [0x4000]
    mov eax, [0x4000]
    mov eax, [0x2010]
    and eax, 0x20
    mov [0x5024], eax
    mov esi, accessed_msg
    call puts
    mov eax, [0x5020]
    call hex
    mov eax, [0x5024]
    call hex
    call newline

    mov dword [0x2010], 0x8003
    mov eax, cr3
    mov cr3, eax
    mov eax, [0x4000]
    mov [0x5020], eax
    mov esi, cr3_msg
    call puts
    mov eax, [0x5020]
    call hex
    call newline

    mov dword [0x2010], 0x9003
    mov eax, cr0
    xor eax, 8                    ; TS does not change address translation
    mov cr0, eax
    mov eax, [0x4000]
    mov [0x5020], eax
    mov eax, cr4
    xor eax, 4                    ; TSD also leaves the retained mapping
    mov cr4, eax
    mov eax, [0x4000]
    mov [0x5024], eax
    mov eax, cr0
    xor eax, 0x10000              ; WP changes cached write permissions
    mov cr0, eax
    mov eax, [0x4000]
    mov [0x5028], eax
    mov dword [0x2010], 0x8003
    mov eax, cr4
    xor eax, 0x80                 ; PGE changes translation lifetime
    mov cr4, eax
    mov eax, [0x4000]
    mov [0x502c], eax
    mov esi, control_msg
    call puts
    mov eax, [0x5020]
    call hex
    mov eax, [0x5024]
    call hex
    mov eax, [0x5028]
    call hex
    mov eax, [0x502c]
    call hex
    call newline

    mov dword [0x2010], 0x8103
    mov dword [0x2018], 0xb003
    mov dword [0xb000], 0x33333333
    mov dword [0xc000], 0x55555555
    invlpg [0x4000]
    invlpg [0x6000]
    mov eax, [0x4000]
    mov eax, [0x6000]
    mov dword [0x2010], 0x9103
    mov dword [0x2018], 0xc003
    mov eax, cr3
    mov cr3, eax
    mov eax, [0x4000]
    mov [0x5020], eax
    mov eax, [0x6000]
    mov [0x5024], eax
    invlpg [0x4000]
    mov eax, [0x4000]
    mov [0x5028], eax
    mov dword [0x2010], 0x8103
    mov eax, cr4
    and eax, ~0x80
    mov cr4, eax
    mov eax, [0x4000]
    mov [0x502c], eax
    mov esi, global_msg
    call puts
    mov eax, [0x5020]
    call hex
    mov eax, [0x5024]
    call hex
    mov eax, [0x5028]
    call hex
    mov eax, [0x502c]
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
map_msg: db 'MAP ', 0
write_msg: db 'WRITE ', 0
accessed_msg: db 'ACCESSED ', 0
cr3_msg: db 'CR3 ', 0
control_msg: db 'CONTROL ', 0
global_msg: db 'GLOBAL ', 0
done_msg: db 'R86_TLB_DONE', 10, 0
times 0xfff0 - ($ - $$) db 0xff
bits 16
    jmp 0xf000:0
times 0x10000 - ($ - $$) db 0xff
