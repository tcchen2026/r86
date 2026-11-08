; nasm -f bin tests/bootsectors/firmware_dac.asm -o tests/bootsectors/firmware_dac.bin
; Real SeaBIOS VBE 4F08: 800x600x8, DAC state/readback, and banked corner pixels.
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
    mov ax, 0x4f02
    mov bx, 0x103
    int 0x10
    cmp ax, 0x004f
    jne failed
    mov ax, 0xa000
    mov es, ax
    mov byte [es:0], 0x20
    mov byte [es:799], 0x21
    mov ax, 0x4f05
    xor bx, bx
    mov dx, 7
    int 0x10
    cmp ax, 0x004f
    jne failed
    mov byte [es:599*800 - 7*65536], 0x22
    mov byte [es:599*800 + 799 - 7*65536], 0x23
    ; Changing DAC width must preserve framebuffer contents and the bank.
    mov ax, 0x4f08
    mov bx, 0x0800
    int 0x10
    cmp ax, 0x004f
    jne failed
    cmp bh, 8
    jne failed
    mov ax, 0x4f08
    mov bx, 1
    int 0x10
    cmp ax, 0x004f
    jne failed
    cmp bh, 8
    jne failed
    mov dx, 0x3c8
    mov al, 0x20
    out dx, al
    mov dx, 0x3c7
    in al, dx
    test al, al
    jnz failed
    mov dx, 0x3c9
    mov si, colors
    mov cx, 12
.write:
    lodsb
    out dx, al
    loop .write
    mov dx, 0x3c8
    in al, dx
    cmp al, 0x24
    jne failed
    mov dx, 0x3c7
    mov al, 0x20
    out dx, al
    in al, dx
    cmp al, 3
    jne failed
    mov dx, 0x3c9
    mov si, colors
    mov cx, 12
.read:
    in al, dx
    cmp al, [si]
    jne failed
    inc si
    loop .read
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
colors: db 0x80, 0xa5, 0xff, 0x12, 0x34, 0x56, 0x9a, 0xfe, 0x34, 0xff, 0xff, 0xff
success: db 'R86_DAC_OK', 10, 0
error: db 'R86_DAC_ERROR', 10, 0
times 510-($-$$) db 0
dw 0xaa55
