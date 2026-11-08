; Transient generated framebuffer accesses: PCI relocation/decode, read-only
; paging, and host rendering between repeated writes to an already clean page.
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
    in al, 0x92
    or al, 2
    out 0x92, al
    lgdt [cs:gdtr - start]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp dword 8:protected
align 8
gdt: dq 0, 0x00cf9a000000ffff, 0x00cf92000000ffff
gdtr: dw 23
    dd gdt
idtr: dw 0x7ff
    dd 0x17000
bits 32
protected:
    mov ax, 16
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov esp, 0x90000
    mov esi, payload
    mov edi, 0x100000
    mov ecx, payload_end - payload
    rep movsb
    jmp 0x100000

%macro vbe 2
    mov dx, 0x1ce
    mov ax, %1
    out dx, ax
    inc dx
    mov ax, %2
    out dx, ax
%endmacro
%macro pci 2
    mov dx, 0xcf8
    mov eax, %1
    out dx, eax
    mov dx, 0xcfc
    mov eax, %2
    out dx, eax
%endmacro
%macro mark 1
    mov dx, 0x3f8
    mov al, %1
    out dx, al
%endmacro

payload:
    pci 0x80001004, 3
    vbe 4, 0
    vbe 1, 800
    vbe 2, 600
    vbe 3, 32
    vbe 4, 0x41
    ; Identity-map code/stack; the page at 3 MiB is the linear framebuffer.
    mov edi, 0x10000
    xor eax, eax
    mov ecx, 1024
    rep stosd
    mov ecx, 1024
    mov eax, 3
.identity:
    stosd
    add eax, 4096
    loop .identity
    mov dword [0x10000], 0x11003
    mov dword [0x11c00], 0xe0000003
    mov edi, 0x17000
    xor eax, eax
    mov ecx, 512
    rep stosd
    mov eax, 0x100000 + page_fault - payload
    mov [0x17070], ax
    mov word [0x17072], 8
    mov word [0x17074], 0x8e00
    shr eax, 16
    mov [0x17076], ax
    lidt [idtr]
    mov eax, 0x10000
    mov cr3, eax
    mov eax, cr0
    or eax, 0x80010000
    mov cr0, eax
    mov esi, 0x300000
    mov eax, 0xff0000
    call paint
    mark 'R'                   ; host renders and clears the dirty page
    mov eax, 0x00ff00
    call paint
    mark 'G'                   ; the next generated store must dirty it again

    ; No PTE change: an old device pointer must expire on the PCI write.
    pci 0x80001010, 0xfd000000
    cmp dword [esi], -1
    jne fail
    mov dword [0x11c00], 0xfd000003
    invlpg [esi]
    cmp dword [esi], 0x00ff00
    jne fail
    mov eax, 0x0000ff
    call paint
    mark 'B'
    pci 0x80001004, 0
    cmp dword [esi], -1
    jne fail
    pci 0x80001004, 3
    cmp dword [esi], 0x0000ff
    jne fail

    and dword [0x11c00], ~2
    invlpg [esi]
    mov ecx, 100000
.read_only:
    mov eax, [esi]
    cmp eax, 0x0000ff
    jne fail
    dec ecx
    jnz .read_only
    xor ebp, ebp
    mov edx, 0x12345678
    mov [esi], edx             ; a read proof must not authorize this write
    cmp ebp, 1
    jne fail
    cmp dword [esi], 0x0000ff
    jne fail
    mark 'K'
    hlt
paint:
    mov ecx, 200000
.loop:
    mov [esi], eax
    mov edx, [ss:esi]
    cmp edx, eax
    jne fail
    dec ecx
    jnz .loop
    ret
page_fault:
    cmp dword [esp], 3
    jne fail
    mov eax, cr2
    cmp eax, 0x300000
    jne fail
    inc ebp
    add dword [esp + 4], 2
    add esp, 4
    iretd
fail:
    mark 'F'
    hlt
payload_end:
times 0xfff0 - ($ - $$) db 0
bits 16
    jmp 0xf000:0
times 0x10000 - ($ - $$) db 0
