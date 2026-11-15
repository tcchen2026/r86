; nasm -f bin tests/bootsectors/firmware_vbe_936x666.asm -o tests/bootsectors/firmware_vbe_936x666.bin
; Modes 1A1h/1A2h: VBE enumeration, 936x666 mode information and setting,
; then four corner pixels through the banked A000 window. The host patches
; test_mode/test_bpp at 1F0h for each depth.
bits 16
org 0x7c00
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7c00
    sti
    cld
    mov dx, 0x3fb              ; 115200 8N1
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
    mov dword [0x9000], 'VBE2'
    mov ax, 0x4f00
    mov di, 0x9000
    int 0x10
    cmp ax, 0x004f
    jne failed
    les si, [0x900e]           ; VideoModePtr
.find_mode:
    mov ax, [es:si]
    add si, 2
    cmp ax, 0xffff
    je failed
    cmp ax, [test_mode]
    jne .find_mode
    xor ax, ax
    mov es, ax
    mov ax, 0x4f01
    mov cx, [test_mode]
    mov di, 0x9000
    int 0x10
    cmp ax, 0x004f
    jne failed
    cmp word [0x9012], 936
    jne failed
    cmp word [0x9014], 666
    jne failed
    movzx ax, byte [test_bpp]
    cmp byte [0x9019], al
    jne failed
    shr ax, 3
    imul ax, 936
    cmp word [0x9010], ax
    jne failed
    mov ax, 0x4f02
    mov bx, [test_mode]
    int 0x10
    cmp ax, 0x004f
    jne failed
    mov ax, 0xa000
    mov es, ax
    xor edi, edi
    cmp byte [test_bpp], 16
    jne rgb32
    mov eax, 0xf800            ; red at (0,0)
    call pixel
    mov edi, 935*2
    mov eax, 0x07e0            ; green at (935,0)
    call pixel
    mov edi, 665*936*2
    mov eax, 0x001f            ; blue at (0,665), bank 18
    call pixel
    mov edi, (665*936+935)*2
    mov eax, 0xffff            ; white at (935,665), bank 19
    call pixel
    jmp passed
rgb32:
    mov eax, 0x00ff0000
    call pixel
    mov edi, 935*4
    mov eax, 0x0000ff00
    call pixel
    mov edi, 665*936*4
    mov eax, 0x000000ff        ; bank 37
    call pixel
    mov edi, (665*936+935)*4
    mov eax, 0x00ffffff        ; bank 38
    call pixel
passed:
    mov si, success
    jmp report
pixel:
    push eax
    mov edx, edi
    shr edx, 16
    mov ax, 0x4f05
    xor bx, bx
    int 0x10
    cmp ax, 0x004f
    pop eax
    jne failed
    cmp byte [test_bpp], 16
    jne .dword
    stosw
    ret
.dword:
    stosd
    ret
failed:
    mov si, error
report:
    lodsb
    test al, al
    jz shutdown
    push ax
    mov dx, 0x3fd
.ready:
    in al, dx
    test al, 0x20
    jz .ready
    pop ax
    mov dx, 0x3f8
    out dx, al
    jmp report
shutdown:
    mov dx, 0x3fd
.drain:
    in al, dx
    test al, 0x40
    jz .drain
    mov dx, 0x604
    mov ax, 0x2000
    out dx, ax
    cli
    hlt
    jmp $
success: db 'R86_VBE_OK', 10, 0
error: db 'R86_VBE_ERROR', 10, 0
times 0x1f0-($-$$) db 0
test_mode: dw 0x1a1
test_bpp: db 16
times 510-($-$$) db 0
dw 0xaa55
