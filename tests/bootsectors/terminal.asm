bits 16
org 0x7c00

start:
    cli
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x7c00
    inc byte [0x500]             ; RAM survives a machine reset
    mov ax, 0xb800
    mov es, ax
    xor di, di
    mov ax, 0x1f20
    mov cx, 80 * 25
    rep stosw
    xor di, di
    mov si, message
    mov ah, 0x1f
.text:
    lodsb
    test al, al
    jz .ready
    stosw
    jmp .text
.ready:
    mov al, [0x500]
    add al, '0'
    mov [es:160], ax
    mov di, 320
    mov al, 0x60
    out 0x64, al
    mov al, 0x44                 ; enable set-2 to set-1 translation
    out 0x60, al
.poll:
    in al, 0x64
    test al, 1
    jz .poll
    in al, 0x60
    mov bl, al
    shr al, 4
    call nibble
    mov al, bl
    and al, 15
    call nibble
    mov al, ' '
    stosw
    jmp .poll
nibble:
    cmp al, 10
    jb .digit
    add al, 'A' - 10
    stosw
    ret
.digit:
    add al, '0'
    stosw
    ret

message db 'R86_TERM_READY ', 0xda, 0xc4, 0xbf, 0
times 510-($-$$) db 0
dw 0xaa55
