; nasm -f bin tests/bootsectors/firmware_vbe_640x340.asm -o tests/bootsectors/firmware_vbe_640x340.bin
; Enumerate the selected 640x340 mode and leave its ModeInfoBlock at 9000h.
; Draw through A000, then switch to the linear framebuffer preserving VRAM.
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
    mov ax, 0x4f02
    mov bx, [test_mode]
    int 0x10
    cmp ax, 0x004f
    jne failed
    mov ax, 0xa000
    mov es, ax
    movzx ebp, byte [0x9019]
    shr bp, 3                  ; bytes per pixel
    mov si, corners
.corner:
    lodsd                      ; pixel index
    imul eax, ebp
    mov di, ax
    shr eax, 16                ; select the 64 KiB bank
    mov dx, ax
    mov ax, 0x4f05
    xor bx, bx
    int 0x10
    cmp ax, 0x004f
    jne failed
    lodsw                      ; RGB565 color
    cmp bp, 2
    jne .rgb32
    stosw
    add si, 4
    jmp .next
.rgb32:
    lodsd                      ; XRGB8888 color
    stosd
.next:
    cmp si, corners_end
    jb .corner
    mov ax, 0x4f02
    mov bx, [test_mode]
    or bx, 0xc000              ; linear framebuffer, preserve the four pixels
    int 0x10
    cmp ax, 0x004f
    jne failed
    mov si, success
    jmp report
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
corners:                       ; pixel index, RGB565, XRGB8888
    dd 0
    dw 0xf800
    dd 0xff0000
    dd 639
    dw 0x07e0
    dd 0x00ff00
    dd 339*640
    dw 0x001f
    dd 0x0000ff
    dd 339*640+639
    dw 0xffff
    dd 0xffffff
corners_end:
times 0x1f0-($-$$) db 0
test_mode: dw 0x1a0
times 510-($-$$) db 0
dw 0xaa55
