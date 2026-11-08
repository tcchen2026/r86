; PTY regression: disable scanning while the host pastes, then verify every tap.
; scenario at 1F0: 0=resume/read 201 'A', 1=stay blocked, 2=guest reset/read 'b'.
bits 16
org 0x7c00
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x7c00
    inc byte [0x500]
    mov ax, 0xb800
    mov es, ax
    xor di, di
    mov ax, 0x1f20
    mov cx, 80*25
    rep stosw
    mov al, 0x60
    out 0x64, al
    mov al, 0x44                 ; Set 2 keyboard, controller translation.
    out 0x60, al
    mov al, 0xf5                 ; Scanning disabled, not just IRQ masked.
    out 0x60, al
    call read_key
    cmp al, 0xfa
    jne bad
    mov di, 160
    mov al, [0x500]
    add al, '0'
    mov ah, 0x1f
    stosw
    xor di, di
    mov si, blocked
    call print
    cmp byte [scenario], 1
    je halt
    cmp byte [scenario], 2
    jne delay
    cmp byte [0x500], 1
    ja restarted
delay:
    ; Two real RTC second changes, independent of interpreter throughput.
    xor al, al
    out 0x70, al
    in al, 0x71
    mov bl, al
    mov cx, 2
.second:
    in al, 0x71
    cmp al, bl
    je .second
    mov bl, al
    loop .second
    cmp byte [scenario], 2
    jne resume
    mov al, 3
    out 0x92, al
    jmp halt
resume:
    call enable
    mov di, 320
    mov si, draining
    call print
    mov cx, 201*4
    xor bx, bx
.tap:
    call read_key
    cmp al, [shift_a+bx]
    jne bad
    inc bx
    and bx, 3
    loop .tap
    mov si, passed
    jmp report
restarted:
    call enable
    mov di, 320
    mov si, after_reset
    call print
    call read_key
    cmp al, 0x30                ; A fresh lowercase 'b', with no stale Shift/A.
    jne bad
    call read_key
    cmp al, 0xb0
    jne bad
    mov si, passed
    jmp report
enable:
    mov al, 0xf4
    out 0x60, al
    call read_key
    cmp al, 0xfa
    jne bad
    ret
read_key:
    in al, 0x64
    test al, 1
    jz read_key
    in al, 0x60
    ret
bad:
    mov si, failed
report:
    mov di, 480
    call print
halt:
    hlt
    jmp halt
print:
    mov ah, 0x1f
.byte:
    lodsb
    test al, al
    jz .done
    stosw
    jmp .byte
.done:
    ret
shift_a: db 0x2a, 0x1e, 0x9e, 0xaa
blocked: db 'R86_TERM_BLOCKED', 0
draining: db 'R86_TERM_DRAINING', 0
after_reset: db 'R86_TERM_AFTER_RESET', 0
passed: db 'R86_TERM_QUEUE_OK', 0
failed: db 'R86_TERM_QUEUE_BAD', 0
times 0x1f0-($-$$) db 0
scenario: db 0
times 510-($-$$) db 0
dw 0xaa55
