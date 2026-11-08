; nasm -f bin tests/bootsectors/firmware_vga.asm -o tests/bootsectors/firmware_vga.bin
; Boot via SeaBIOS, then exercise the installed VGA BIOS and actual pixels.
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
    ; The same guest runs on independent UART models: initialize 115200 8N1.
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
    mov ax, 0x0012
    int 0x10
    mov ah, 0x0f
    int 0x10
    cmp al, 0x12
    jne failed
    xor bx, bx
    mov cx, 10
    mov dx, 10
    mov ax, 0x0c04
    int 0x10
    mov ah, 0x0d
    int 0x10
    cmp al, 4
    jne failed
    ; Copy eight planar pixels through VGA read latches, as a window move does.
    ; Write mode 1 ignores the CPU byte, raster op, rotate and bit mask.
    mov dx, 0x3ce
    mov ax, 0x0105
    out dx, ax
    mov ax, 0x1f03
    out dx, ax
    mov ax, 0x5a08
    out dx, ax
    mov ax, 0xa000
    mov ds, ax
    mov es, ax
    mov si, 10*80+1
    mov di, 20*80+3
    movsb
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ax, 0x0005
    out dx, ax
    mov ax, 0x0003
    out dx, ax
    mov ax, 0xff08
    out dx, ax
    mov cx, 26
    mov dx, 20
    mov ah, 0x0d
    int 0x10
    cmp al, 4
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
success: db 'R86_VGA_BIOS_OK', 10, 0
error: db 'R86_VGA_BIOS_ERROR', 10, 0
times 510-($-$$) db 0
dw 0xaa55
