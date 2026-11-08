; nasm -f bin tests/bootsectors/firmware_vbe.asm -o tests/bootsectors/firmware_vbe.bin
; Real INT 10h VBE modes: plane addressing (102h) and banked RGB565 (114h).
; The host patches test_mode, boots SeaBIOS, then checks all four corner pixels.
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
    mov ax, 0x4f01
    mov cx, [test_mode]
    mov di, 0x9000
    int 0x10
    cmp ax, 0x004f
    jne failed
    cmp word [0x9012], 800
    jne failed
    cmp word [0x9014], 600
    jne failed
    mov ax, 0x4f02
    mov bx, [test_mode]
    int 0x10
    cmp ax, 0x004f
    jne failed
    ; Record the PCI framebuffer aperture as observed by the guest. The host
    ; checks that VBE and ACPI describe the same allocation made by SeaBIOS.
    mov dx, 0xcf8
    mov eax, 0x80001010       ; bus 0, device 2, function 0, BAR0
    out dx, eax
    mov dx, 0xcfc
    in eax, dx
    mov [0x9100], eax
    mov eax, 0xffffffff
    out dx, eax
    in eax, dx
    mov [0x9104], eax
    mov eax, [0x9100]
    out dx, eax
    cmp word [test_mode], 0x114
    je rgb565
    cmp byte [0x9019], 4
    jne failed
    cmp word [0x9010], 100
    jne failed
    xor bx, bx
    xor cx, cx
    xor dx, dx
    mov al, 4
    call planar_pixel
    mov cx, 799
    mov al, 2
    call planar_pixel
    xor cx, cx
    mov dx, 599
    mov al, 1
    call planar_pixel
    mov cx, 799
    mov al, 15
    call planar_pixel
    jmp passed
rgb565:
    cmp byte [0x9019], 16
    jne failed
    cmp word [0x9010], 1600
    jne failed
    mov ax, 0xa000
    mov es, ax
    xor di, di
    mov ax, 0xf800
    stosw
    mov di, 799*2
    mov ax, 0x07e0
    stosw
    mov ax, 0x4f05
    xor bx, bx
    mov dx, 14
    int 0x10
    cmp ax, 0x004f
    jne failed
    mov di, 599*1600 - 14*65536
    mov ax, 0x001f
    stosw
    mov di, 599*1600 + 799*2 - 14*65536
    mov ax, 0xffff
    stosw
passed:
    mov si, success
    jmp report
planar_pixel:
    mov [expected], al
    mov ah, 0x0c
    int 0x10
    mov ah, 0x0d
    int 0x10
    cmp al, [expected]
    jne failed
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
expected: db 0
success: db 'R86_VBE_OK', 10, 0
error: db 'R86_VBE_ERROR', 10, 0
times 0x1f0-($-$$) db 0
test_mode: dw 0x102
times 510-($-$$) db 0
dw 0xaa55
