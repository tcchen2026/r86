; Deterministic SB16 diagnostic, entered at 0000:7c00. No firmware services.
; NASM -f bin. One second of an 8 kHz unsigned square wave via DMA 1, IRQ 5.
; The IRQ handler acknowledges the DSP and PIC and prints R86_SOUND_OK.
bits 16
org 0x7c00
start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7000
    cld
    mov word [0x25*4], irq5
    mov word [0x25*4+2], 0
    mov al, 0x11
    out 0x20, al
    out 0xa0, al
    mov al, 0x20
    out 0x21, al
    mov al, 0x28
    out 0xa1, al
    mov al, 4
    out 0x21, al
    mov al, 2
    out 0xa1, al
    mov al, 1
    out 0x21, al
    out 0xa1, al
    mov al, 0xdf
    out 0x21, al
    mov al, 0xff
    out 0xa1, al
    mov dx, 0x226
    mov al, 1
    out dx, al
    xor al, al
    out dx, al
    mov dx, 0x22a
    in al, dx
    cmp al, 0xaa
    jne failed
    mov dx, 0x22c
    mov al, 0xe1
    out dx, al
    mov dx, 0x22a
    in al, dx
    cmp al, 4
    jne failed
    in al, dx
    cmp al, 5
    jne failed
    mov cx, 4
    mov bl, 0x30
.volume:
    mov dx, 0x224
    mov al, bl
    out dx, al
    inc dx
    mov al, 0xf8
    out dx, al
    inc bl
    loop .volume
    mov di, 0x1000
    mov cx, 8000
.wave:
    mov al, 32
    test di, 4
    jz .low
    mov al, 224
.low:
    stosb
    loop .wave
    xor al, al
    out 0xd4, al
    mov al, 5
    out 0x0a, al
    xor al, al
    out 0x0c, al
    out 0x83, al
    out 2, al
    mov al, 0x10
    out 2, al
    mov ax, 7999
    out 3, al
    mov al, ah
    out 3, al
    mov al, 0x49
    out 0x0b, al
    mov al, 1
    out 0x0a, al
    mov si, commands
    mov cx, commands_end - commands
    mov dx, 0x22c
.cmd:
    lodsb
    out dx, al
    loop .cmd
    sti
.wait:
    hlt
    jmp .wait
failed:
    mov si, failure
    jmp print
irq5:
    mov dx, 0x22e
    in al, dx
    mov al, 0x20
    out 0x20, al
    mov si, success
print:
    mov dx, 0x402
.char:
    lodsb
    test al, al
    jz .done
    out dx, al
    jmp .char
.done:
    cli
    hlt
    jmp .done
commands: db 0x41, 0x1f, 0x40, 0xc0, 0, 0x3f, 0x1f
commands_end:
success: db 'R86_SOUND_OK', 10, 0
failure: db 'R86_SOUND_FAIL', 10, 0
times 510-($-$$) db 0
dw 0xaa55
