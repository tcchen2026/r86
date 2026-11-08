; Execute from RAM, modify instructions before/after publication, switch DS,
; modify a watched second page through word/dword writes from ordinary data,
; reload warmed segment descriptors (including null/A-bit faults), remap
; warmed 4 KiB/4 MiB pages and require a user fetch fault after IRET.
; Revisit enough code pages to force reclamation. Guest checks observe stale
; translations, permission leaks and recycled function slots.
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
    lgdt [cs:gdtr - start]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp dword 8:protected
align 8
gdt: dq 0, 0x00cf9a000000ffff, 0x00cf92000000ffff, 0x00cf92001000ffff
    dq 0x00cffa000000ffff, 0x00cff2000000ffff, 0x0000890180000067
gdt_end:
gdtr: dw gdt_end - gdt - 1
    dd gdt
ram_gdtr: dw gdt_end - gdt - 1
    dd 0x19000
idtr: dw 0x7ff
    dd 0x17000
bits 32
protected:
    mov ax, 16
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov esp, 0x90000
    mov esi, payload
    mov edi, 0x100000
    mov ecx, payload_end - payload
    rep movsb
    jmp 0x100000

payload:
    xor esi, esi
    mov ecx, 200000
    xor edi, edi
hot:
    add edi, byte 1
    dec ecx
    jnz hot
    test esi, esi
    jnz changed
    cmp edi, 200000
    jne fail
    mov byte [0x100000 + hot - payload + 2], 2
    mov esi, 1
    mov dx, 0x3f8
    mov al, 'W'
    out dx, al
    mov ecx, 500000
    xor edi, edi
    jmp hot
changed:
    cmp edi, 1000000
    jne fail
    ; Warm one load, then execute it with a different DS base and return to
    ; flat DS. Existing translations must not waive the new segment semantics.
    mov dword [0x200000], 0x12345678
    mov dword [0x201000], 0x87654321
    mov ebp, 2
segment_round:
    mov ecx, 512
segment_hot:
    call read_value
    cmp eax, 0x12345678
    jne fail
    dec ecx
    jnz segment_hot
    mov ax, 24
    mov ds, ax
    call read_value
    cmp eax, 0x87654321
    jne fail
    mov ax, 16
    mov ds, ax
    dec ebp
    jnz segment_round
    ; Change an instruction after publication, including any same-module edge
    ; already compiled to the helper. Source-version checks must retire it.
    mov ecx, 2048
published_hot:
    call published_value
    cmp eax, 41
    jne fail
    dec ecx
    jnz published_hot
    mov byte [0x100000 + published_value - payload + 1], 42
    call published_value
    cmp eax, 42
    jne fail
    ; The first page is ordinary writable data, the second has translated code.
    ; Neither a word nor a dword store may reuse only the first page's proof.
    mov ebx, 0x220000
    mov word [ebx], 0xc340      ; INC EAX; RET
    mov ecx, 2048
split_code_hot:
    mov eax, 100
    call ebx
    cmp eax, 101
    jne fail
    dec ecx
    jnz split_code_hot
    mov word [0x21ffff], 0x48aa ; second-page INC becomes DEC
    mov ecx, 2048
split_code_changed:
    mov eax, 100
    call ebx
    cmp eax, 99
    jne fail
    dec ecx
    jnz split_code_changed
    mov dword [0x21fffd], 0x40bbccdd ; second-page DEC becomes INC
    mov eax, 100
    call ebx
    cmp eax, 101
    jne fail
    ; Bulk fills retire the whole range once; the first word is executable.
    mov edi, ebx
    mov ecx, 16
    mov eax, 0xc348
    rep stosw
    mov eax, 100
    call ebx
    cmp eax, 99
    jne fail
    mov edi, ebx
    mov ecx, 16
    mov eax, 0x9090c340
    rep stosd
    mov eax, 100
    call ebx
    cmp eax, 101
    jne fail
    ; Warm execute links, replace their PTE/PDE without touching code, and
    ; invalidate the translation. A valid source version alone is insufficient.
    mov byte [0x200000], 0xb8
    mov dword [0x200001], 0x11223344
    mov byte [0x200005], 0xc3
    mov byte [0x201000], 0xb8
    mov dword [0x201001], 0x55667788
    mov byte [0x201005], 0xc3
    mov byte [0x401000], 0xb8
    mov dword [0x401001], 0x10203040
    mov byte [0x401005], 0xc3
    mov byte [0x801000], 0xb8
    mov dword [0x801001], 0x50607080
    mov byte [0x801005], 0xc3
    mov edi, 0x10000
    xor eax, eax
    mov ecx, 1024
    rep stosd
    mov ecx, 4096
    mov eax, 3
identity:
    stosd
    add eax, 4096
    loop identity
    mov dword [0x10000], 0x11003
    mov dword [0x10004], 0x12003
    mov dword [0x10008], 0x13003
    mov dword [0x1000c], 0x14003
    mov dword [0x13000], 0x200003
    mov eax, 0x10000
    mov cr3, eax
    mov eax, cr0
    or eax, 0x80010000
    mov cr0, eax
    mov ebx, 0x800000
    mov ecx, 2048
page_hot:
    call ebx
    cmp eax, 0x11223344
    jne fail
    loop page_hot
    mov dword [0x13000], 0x201003
    invlpg [0x800000]
    call ebx
    cmp eax, 0x55667788
    jne fail
    mov eax, cr4
    or eax, 0x10
    mov cr4, eax
    mov dword [0x10004], 0x400083
    invlpg [0x401000]
    mov ebx, 0x401000
    mov ecx, 2048
large_hot:
    call ebx
    cmp eax, 0x10203040
    jne fail
    loop large_hot
    mov dword [0x10004], 0x800083
    invlpg [0x403000]          ; another piece of the cached 4 MiB mapping
    call ebx
    cmp eax, 0x50607080
    jne fail
    ; A warmed supervisor-only execute link must not authorize CPL 3. Keep
    ; code bytes/mapping unchanged across IRET, then require a user #PF.
    mov esi, gdt
    mov edi, 0x19000
    mov ecx, (gdt_end - gdt) / 4
    rep movsd
    lgdt [ram_gdtr]
    mov edi, 0x18000
    xor eax, eax
    mov ecx, 26
    rep stosd
    mov dword [0x18004], 0x90000
    mov word [0x18008], 16
    mov word [0x18066], 104
    mov ax, 48
    ltr ax
    mov edi, 0x17000
    xor eax, eax
    mov ecx, 512
    rep stosd
    mov eax, 0x100000 + user_fault - payload
    mov [0x17070], ax
    mov word [0x17072], 8
    mov word [0x17074], 0x8e00
    shr eax, 16
    mov [0x17076], ax
    mov eax, 0x100000 + kernel_gate - payload
    mov [0x17180], ax
    mov word [0x17182], 8
    mov word [0x17184], 0xee00
    shr eax, 16
    mov [0x17186], ax
    mov eax, 0x100000 + null_segment_fault - payload
    mov [0x17068], ax
    mov word [0x1706a], 8
    mov word [0x1706c], 0x8e00
    shr eax, 16
    mov [0x1706e], ax
    lidt [idtr]
    ; A compiled MOV FS must reload current descriptor bytes, preserve flags,
    ; and honor a changed base. The selector itself comes from ordinary RAM.
    mov dword [0x240000], 0x11224488
    mov dword [0x241000], 0x22448811
    mov dword [0x242000], 0x44881122
    mov word [0x1a000], 24
    mov esi, 0x22448811
    mov ebp, 2
segment_reload_round:
    mov ecx, 4096
segment_reload_hot:
    stc
    call load_fs_value
    jnc fail
    cmp edx, esi
    jne fail
    dec ecx
    jnz segment_reload_hot
    mov word [0x19000 + 24 + 2], 0x2000
    mov esi, 0x44881122
    dec ebp
    jnz segment_reload_round
    ; Clearing descriptor A requires a real architectural write on the next
    ; load. A read-only descriptor page must fault before changing hidden FS.
    or byte [0x19000 + 8 + 5], 1  ; the #PF gate's CS must need no descriptor write
    mov word [0x19000 + 24 + 2], 0x1000
    and byte [0x19000 + 24 + 5], 0xfe
    mov dword [0x11064], 0x19001
    invlpg [0x19000]
    mov eax, cr0
    or eax, 0x10000               ; supervisor writes also obey PTE.RW
    mov cr0, eax
    mov eax, [0x19000]             ; warm only a read proof in the new context
    mov dword [0x1a004], 0
    call load_fs_value
    cmp edx, 0x44881122           ; fault retained the previous hidden base
    jne fail
    cmp dword [0x1a004], 1
    jne fail
    test byte [0x19000 + 24 + 5], 1
    jnz fail
    mov dword [0x11064], 0x19003
    invlpg [0x19000]
    call load_fs_value
    cmp edx, 0x22448811
    jne fail
    test byte [0x19000 + 24 + 5], 1
    jz fail
    ; Null FS is a valid load but the following memory operand must #GP(0).
    mov word [0x1a000], 0
    mov edx, 0xa5a55a5a
    call load_fs_value
    cmp edx, 0xa5a55a5a
    jne fail
    cmp dword [0x1a004], 2
    jne fail
    mov word [0x1a000], 24
    call load_fs_value
    ; Warm a flat DS load, then use that same compiled entry with a based DS.
    ; Native continuation must not keep the old flat-segment qualification.
    mov ecx, 4096
flat_ds_reload_hot:
    mov ax, 16
    call load_ds_value
    cmp edx, 0x11224488
    jne fail
    dec ecx
    jnz flat_ds_reload_hot
    mov ax, 24
    call load_ds_value
    cmp edx, 0x22448811
    jne fail
    mov dword [0x10000], 0x11007
    mov dword [0x11400], 0x100007   ; user code at 1 MiB
    mov dword [0x11054], 0x15007    ; user stack below 16000h
    invlpg [0x100000]
    invlpg [0x15000]
    mov ebx, 0x800000
    mov ecx, 2048
supervisor_hot:
    call ebx
    cmp eax, 0x55667788
    jne fail
    loop supervisor_hot
    mov ax, 0x2b
    mov ds, ax
    mov es, ax
    push dword 0x2b
    push dword 0x16000
    push dword 2
    push dword 0x23
    push dword 0x100000 + user_start - payload
    iretd
user_start:
    mov ecx, 100000
user_hot:
    dec ecx
    jnz user_hot
    xor ebp, ebp
    call ebx                      ; supervisor PDE: fetch must fault
    xor eax, eax
    int 0x30                      ; reaching here is a failure
user_resume:
    mov eax, 1
    int 0x30
user_fault:
    mov eax, cr2
    cmp eax, 0x19000 + 24 + 5
    je descriptor_write_fault
    mov eax, [esp]
    and eax, 15                  ; present, user, read; no reserved-bit fault
    cmp eax, 5
    jne fail
    mov eax, cr2
    cmp eax, 0x800000
    jne fail
    mov dword [esp + 4], 0x100000 + user_resume - payload
    mov ebp, 1
    add esp, 4
    iretd
descriptor_write_fault:
    cmp dword [esp], 3           ; supervisor write to a present read-only page
    jne fail
    cmp dword [esp + 4], 0x100000 + load_fs_value - payload
    jne fail
    inc dword [0x1a004]
    mov dword [esp + 4], 0x100000 + read_fs_value - payload
    add esp, 4
    iretd
null_segment_fault:
    cmp dword [esp], 0
    jne fail
    cmp dword [esp + 4], 0x100000 + read_fs_value - payload
    jne fail
    mov ax, fs
    test ax, ax
    jnz fail
    inc dword [0x1a004]
    mov dword [esp + 4], 0x100000 + fs_value_return - payload
    add esp, 4
    iretd
kernel_gate:
    cmp eax, 1
    jne fail
    cmp ebp, 1
    jne fail
    add esp, 20
    mov ax, 16
    mov ds, ax
    mov es, ax
    mov eax, cr0
    and eax, 0x7fffffff
    mov cr0, eax
    xor eax, eax
    mov ebx, 0x300000
generate:
    mov byte [ebx], 0xb8       ; MOV EAX, page index; RET
    mov [ebx + 1], eax
    mov byte [ebx + 5], 0xc3
    add ebx, 4096
    inc eax
    cmp eax, 384
    jb generate
    xor edi, edi
    mov ebp, 2
round:
    mov ebx, 0x300000
    xor esi, esi
page:
    mov ecx, 20000
invoke:
    call ebx
    add edi, eax
    dec ecx
    jnz invoke
    add ebx, 4096
    inc esi
    cmp esi, 384
    jb page
    dec ebp
    jnz round
    cmp edi, 2941440000
    jne fail
    mov dx, 0x3f8
    mov al, 'K'
    out dx, al
    hlt
fail:
    mov dx, 0x3f8
    mov al, 'F'
    out dx, al
    hlt
read_value:
    mov eax, [0x200000]
    ret
published_value:
    mov eax, 41
    ret
load_fs_value:
    mov fs, [ss:0x1a000]
read_fs_value:
    mov edx, [fs:0x240000]
fs_value_return:
    ret
load_ds_value:
    mov ds, ax
    mov edx, [0x240000]
    mov ax, 16
    mov ds, ax
    ret
payload_end:
times 0xfff0 - ($ - $$) db 0
bits 16
    jmp 0xf000:0
times 0x10000 - ($ - $$) db 0
