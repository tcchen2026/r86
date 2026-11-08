bits 16
org 0x7c00

com1 equ 0x3f8

start:
    cli
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x7c00

    mov ax, 0xb800
    mov es, ax
    xor di, di
    mov si, vga_msg
    mov ah, 0x1f
.vga:
    lodsb
    test al, al
    jz .vga_done
    stosw
    jmp .vga

.vga_done:
    call serial_init
    mov si, begin_msg
    call puts

    call wait_ibf_clear
    mov al, 0x60
    out 0x64, al
    call wait_ibf_clear
    mov al, 0x44
    out 0x60, al

    call wait_ibf_clear
    mov al, 0xa8
    out 0x64, al

    call wait_ibf_clear
    mov al, 0xd3
    out 0x64, al
    call wait_ibf_clear
    mov al, 0x5a
    out 0x60, al
    call read_and_print

    call wait_ibf_clear
    mov al, 0xd4
    out 0x64, al
    call wait_ibf_clear
    mov al, 0xf4
    out 0x60, al
    call read_and_print

    mov si, ready_msg
    call puts

.poll:
    in al, 0x64
    test al, 0x01
    jz .poll
    call read_and_print
    jmp .poll

serial_init:
    mov dx, com1 + 3
    mov al, 0x80
    out dx, al
    mov dx, com1
    mov al, 0x01
    out dx, al
    mov dx, com1 + 1
    xor al, al
    out dx, al
    mov dx, com1 + 3
    mov al, 0x03
    out dx, al
    mov dx, com1 + 4
    mov al, 0x0b
    out dx, al
    ret

puts:
    lodsb
    test al, al
    jz .done
    call putc
    jmp puts
.done:
    ret

putc:
    push ax
    push dx
    mov ah, al
    mov dx, com1 + 5
.wait:
    in al, dx
    test al, 0x20
    jz .wait
    mov al, ah
    mov dx, com1
    out dx, al
    pop dx
    pop ax
    ret

wait_ibf_clear:
    in al, 0x64
    test al, 0x02
    jnz wait_ibf_clear
    ret

read_and_print:
    in al, 0x64
    test al, 0x01
    jz read_and_print
    mov bh, al
    in al, 0x60
    mov bl, al
    mov al, 'K'
    test bh, 0x20
    jz .prefix
    mov al, 'M'
.prefix:
    call putc
    mov al, bl
    call print_hex
    mov al, ' '
    call putc
    ret

print_hex:
    push ax
    shr al, 4
    call print_nibble
    pop ax
    and al, 0x0f
    call print_nibble
    ret

print_nibble:
    cmp al, 10
    jb .digit
    add al, 'A' - 10
    jmp putc
.digit:
    add al, '0'
    jmp putc

vga_msg db 'R86_SYN_VGA', 0
begin_msg db 'R86_SYN_BEGIN ', 0
ready_msg db 'R86_SYN_READY ', 0

times 510 - ($ - $$) db 0
dw 0xaa55
