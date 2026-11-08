; nasm -f bin tests/bootsectors/apm_disk.asm -o tests/bootsectors/apm_disk.bin
; SeaBIOS boots this from a real HDD. Exercise BIOS EDD read/write, reboot,
; persisted boot state and APM power-off, without knowing an ACPI port address.
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
    mov [drive], dl
    mov ah, 0x42
    call disk_io
    cmp word [0x8000], 2
    je reopened
    cmp word [0x8000], 1
    je rebooted
    cmp word [0x8000], 0
    jne failed
    mov si, first
    call print
    inc word [0x8000]
    mov ah, 0x43
    call disk_io
    mov al, 3
    out 0x92, al
    jmp failed
rebooted:
    inc word [0x8000]
    mov ah, 0x43
    call disk_io
    mov si, second
    jmp shutdown
reopened:
    mov si, third
shutdown:
    call print
    mov ax, 0x5300
    xor bx, bx
    int 0x15
    jc failed
    mov ax, 0x5301
    xor bx, bx
    int 0x15
    jc failed
    mov ax, 0x530e
    xor bx, bx
    mov cx, 0x0102
    int 0x15
    jc failed
    mov ax, 0x5307
    mov bx, 1
    mov cx, 3
    int 0x15
failed:
    mov si, error
    call print
    cli
    hlt
    jmp $
disk_io:
    xor al, al
    mov dl, [drive]
    mov si, dap
    int 0x13
    jc failed
    ret
print:
    lodsb
    test al, al
    jz .done
    mov dx, 0x3f8
    out dx, al
    jmp print
.done:
    ret
first: db 'R86_APM_FIRST', 10, 0
second: db 'R86_APM_REBOOT', 10, 0
third: db 'R86_APM_REOPEN', 10, 0
error: db 'R86_APM_ERROR', 10, 0
drive: db 0
align 4
dap: db 16, 0
     dw 1
     dw 0x8000, 0
     dq 1
times 510-($-$$) db 0
dw 0xaa55
