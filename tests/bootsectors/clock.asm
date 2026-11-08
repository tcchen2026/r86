; Observe the same PIT/RTC/PM/TSC clocks with a busy CPU and a halted CPU.
bits 16
org 0x7c00
start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7c00
    lgdt [gdtr]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp 8:protected
bits 32
protected:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov esp, 0x9000
    lidt [idtr]
    xor eax, eax
    mov edi, 0x500
    mov ecx, 64
    rep stosw
    mov word [0x900], irq0
    mov word [0x902], 8
    mov dword [0x904], 0x8e00
    mov word [0x940], irq8
    mov word [0x942], 8
    mov dword [0x944], 0x8e00
    mov al, 0xfa
    out 0x21, al
    mov al, 0xfe
    out 0xa1, al
    mov al, 0x34
    out 0x43, al
    mov ax, 11932
    out 0x40, al
    mov al, ah
    out 0x40, al
    mov al, 0x0a
    out 0x70, al
    mov al, 0x2a                 ; 64 Hz RTC interrupt
    out 0x71, al
    mov al, 0x0b
    out 0x70, al
    mov al, 0x46                 ; PIE, 24-hour, binary
    out 0x71, al
    sti
    mov edi, 0x520
    call snapshot
    mov al, 'S'
    call serial
busy:
    mov ecx, 4096
.work:
    inc dword [0x508]
    loop .work
    mov dx, 0x608
    in eax, dx
    sub eax, [0x520]
    and eax, 0xffffff
    cmp eax, 4295455             ; 1.2 seconds of 3,579,546 Hz PM time
    jb busy
    mov edi, 0x540
    call snapshot
    mov al, 'B'
    call serial
idle:
    hlt
    mov dx, 0x608
    in eax, dx
    sub eax, [0x540]
    and eax, 0xffffff
    cmp eax, 4295455
    jb idle
    mov edi, 0x560
    call snapshot
    mov al, 'I'
    call serial
    mov dx, 0x605
    mov al, 0x20
    out dx, al
    cli
    hlt

snapshot:
    pushfd
    cli
    mov dx, 0x608
    in eax, dx
    mov [edi], eax
    rdtsc
    mov [edi + 4], eax
    mov [edi + 8], edx
    mov al, 0
    out 0x70, al
    in al, 0x71
    mov [edi + 12], al
    mov eax, [0x500]
    mov [edi + 16], eax
    mov eax, [0x504]
    mov [edi + 20], eax
    mov eax, [0x508]
    mov [edi + 24], eax
    popfd
    ret
serial:
    mov dx, 0x3f8
    out dx, al
    ret
irq0:
    push eax
    inc dword [0x500]
    mov al, 0x20
    out 0x20, al
    pop eax
    iret
irq8:
    push eax
    inc dword [0x504]
    mov al, 0x0c
    out 0x70, al
    in al, 0x71
    mov al, 0x20
    out 0xa0, al
    out 0x20, al
    pop eax
    iret
align 8
 gdt: dq 0
      dq 0x00cf9a000000ffff
      dq 0x00cf92000000ffff
 gdtr: dw 23
       dd gdt
 idtr: dw 2047
       dd 0x800
times 510-($-$$) db 0
dw 0xaa55
