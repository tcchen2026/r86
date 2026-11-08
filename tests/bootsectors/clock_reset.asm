; Guest snapshots across a host pause and a warm board reset. The host pauses
; after S, resumes through P/A, then reboots to R. Rebuild with:
; nasm -f bin tests/bootsectors/clock_reset.asm -o tests/bootsectors/clock_reset.bin
bits 16
org 0x7c00
    cli
    xor ax, ax
    mov ds, ax
    mov ss, ax
    mov sp, 0x7c00
    cmp word [0x500], 0x86c1
    je reset_entry
    mov word [0x500], 0x86c1

    mov al, 0x0b
    out 0x70, al
    mov al, 0x86                 ; SET, binary, 24-hour
    out 0x71, al
    xor al, al
    out 0x70, al
    mov al, 20                   ; guest-programmed seconds, not the host RTC
    out 0x71, al
    mov al, 0x0a
    out 0x70, al
    mov al, 0x20                 ; disable periodic source
    out 0x71, al
    mov al, 0x0b
    out 0x70, al
    mov al, 0x16                 ; update-ended interrupt, calendar running
    out 0x71, al
    mov dx, 0x602
    mov al, 1                   ; enable PM timer events
    out dx, al
    mov al, 0xf1
    out 0xb2, al                 ; SCI_EN

    mov ecx, 0x10
    mov eax, 0x540be400
    mov edx, 2
    wrmsr                       ; TSC = 10 seconds, independent of PM/RTC
    mov di, 0x520
    call snapshot
    mov al, 'S'
    call serial
    mov di, 0x540
    call snapshot
    mov al, 'P'
    call serial
    mov dx, 0x600
    mov al, 1                   ; acknowledge PM timer, RTC was read-to-clear
    out dx, al
    mov di, 0x560
    call snapshot
    mov al, 'A'
    call serial
    hlt
reset_entry:
    mov di, 0x580
    call snapshot
    mov al, 'R'
    call serial
    hlt

snapshot:
    mov dx, 0x608
    in eax, dx
    mov [di], eax
    rdtsc
    mov [di + 4], eax
    mov [di + 8], edx
    xor al, al
    out 0x70, al
    in al, 0x71
    mov [di + 12], al
    mov al, 0x0c
    out 0x70, al
    in al, 0x71
    mov [di + 13], al
    mov dx, 0x600
    in ax, dx
    mov [di + 14], ax
    ret
serial:
    mov dx, 0x3f8
    out dx, al
    ret
times 510-($-$$) db 0
dw 0xaa55
