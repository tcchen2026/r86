; Calibration guest. Build: nasm -f bin loop.asm -o loop.bin
; Sector 0 enters protected mode and reads the remaining sectors by ATA PIO.
; All measured code uses 32-bit flat segments and 4 KiB identity paging.
bits 16
org 0x7c00
%ifndef HOT_ITERS
%define HOT_ITERS 200000000
%endif
%ifndef PAGE_ITERS
%define PAGE_ITERS 1000000
%endif
%ifndef FRESH_ITERS
%define FRESH_ITERS 300
%endif
%ifndef FRESH_PAGES
%define FRESH_PAGES 256
%endif
%define WARM_ITERS 2048
%define COLD_ITERS 2
%define BLOCKS 130
%define CHAIN_STEPS 915
%define FRESH_BASE 0x100000
%define COLD_BASE 0x200000
%define DATA 0x6000
%define checksum (DATA+256)
%define iterations (DATA+260)

    cli
    cld
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7000
    in al, 0x92
    or al, 2
    and al, 0xfe
    out 0x92, al
    lgdt [gdtr]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp 8:loader
bits 32
loader:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov esp, 0x7000
    mov dx, 0x3f6
    mov al, 2                   ; no ATA IRQs
    out dx, al
    mov dx, 0x1f2
    mov al, (image_end-$$)/512-1
    out dx, al
    inc dx
    mov al, 1                   ; LBA 1
    out dx, al
    inc dx
    xor al, al
    out dx, al
    inc dx
    out dx, al
    inc dx
    mov al, 0xe0
    out dx, al
    inc dx
    mov al, 0x20
    out dx, al
    mov ebp, (image_end-$$)/512-1
    mov edi, 0x7e00
.read:
    mov dx, 0x1f7
.wait:
    in al, dx
    test al, 1
    jnz .failed
    test al, 0x80
    jnz .wait
    test al, 8
    jz .wait
    mov dx, 0x1f0
    mov ecx, 256
    rep insw
    dec ebp
    jnz .read
    jmp main
.failed:
    mov dx, 0x3f8
    mov al, '!'
    out dx, al
    hlt
    jmp .failed
align 8
 gdt: dq 0, 0x00cf9a000000ffff, 0x00cf92000000ffff
 gdtr: dw 23
       dd gdt
; The runner reads this fixed header, also in quick-build variants.
times 448-($-$$) db 0
 db 'R86LOOP1'
 dd HOT_ITERS, PAGE_ITERS, FRESH_ITERS, FRESH_PAGES
 dd COLD_ITERS, BLOCKS, CHAIN_STEPS, WARM_ITERS
 times 510-($-$$) db 0
 dw 0xaa55

main:
    mov dword [checksum], 0
    mov edi, 0x10000
    xor eax, eax
    mov ecx, 2048
    rep stosd
    mov dword [0x10000], 0x11003
    mov edi, 0x11000
    mov eax, 3
    mov ecx, 1024
.pt:
    stosd
    add eax, 4096
    loop .pt
    mov eax, 0x10000
    mov cr3, eax
    mov eax, cr0
    or eax, 0x80010000           ; PG and WP, ordinary 4 KiB pages
    mov cr0, eax

    ; Copy before the timed phases. These pages have never executed.
    mov edi, FRESH_BASE
    call copy_pages
    mov edi, COLD_BASE
    call copy_pages

    ; Warm marker formatting, including both hexadecimal digit paths.
    mov ecx, 3
.markers:
    mov esi, warm
    call mark
    push ecx
    mov ebx, 0xa9b8c7d6
    call hex32
    mov al, 10
    out dx, al
    pop ecx
    loop .markers
    call gap
    mov ecx, WARM_ITERS
    call init_hot
    call hot
    mov esi, p1_start
    call mark
    mov ecx, HOT_ITERS
    call init_hot
    call hot
    xor eax, ebx
    xor eax, [DATA+7*4]
    xor [checksum], eax
    mov esi, p1_end
    call mark
    call gap

    mov ecx, WARM_ITERS
    call init_page
    mov ebp, page_body
    lea esi, [ebp+97*32]
    call ebp
    mov esi, p2_start
    call mark
    mov ecx, PAGE_ITERS
    call init_page
    mov ebp, page_body
    lea esi, [ebp+97*32]
    call ebp
    xor eax, ebx
    xor [checksum], eax
    mov esi, p2_end
    call mark
    call gap

    mov esi, p3_start
    call mark
    mov ebp, FRESH_BASE
    mov dword [iterations], FRESH_ITERS
    call run_pages
    xor eax, ebx
    xor [checksum], eax
    mov esi, p3_end
    call mark
    call gap

    mov esi, p4_start
    call mark
    mov ebp, COLD_BASE
    mov dword [iterations], COLD_ITERS
    call run_pages
    xor eax, ebx
    xor [checksum], eax
    mov esi, p4_end
    call mark
    call gap

    mov esi, done
    call puts
    mov ebx, [checksum]
    call hex32
    mov al, 10
    out dx, al
    mov esi, finished
    call puts
    cli
.halt:
    hlt
    jmp .halt

; Keep serial endpoints apart even when a native debugger slice overshoots.
; This warmed gap is outside every measured interval.
gap:
    mov ecx, 65536
.next:
    dec ecx
    jnz .next
    ret

copy_pages:
    mov edx, FRESH_PAGES
.next:
    mov esi, page_body
    mov ecx, 1024
    rep movsd
    dec edx
    jnz .next
    ret

init_hot:
    mov eax, 1
    mov ebx, 2
    mov esi, DATA
    mov edi, 7
    mov dword [DATA+7*4], 3
    ret
hot:
    add eax, ebx
    xor ebx, eax
    mov edx, [esi+edi*4]
    add edx, eax
    mov [esi+edi*4], edx
    dec ecx
    cmp ecx, 0
    jnz hot
    ret

init_page:
    mov eax, 1
    mov ebx, 2
    mov edi, 0x13579bdf
    ret
run_pages:
    call init_page
    mov edx, FRESH_PAGES
.next:
    lea esi, [ebp+97*32]
    mov ecx, [iterations]
    call ebp
    add ebp, 4096
    dec edx
    jnz .next
    ret

; Preserve the arithmetic registers. Deterministic TSC is the guest step clock.
mark:
    pushad
    rdtsc
    push eax
    push edx
    call puts
    pop ebx
    call hex32
    pop ebx
    call hex32
    mov al, 10
    out dx, al
    popad
    ret
puts:
    mov dx, 0x3f8
.next:
    lodsb
    test al, al
    jz .done
    out dx, al
    jmp .next
.done:
    ret
hex32:
    mov ecx, 8
    mov dx, 0x3f8
.next:
    rol ebx, 4
    mov eax, ebx
    and al, 15
    cmp al, 10
    jb .digit
    add al, 7
.digit:
    add al, '0'
    out dx, al
    loop .next
    ret
warm: db 'L1 WARM ', 0
p1_start: db 'L1 P1 START ', 0
p1_end:   db 'L1 P1 END ', 0
p2_start: db 'L1 P2 START ', 0
p2_end:   db 'L1 P2 END ', 0
p3_start: db 'L1 P3 START ', 0
p3_end:   db 'L1 P3 END ', 0
p4_start: db 'L1 P4 START ', 0
p4_end:   db 'L1 P4 END ', 0
done:     db 'L1 CHECKSUM ', 0
finished: db 'L1 DONE', 10, 0

; Exactly one aligned code page. 128 bodies plus two call continuations =
; 130 block visits / chain. Every visited block has 7 or 8 instructions.
; Jcc outcomes are fixed (two taken, two not taken); padding is never executed.
; Relative edges and ESI (page base + 97*32) make the bytes position-independent.
times 0x9000-($-$$+0x7c00) db 0
page_body:
%macro arithmetic 0
    add eax, ebx
    xor ebx, eax
    rol eax, 1
    inc ebx
    add ebx, edi
    xor eax, edi
%endmacro
%assign i 0
%rep 128
%assign j i+1
%assign k i+2
%if i != 48 && i != 112
    times i*32-($-page_body) db 0xcc
block_%+i:
    arithmetic
%if i == 31 || i == 63
    call block_%+j
    arithmetic                  ; return continuation is a full block
    jmp short block_%+k
%elif i == 32 || i == 64
    ret
%elif i == 15 || i == 79
    cmp edi, edi
    je block_%+j             ; taken, skips padding
    ud2
%elif i == 47 || i == 111
    cmp edi, edi
    jne short bad_%+i           ; not taken, falls into next body
block_%+j:
    arithmetic
    jmp short block_%+k
bad_%+i:
    ud2
%elif i == 96
    jmp esi
%elif i == 127
    dec ecx
    jnz page_body
    ret
%else
    jmp block_%+j
%endif
%endif
%assign i i+1
%endrep
times 4096-($-page_body) db 0xcc
image_end:
