; Standalone 64 KiB BIOS: descriptor accessed-bit writes and segment-load commits.
; Build: nasm -f bin tests/roms/segment_accessed.asm -o /tmp/segment_accessed.bin
; Use as a reset BIOS in r86 or Bochs p2_klamath (ROM address FFFF0000h).
; COM1 ends with R86_SEGMENT_ORACLE_DONE. Compare every field, not just the marker.
; Rows: PFEC (FFFFFFFF means none), CR2, descriptor AR byte, GS, ESP delta,
; EBX, descriptor-page PTE A/D. WP-protected A=0 must fault at byte 601Dh with
; PFEC 3, preserve selectors/ESP/EBX and leave A clear. A=1 needs no write.
; Writable/WP0 loads set descriptor A and PTE.D. Final POP SS wraps old 16-bit
; SP and preserves upper ESP while loading a 32-bit stack: DEAD0000.
; Full output matched Bochs 3.0 p2_klamath, r86 interpreter and ARM64 JIT.
; Unit checks also cover CPL3 supervisor access and operand16.
; The INT rows additionally check CS accessed writes during interrupt entry,
; including a supervisor write fault and a read-only already-accessed entry.
bits 16
org 0xf0000

start:
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x7000
    lgdt [cs:gdt_ptr - start]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp dword 8:protected

align 8
gdt:
    dq 0, 0x00cf9b000000ffff, 0x00cf93000000ffff
gdt_ptr:
    dw 23
    dd gdt
idt_ptr:
    dw 0x7ff
    dd 0x3000

bits 32
protected:
    mov ax, 16
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov esp, 0x90000
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
    mov eax, page_fault
    mov [0x3070], ax
    mov word [0x3072], 8
    mov word [0x3074], 0x8e00
    shr eax, 16
    mov [0x3076], ax
    mov eax, int_handler
    mov [0x3180], ax
    mov word [0x3182], 0x18
    mov word [0x3184], 0x8e00
    shr eax, 16
    mov [0x3186], ax
    lidt [idt_ptr]
    mov edi, 0x1000
    xor eax, eax
    mov ecx, 1024
    rep stosd
    mov eax, 3
    mov ecx, 1024
.map:
    stosd
    add eax, 0x1000
    loop .map
    mov dword [0x1000], 0x2003
    mov eax, 0x1000
    mov cr3, eax
    mov eax, cr0
    or eax, 0x80010000
    mov cr0, eax


    mov esi, gdt
    mov edi, 0x6000
    mov ecx, 6
    rep movsd
    mov dword [0x6018], 0xffff
    mov dword [0x6020], 0xffff
    mov dword [0x6024], 0x00809300
    lgdt [ram_gdt]
    mov word [0x7004], 0x18
    mov dword [0x7000], 0x12345678

; Each case prints PFEC, CR2, AR, selector, ESP delta, EBX and PTE A/D.
; A load with A=0 must write byte 601Dh before committing any destination.
%macro prepare 3
    mov eax, cr0
    or eax, 0x10000
    mov cr0, eax
    mov dword [0x2018], 0x6003
    mov eax, cr3
    mov cr3, eax
    mov ax, 16
    mov gs, ax
    mov ss, ax
    mov esp, 0x90000
    mov dword [0x601c], %2
    mov dword [0x2018], %1
    mov eax, cr0
%if %3 == 0
    and eax, ~0x10000
%endif
    mov cr0, eax
    mov eax, cr3
    mov cr3, eax
    mov dword [0x5000], -1
    mov eax, 0xdeadbeef
    mov cr2, eax
    mov ebx, eax
%endmacro
%macro probe 2
    mov esi, %1
    call puts
    mov [0x5010], esp
    mov dword [0x5004], %%resume
    mov eax, 0x18
    %2
%%resume:
    mov eax, [0x5000]
    call hex
    mov eax, cr2
    call hex
    movzx eax, byte [0x601d]
    call hex
    mov eax, gs
    call hex
    mov eax, esp
    sub eax, [0x5010]
    call hex
    mov eax, ebx
    call hex
    mov eax, [0x2018]
    and eax, 0x60
    call hex
    call newline
%endmacro
    prepare 0x6003, 0x00cf9200, 1
    probe writable_msg, {mov gs, ax}
    prepare 0x6001, 0x00cf9200, 1
    probe readonly_msg, {mov gs, ax}
    prepare 0x6001, 0x00cf9300, 1
    probe accessed_msg, {mov gs, ax}
    prepare 0x6001, 0x00cf9200, 0
    probe wpclear_msg, {mov gs, ax}
    prepare 0x6001, 0x00cf9200, 1
    push dword 0x18
    probe pop_msg, {pop gs}
    prepare 0x6001, 0x00cf9200, 1
    probe lgs_msg, {lgs ebx, [0x7000]}
    prepare 0x6001, 0x00cf9200, 1
    push dword 0x18
    probe popss_msg, {pop ss}
    prepare 0x6001, 0x00cf9200, 1
    probe lss_msg, {lss ebx, [0x7000]}
    prepare 0x6003, 0x00cf9a00, 1
    probe int_write_msg, {int 0x30}
    prepare 0x6001, 0x00cf9a00, 1
    probe int_fault_msg, {int 0x30}
    prepare 0x6001, 0x00cf9b00, 1
    probe int_accessed_msg, {int 0x30}

; POP SS advances with the old SS.B, including SP wrap and upper-ESP retention.
    prepare 0x6003, 0x00cf9200, 1
    mov word [0xfffc], 0x18
    mov ax, 0x20
    mov ss, ax
    mov esp, 0xdeadfffc
    pop ss
    mov ebx, esp
    mov esp, 0x90000
    mov esi, wrap_msg
    call puts
    mov eax, ebx
    call hex
    call newline
    mov esi, done_msg
    call puts
.halt:
    hlt
    jmp .halt

ram_gdt:
    dw 39
    dd 0x6000

int_handler:
    iretd

page_fault:
    push eax
    mov eax, [esp + 4]
    mov [0x5000], eax
    mov eax, [esp + 16]
    mov [0x5008], eax
    mov eax, [0x5004]
    mov [esp + 8], eax
    pop eax
    add esp, 4
    iretd

putc:
    push eax
    push edx
    mov dx, 0x3fd
.wait:
    in al, dx
    test al, 0x20
    jz .wait
    mov al, [esp + 4]
    mov dx, 0x3f8
    out dx, al
    pop edx
    pop eax
    ret
int_write_msg: db 'INT RW ', 0
int_fault_msg: db 'INT RO ', 0
int_accessed_msg: db 'INT A1 ', 0
puts:
    lodsb
    test al, al
    jz .done
    call putc
    jmp puts
.done:
    ret
newline:
    mov al, 10
    jmp putc
hex:
    pushad
    mov ebx, eax
    mov ecx, 8
.digit:
    rol ebx, 4
    mov eax, ebx
    and al, 15
    add al, '0'
    cmp al, '9'
    jbe .print
    add al, 7
.print:
    call putc
    loop .digit
    mov al, ' '
    call putc
    popad
    ret

writable_msg: db 'RW ',0
readonly_msg: db 'RO ',0
accessed_msg: db 'A1 ',0
wpclear_msg: db 'WP0 ',0
pop_msg: db 'POP ',0
lgs_msg: db 'LGS ',0
popss_msg: db 'POPSS ',0
lss_msg: db 'LSS ',0
wrap_msg: db 'WRAP ',0
done_msg: db 'R86_SEGMENT_ORACLE_DONE',10,0

times 0xfff0 - ($ - $$) db 0xff
bits 16
    jmp 0xf000:0
times 0x10000 - ($ - $$) db 0xff
