; nasm -f bin tests/bootsectors/firmware_vbe_640x340.asm -o tests/bootsectors/firmware_vbe_640x340.bin
; The VGA BIOS's 640x340 RGB565 VBE mode (1A0h, twice the CardputerZero
; panel): BIOS discovery, mode set and the four corner pixels through the
; banked A000 window.
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
    mov ax, 0x4f01
    mov cx, 0x1a0
    mov di, 0x9000
    int 0x10
    cmp ax, 0x004f
    jne failed
    cmp word [0x9012], 640
    jne failed
    cmp word [0x9014], 340
    jne failed
    cmp byte [0x9019], 16
    jne failed
    cmp word [0x9010], 1280
    jne failed
    mov ax, 0x4f02
    mov bx, 0x1a0
    int 0x10
    cmp ax, 0x004f
    jne failed
    mov ax, 0xa000
    mov es, ax
    xor di, di
    mov ax, 0xf800             ; red at (0,0)
    stosw
    mov di, 639*2
    mov ax, 0x07e0             ; green at (639,0)
    stosw
    mov ax, 0x4f05             ; row 339 starts at 433920: 64 KiB bank 6
    xor bx, bx
    mov dx, 6
    int 0x10
    cmp ax, 0x004f
    jne failed
    mov di, 339*1280 - 6*65536
    mov ax, 0x001f             ; blue at (0,339)
    stosw
    mov di, 339*1280 + 639*2 - 6*65536
    mov ax, 0xffff             ; white at (639,339)
    stosw
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
times 510-($-$$) db 0
dw 0xaa55
