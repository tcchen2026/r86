; Actual x86 guest used by tests/disk.rs. No BIOS or host disk API shortcuts.
; nasm -f bin tests/bootsectors/vhd_pio.asm -o tests/bootsectors/vhd_pio.bin
; Empty disk: write two PIO sectors across a 2 MiB boundary, FLUSH, report W.
; Reopened disk: verify both sectors and their unwritten neighbours, report R.
; An ATA error (including injected host file failure) reports E and halts.
bits 16
org 0x7c00
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7c00
    mov dx, 0x3f6
    mov al, 2                 ; polling PIO, no IRQ handler
    out dx, al
    mov eax, 4095
    call read_two
    cmp word [0x8000], 0
    je write_empty
    mov di, 0x8000
    mov ax, 0x6a3c
    mov cx, 256
    repe scasw
    jne failed
    mov ax, 0xb791
    mov cx, 256
    repe scasw
    jne failed
    mov eax, 4093
    call read_two
    call verify_zero
    mov eax, 4097
    call read_two
    call verify_zero
    mov al, 'R'
    jmp report

write_empty:
    mov di, 0x8000
    mov ax, 0x6a3c
    mov cx, 256
    rep stosw
    mov ax, 0xb791
    mov cx, 256
    rep stosw
    mov eax, 4095
    mov bl, 0x30
    call command_two
    mov si, 0x8000
    mov bp, 2
.sector:
    call wait_drq
    mov dx, 0x1f0
    mov cx, 256
    rep outsw
    call wait_ready
    dec bp
    jnz .sector
    mov dx, 0x1f7
    mov al, 0xe7
    out dx, al
    call wait_ready
    mov al, 'W'
    jmp report

read_two:
    mov bl, 0x20
    call command_two
    mov di, 0x8000
    mov bp, 2
.sector:
    call wait_drq
    mov dx, 0x1f0
    mov cx, 256
    rep insw
    dec bp
    jnz .sector
    call wait_ready
    ret

verify_zero:
    mov di, 0x8000
    xor ax, ax
    mov cx, 512
    repe scasw
    jne failed
    ret

command_two:                  ; EAX = LBA (< 2^24), BL = command
    mov esi, eax
    mov dx, 0x1f2
    mov al, 2
    out dx, al
    inc dx
    mov eax, esi
    out dx, al
    inc dx
    shr eax, 8
    out dx, al
    inc dx
    shr eax, 8
    out dx, al
    inc dx                    ; DEVICE last, as with libata task-file loading
    mov al, 0xe0
    out dx, al
    mov dx, 0x1f7
    mov al, bl
    out dx, al
    ret

wait_drq:
    call wait_ready
    test al, 8
    jz wait_drq
    ret

wait_ready:
    mov dx, 0x1f7
    in al, dx
    test al, 0x80
    jnz wait_ready
    test al, 1
    jnz failed
    ret

failed:
    mov al, 'E'
report:
    mov dx, 0x3f8
    out dx, al
    hlt
    jmp $
times 510-($-$$) db 0
dw 0xaa55
