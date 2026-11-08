#!/usr/bin/env python3
"""Compare the single interpreter's software/native-primitives builds on ISS/RTL.

Each guest must agree in full guest RAM, registers, flags, serial and completion
counts across both builds. Within a build, ISS/RTL additionally compare full
native RAM and native instruction counts. These are synthetic correctness gates,
not the fixed OS performance corpus. Prepare both templates with tinycore.py;
use --extensions for the native build, and --engine rtl for its one-bank runner.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import struct
import subprocess

from jit_checks import CASES as SHARED_CASES, PAGING, ROOT, interrupt_tables, serial


def conditions():
    """Both outcomes of all 16 predicates, with independent boolean expectations."""
    source = []
    for flags in (0, 0x8D5, 1, 4, 0x40, 0x80, 0x800, 0x881):
        cf, pf, zf, sf, of = [bool(flags & mask) for mask in (1, 4, 0x40, 0x80, 0x800)]
        taken = (of, not of, cf, not cf, zf, not zf, cf or zf, not (cf or zf),
                 sf, not sf, pf, not pf, sf != of, sf == of, zf or sf != of, not zf and sf == of)
        for cc, expected in enumerate(taken):
            label = f"condition_{flags}_{cc}"
            # Force the SHORT opcode rather than NASM's near-Jcc relaxation.
            source += [f"push dword {flags | 2}", "popfd", f"db {0x70 + cc}, 5",
                       f"jmp near {'fail' if expected else label}",
                       f"jmp near {label if expected else 'fail'}", label + ":"]
    return "\n".join(source) + "\njmp success\n"


STORE_SMC = """
    ; Patch aligned immediate bytes with the ordinary shared interpreter store. Both the
    ; identity and paging-alias versions must reread them on the next execution.
    mov eax, 1
    mov [patch + ALIAS], eax
    mov ebx, 1
    mov ecx, 32
    jmp target
align 4
    db 0x90, 0x90, 0x90
target:
    db 0xb8
patch: dd 1
    cmp eax, ebx
    jne fail
    dec ecx
    jnz target
    cmp ebx, 7
    je success
    mov eax, 7
    mov [patch + ALIAS], eax
    mov ebx, eax
    mov ecx, 32
    jmp target
"""


def addressing():
    # Independent expected values plus complete guest-RAM comparison catch a
    # wrong EA even if a matching wrong store/load would cancel in registers.
    expressions = ["eax", "ebp", "esp", "eax-4", "ebp+124", "esp+4096", "0x28000"]
    for base in ("eax", "ebp", "esp", ""):
        for index in ("ecx", "ebp"):
            for scale in (1, 2, 4, 8):
                expressions.append((base + "+" if base else "") + f"{index}*{scale}+0x1000")
    source = []
    for number, expr in enumerate(expressions):
        value = 0x12340000 + number
        source += ["mov esp, 0x200000", "mov eax, 0x300000", "mov ebp, 0x400000", "mov ecx, 0x10000",
                   f"mov ebx, {value}", f"mov [{expr}], ebx", f"add dword [{expr}], byte -3",
                   f"mov edi, [{expr}]", f"cmp edi, {value - 3}", "jne fail",
                   f"sub dword [{expr}], ebx", f"cmp dword [{expr}], byte -3", "jne fail"]
    return "\n".join(source) + "\njmp success\n"


def wide_fetch_lanes():
    """Raw immediates and displacements at all native word lanes, reexecuted."""
    source = ["mov dword [0x28000], 0x76543210", "mov ebx, 0x20000", "mov ecx, 16", "again:"]
    for lane in range(4):
        # Exact bytes prevent assembler choices from silently changing the form.
        source += ["align 4, db 0x90", f"times {lane} db 0x90",
                   "db 0xb8", "dd 0x89abcdef", "cmp eax, 0x89abcdef", "jne fail",
                   "align 4, db 0x90", f"times {lane} db 0x90",
                   "db 0x66, 0xb8", "dw 0x1234", "cmp eax, 0x89ab1234", "jne fail",
                   "align 4, db 0x90", f"times {lane} db 0x90",
                   "db 0x8b, 0x83", "dd 0x8000", "cmp eax, 0x76543210", "jne fail"]
    return "\n".join(source + ["dec ecx", "jnz again", "jmp success"]) + "\n"


RMW_FAULT = """
    mov dword [0x5800], 0xa001
    mov dword [0xa000], 0x7fffffff
    mov eax, cr0
    or eax, 0x10000
    mov cr0, eax
    push dword 0x47
    popfd
faulting:
    add dword [0x600000], byte 1
    pushfd
    pop eax
    and eax, 0x8d5
    cmp eax, 0x894
    jne fail
    cmp dword [0x600000], 0x80000000
    jne fail
    cmp dword [0x10000], 1
    jne fail
    jmp success
fault_handler:
    cmp dword [esp], 3
    jne fail
    cmp dword [esp+4], faulting
    jne fail
    mov eax, cr2
    cmp eax, 0x600000
    jne fail
    mov eax, [esp+12]
    and eax, 0x8d5
    cmp eax, 0x45
    jne fail
    cmp dword [0xa000], 0x7fffffff
    jne fail
    inc dword [0x10000]
    mov dword [0x5800], 0xa003
    invlpg [0x600000]
    add esp, 4
    iretd
"""

IMMEDIATE_FETCH_FAULT = """
    ; The immediate is on an absent code page; data is absent too. Fetch
    ; must fault first, then the retried instruction must raise the write fault.
    mov word [0x8ffa], 0x0583
    mov dword [0x8ffc], 0x600000
    mov word [0x9000], 0xc301
    mov dword [0xa000], 0x11223344
    mov dword [0x4024], 0
    invlpg [0x9000]
    call 0x8ffa
    cmp dword [0x10000], 2
    jne fail
    cmp dword [0x600000], 0x11223345
    jne fail
    jmp success
fault_handler:
    cmp dword [esp+4], 0x8ffa
    jne fail
    mov eax, cr2
    cmp dword [0x10000], 0
    jne .data
    cmp eax, 0x9000
    jne fail
    test dword [esp], 0xf
    jne fail
    mov dword [0x4024], 0x9003
    invlpg [0x9000]
    jmp .return
.data:
    cmp eax, 0x600000
    jne fail
    cmp dword [esp], 2
    jne fail
    mov dword [0x5800], 0xa003
    invlpg [0x600000]
.return:
    inc dword [0x10000]
    add esp, 4
    iretd
"""

def flags_cases():
    """Independent arithmetic expectations; test Rust->custom-op integration.

    Include high byte writes, incoming carry, INC/DEC preserving CF, overflow,
    parity/auxiliary carry, unchanged DF/IOPL, and shift-by-zero preserving all
    flags. The ISA's exhaustive flag tests are separate; these guests test
    commit callers, including XFGET's merge into the old control flags.
    """
    source = []
    for bits, reg, shift in ((8, "al", 0), (8, "ah", 8), (16, "ax", 0), (32, "eax", 0)):
        mask, sign = (1 << bits) - 1, 1 << (bits - 1)
        pairs = ((0, 1), (mask, 1), (sign - 1, 1), (sign, mask), (15, 1), (85, 15))
        for op in ("add", "adc", "sub", "sbb", "inc", "dec", "or", "and", "xor", "neg", "shl", "shr", "sar"):
            for lhs, rhs in pairs:
                for carry in ((0, 1) if op in ("adc", "sbb", "inc", "dec") else (1,)):
                    value, cf, af, of = 0, 0, 0, 0
                    if op in ("add", "adc", "inc"):
                        right = 1 if op == "inc" else rhs
                        total = lhs + right + (carry if op == "adc" else 0)
                        value = total & mask
                        cf = carry if op == "inc" else int(total > mask)
                        af = bool((lhs ^ right ^ value) & 16)
                        of = bool(~(lhs ^ right) & (lhs ^ value) & sign)
                    elif op in ("sub", "sbb", "dec", "neg"):
                        left, right = (0, lhs) if op == "neg" else (lhs, 1 if op == "dec" else rhs)
                        borrow = carry if op == "sbb" else 0
                        value = (left - right - borrow) & mask
                        cf = carry if op == "dec" else int(left < right + borrow)
                        af = bool((left ^ right ^ value) & 16)
                        of = bool((left ^ right) & (left ^ value) & sign)
                    elif op in ("or", "and", "xor"):
                        value = {"or": lhs | rhs, "and": lhs & rhs, "xor": lhs ^ rhs}[op]
                    elif op == "shl":
                        value, cf = (lhs << 1) & mask, bool(lhs & sign)
                        of = bool(value & sign) != cf
                    elif op == "shr":
                        value, cf, of = lhs >> 1, lhs & 1, bool(lhs & sign)
                    else:
                        value, cf = (lhs >> 1) | (lhs & sign), lhs & 1
                    status = int(cf) | ((value & 255).bit_count() % 2 == 0) << 2
                    status |= int(af) << 4 | (value == 0) << 6 | bool(value & sign) << 7 | int(of) << 11
                    initial = (0xa5a55a5a & ~(mask << shift)) | (lhs << shift)
                    expected = (initial & ~(mask << shift)) | (value << shift)
                    control = 0x3400 if (lhs ^ rhs ^ bits) & 1 else 0x400
                    operand = "" if op in ("inc", "dec", "neg") else f", {1 if op in ('shl', 'shr', 'sar') else rhs}"
                    source += [f"mov eax, {initial}", f"push dword {2 | carry | control}", "popfd",
                               f"{op} {reg}{operand}", "pushfd", "pop edx",
                               f"cmp eax, {expected}", "jne fail", "and edx, 0x3cd5",
                               f"cmp edx, {status | control}", "jne fail"]
        source += ["push dword 0x3cd7", "popfd", f"shl {reg}, 0", "pushfd", "pop edx",
                   "and edx, 0x3cd5", "cmp edx, 0x3cd5", "jne fail"]
    return "\n".join(source) + "\njmp success\n"


# Warm a data mapping, then revoke W using CR0.WP without INVLPG. The load is
# still permitted but the old hardware write proof must no longer be accepted.
WP_CHANGE = """
    mov dword [0x5800], 0xa001
    mov dword [0x600000], 0x11223344
    mov dword [0x600000], 0x11223344
    mov dword [0x600000], 0x11223344
    mov eax, cr0
    or eax, 0x10000
    mov cr0, eax
faulting:
    mov dword [0x600000], 0x55667788
    cmp dword [0x10000], 1
    jne fail
    cmp dword [0x600000], 0x55667788
    jne fail
    jmp success
fault_handler:
    cmp dword [esp], 3
    jne fail
    cmp dword [esp+4], faulting
    jne fail
    cmp dword [0xa000], 0x11223344
    jne fail
    inc dword [0x10000]
    mov eax, cr0
    and eax, 0xfffeffff
    mov cr0, eax
    add esp, 4
    iretd
"""


CPL_CHANGE = """
    ; Add ring-3 code/data and a 32-bit TSS to the real descriptor table.
    mov dword [0x6020], 0x0000ffff
    mov dword [0x6024], 0x00cffb00
    mov dword [0x6028], 0x0000ffff
    mov dword [0x602c], 0x00cff300
    mov dword [0x6030], 0xb0000067
    mov dword [0x6034], 0x00008900
    mov word [0x6200], 55
    lgdt [0x6200]
    mov dword [0xb004], 0x1f0000
    mov word [0xb008], 0x18
    mov word [0xb066], 0x68
    mov ax, 0x30
    ltr ax
    ; The outer page directory permits users, but this leaf is supervisor-only.
    mov dword [0x5800], 0xa003
    mov dword [0xa000], 0x11223344
    mov ebx, [0x600000]
    mov ebx, [0x600000]
    mov ebx, [0x600000]
    mov ebx, 0xdecafbad
    push dword 0x2b
    push dword 0x200000
    push dword 2
    push dword 0x23
    push dword user_start
    iretd
user_start:
    mov ax, 0x2b
    mov ds, ax
    mov es, ax
faulting:
    mov ebx, [0x600000]
    ud2
fault_handler:
    cmp dword [esp], 5
    jne fail
    cmp dword [esp+4], faulting
    jne fail
    cmp dword [esp+8], 0x23
    jne fail
    cmp ebx, 0xdecafbad
    jne fail
    mov eax, cr2
    cmp eax, 0x600000
    jne fail
    cmp dword [0x600000], 0x11223344
    jne fail
    jmp success
"""

DATA_REMAP = """
    mov dword [0x5800], 0xa003
    mov dword [0xa000], 0x11223344
    mov dword [0xb000], 0x55667788
    mov ecx, 8
.warm:
    cmp dword [0x600000], 0x11223344
    jne fail
    loop .warm
    mov dword [0x5800], 0xb003
    ; CR3 reload must retire the hardware data proof even with an unchanged root.
    mov eax, cr3
    mov cr3, eax
    cmp dword [0x600000], 0x55667788
    jne fail
    mov dword [0x600000], 0x12345678
    cmp dword [0xb000], 0x12345678
    jne fail
    cmp dword [0xa000], 0x11223344
    jne fail
    jmp success
"""


CASES = dict(SHARED_CASES)
CASES.update({
    "tsc-observation-order": interrupt_tables(6, "tsc_fault_handler") + """
        ; Independent expected timestamps, including high-word/full wrap,
        ; WRMSR within a batch, an I/O boundary, a fault and partial REP work.
        mov ecx, 0x10
        mov eax, -2
        mov edx, -1
        wrmsr
        rdtsc                       ; MAX
        mov ebx, eax                ; 0
        mov esi, edx                ; 1
        rdmsr                       ; 2
        cmp eax, 2
        jne fail
        test edx, edx
        jnz fail
        cmp ebx, -1
        jne fail
        cmp esi, -1
        jne fail

        mov eax, 0xfffffffe
        mov edx, 0x12345678
        wrmsr
        nop                         ; +1
        rdtsc                       ; +2: carry into high word
        mov [0x10000], eax          ; +3
        mov [0x10004], edx          ; +4
        mov dx, 0x3f8               ; +5
        mov al, 'T'                 ; +6
        out dx, al                  ; +7: firmware boundary
        rdtsc                       ; +8, after entering another batch
        cmp eax, 6
        jne fail
        cmp edx, 0x12345679
        jne fail
        cmp dword [0x10000], 0
        jne fail
        cmp dword [0x10004], 0x12345679
        jne fail

        mov eax, 1000
        xor edx, edx
        wrmsr
    tsc_faulting:
        ud2                         ; 1001: attempt faults, no completion
    tsc_after_fault:
        cld
        mov edi, 0x20000
        mov ecx, 0x10
        mov eax, 100
        xor edx, edx
        wrmsr
        mov ecx, 9000               ; 101
        rep stosb                   ; 102..104: three attempts, one completion
        rdtsc                       ; 105
        cmp eax, 105
        jne fail
        test edx, edx
        jnz fail
        jmp success
    tsc_fault_handler:
        rdtsc                       ; 1002, after fault delivery
        cmp eax, 1002
        jne fail
        test edx, edx
        jnz fail
        cmp dword [esp], tsc_faulting
        jne fail
        mov dword [esp], tsc_after_fault
        iretd
    """,
    "cpl-supervisor-proof": interrupt_tables(14, "fault_handler")
        + PAGING.replace("mov eax, 3", "mov eax, 7").replace("0x4003", "0x4007").replace("0x5003", "0x5007")
        + CPL_CHANGE,
    "cr3-data-remap": PAGING + DATA_REMAP,
    "eflags-widths-carry": flags_cases(),
    "cr0-wp-change": interrupt_tables(14, "fault_handler") + PAGING + WP_CHANGE,
    "all-conditions": conditions(),
    "store-smc": "%define ALIAS 0\n" + STORE_SMC,
    "store-alias": "%define ALIAS 0x400000\n" + PAGING + STORE_SMC,
    "modrm-sib": addressing(),
    "ea-fresh-sib-pop-esp": """
        ; POP's destination uses the incremented ESP. It deliberately remains
        ; in the same software helper with its explicit ESP override.
        mov dword [0x200000], 0
        push dword 0x12345678
        pop dword [esp]
        cmp esp, 0x200000
        jne fail
        cmp dword [esp], 0x12345678
        jne fail
        mov ecx, 4
        mov dword [0x200010], 0
        push word 0xabcd
        pop word [esp+ecx*4]
        cmp esp, 0x200000
        jne fail
        cmp dword [esp+16], 0xabcd
        jne fail

        mov ebx, 0x1000
        mov edi, 0x2000
        mov ecx, 3
        mov ebp, 32
    .first:
        call ea_target
        cmp eax, 0x1005
        jne fail
        dec ebp
        jnz .first
        ; Replace the SIB in existing code, then mutate a source GPR as well.
        mov byte [ea_target+2], 0x8f
        mov ebp, 32
    .second:
        call ea_target
        cmp eax, 0x200b
        jne fail
        dec ebp
        jnz .second
        mov edi, 0x3000
        call ea_target
        cmp eax, 0x300b
        jne fail
        jmp success
    ea_target:
        db 0x8d, 0x44, 0x4b, 0xff ; LEA EAX,[EBX+ECX*2-1]
        ret
    """,
    "ea-cross-page-restart": interrupt_tables(14, "ea_fetch_fault") + PAGING + """
        ; SIB is readable at the end of one page; disp32 is on an absent page.
        ; The EA primitive rejects the span; this same decoder must preserve
        ; the original instruction boundary and destination through #PF/retry.
        mov word [0x8ffd], 0x948d
        mov byte [0x8fff], 0x24
        mov dword [0x9000], 0x12345678
        mov byte [0x9004], 0xc3
        mov dword [0x4024], 0
        invlpg [0x9000]
        mov dx, 0x3f8
        mov al, 'E'
        out dx, al                  ; renew the current paging-context lease
        mov edx, 0xdecafbad
        call 0x8ffd                 ; LEA EDX,[ESP+0x12345678]
        cmp edx, 0x12545674         ; CALL's pushed return address affects ESP
        jne fail
        cmp esp, 0x200000
        jne fail
        cmp dword [0x10000], 1
        jne fail
        jmp success
    ea_fetch_fault:
        test dword [esp], 0xf
        jnz fail
        cmp dword [esp+4], 0x8ffd
        jne fail
        cmp edx, 0xdecafbad
        jne fail
        mov eax, cr2
        cmp eax, 0x9000
        jne fail
        mov dword [0x4024], 0x9003
        invlpg [0x9000]
        inc dword [0x10000]
        add esp, 4
        iretd
    """,
    "guest-fetch-wide-lanes": wide_fetch_lanes(),
    "guest-fetch-wide-page-fault": interrupt_tables(14, "wide_fetch_fault") + PAGING + """
        ; MOV's dword immediate crosses into an absent page. The destination
        ; and flags must survive the failed fetch; restart at the original MOV.
        mov byte [0x8ffd], 0xb8
        mov dword [0x8ffe], 0x12345678
        mov byte [0x9002], 0xc3
        mov dword [0x4024], 0
        invlpg [0x9000]
        mov eax, 0xdecafbad
        push dword 0x47
        popfd
        call 0x8ffd
        pushfd
        pop edx
        and edx, 0x8d5
        cmp edx, 0x45
        jne fail
        cmp eax, 0x12345678
        jne fail
        cmp dword [0x10000], 1
        jne fail
        jmp success
    wide_fetch_fault:
        test dword [esp], 0xf  ; not-present supervisor fetch, no reserved-bit fault
        jnz fail
        cmp dword [esp+4], 0x8ffd
        jne fail
        cmp eax, 0xdecafbad
        jne fail
        mov eax, cr2
        cmp eax, 0x9000
        jne fail
        mov eax, [esp+12]
        and eax, 0x8d5
        cmp eax, 0x45
        jne fail
        mov dword [0x4024], 0x9003
        invlpg [0x9000]
        inc dword [0x10000]
        add esp, 4
        iretd
    """,
    "rmw-write-fault": interrupt_tables(14, "fault_handler") + PAGING + RMW_FAULT,
    "immediate-fetch-before-rmw": interrupt_tables(14, "fault_handler") + PAGING + IMMEDIATE_FETCH_FAULT,
    "rep-completion": """
        cld
        mov ecx, 9000
        mov edi, 0x20000
        mov eax, 0x12345678
        rep stosd
        cmp ecx, 0
        jne fail
        cmp dword [0x28c9c], 0x12345678
        jne fail
        jmp success
    """,
    "task-trap-completion": """
        ; A JMP task switch commits before the incoming TSS's T-bit #DB.
        ; The debug handler runs before the incoming task's first instruction.
        lgdt [task_gdtr]
        jmp 8:task_segments
    task_segments:
        mov ax, 16
        mov ds, ax
        mov es, ax
        mov ss, ax
        mov fs, ax
        mov gs, ax
        mov ax, 24
        ltr ax
        mov eax, task_debug
        mov word [0x6008], ax
        shr eax, 16
        mov word [0x600e], ax
        mov word [0x600a], 8
        mov word [0x600c], 0x8e00
        lidt [task_idtr]
        mov dword [0x8020], task_entry
        mov dword [0x8024], 2
        mov dword [0x8038], 0x1f0000
        mov word [0x8048], 16
        mov word [0x804c], 8
        mov word [0x8050], 16
        mov word [0x8054], 16
        mov word [0x8058], 16
        mov word [0x805c], 16
        mov word [0x8064], 1
        mov word [0x8066], 0x68
        jmp 32:0
    task_debug:
        cmp dword [esp], task_entry
        jne fail
        cmp dword [0x10000], 0
        jne fail
        inc dword [0x10000]
        iretd
    task_entry:
        cmp dword [0x10000], 1
        jne fail
        mov eax, dr6
        test eax, 0x8000
        jz fail
        jmp success
    align 8
    task_gdt:
        dq 0, 0x00cf9a000000ffff, 0x00cf92000000ffff
        dq 0x0000890070000067, 0x0000890080000067
    task_gdtr:
        dw $-task_gdt-1
        dd task_gdt
    task_idtr:
        dw 0x3ff
        dd 0x6000
    """,
})

GUEST_FIELDS = ("guest_ram_fnv64", "x86_steps", "x86_regs", "eflags", "eip", "tsc",
                "x86_completed", "x86_fault_attempts", "x86_repeat_chunks", "x86_halted_polls")
NATIVE_FIELDS = ("ram_fnv64", "retired_native", "chip_extensions",
                 "xalu_ops", "tlb_lookups", "tlb_hits", "tlb_fills", "xld_ops", "xld_hits",
                 "xea_ops", "xea_hits")

# Warm a flat proof, change DS to a based segment, cross a firmware batch
# boundary, then revoke DS entirely. Independent values and the #GP frame check
# detect stale segment proofs even when the page mapping itself remains valid.
CASES["guest-load-segment-change"] = interrupt_tables(13, "segment_fault") + """
    mov dword [0x6020], 0x0000ffff
    mov dword [0x6024], 0x00409302
    mov word [0x6200], 39
    lgdt [0x6200]
    mov dword [0x100], 0x89abcdef
    mov dword [0x20100], 0x76543210
    mov ebx, 0x100
    mov ecx, 32
.warm:
    mov esi, [ebx]
    cmp esi, 0x89abcdef
    jne fail
    mov si, [ebx]
    cmp si, 0xcdef
    jne fail
    mov dl, [ebx]
    cmp dl, 0xef
    jne fail
    dec ecx
    jnz .warm
    mov ax, 0x20
    mov ds, ax
    mov esi, [ebx]
    cmp esi, 0x76543210
    jne fail
""" + serial("S") + """
    mov esi, [ebx]
    cmp esi, 0x76543210
    jne fail
    xor eax, eax
    mov ds, ax
    mov esi, 0xdecafbad
segment_faulting:
    mov esi, [ebx]
after_segment_fault:
    cmp esi, 0xdecafbad
    jne fail
    cmp dword [0x10100], 1
    jne fail
    jmp success
segment_fault:
    cmp dword [esp], 0
    jne fail
    cmp dword [esp+4], segment_faulting
    jne fail
    mov ax, 0x18
    mov ds, ax
    inc dword [0x10100]
    mov dword [esp+4], after_segment_fault
    add esp, 4
    iretd
"""


# Execute-only, based CS must qualify instruction fetch independently of data
# read permissions. OUT forces a firmware boundary so the hardware sees that CS
# at batch entry. The final MOV's immediate crosses both its byte-granular CS
# limit and an absent page; #GP must win, retain the CS-relative instruction
# start and old EAX, and leave CR2 unchanged.
CASES["guest-fetch-nonflat-limit"] = interrupt_tables(13, "code_limit_fault") + PAGING + """
    mov dword [0x6020], 0x0fc0003f
    mov dword [0x6024], 0x00409802
    mov word [0x6200], 39
    lgdt [0x6200]
    cld
    mov esi, cs_code
    mov edi, 0x20fc0
    mov ecx, cs_code_end - cs_code
    rep movsb
    mov dword [0x4084], 0
    invlpg [0x21000]
    mov eax, 0xabcdef00
    mov cr2, eax
    jmp 0x20:0
cs_code:
    mov dx, 0x3f8
    mov al, 'C'
    out dx, al
    mov ecx, 32
.warm:
    nop
    loop .warm
    mov eax, 0xdecafbad
    times 0x3d - ($ - cs_code) db 0x90
cs_faulting:
    mov eax, 0x12345678
cs_code_end:
    jmp fail
code_limit_fault:
    cmp dword [esp], 0
    jne fail
    cmp dword [esp+4], cs_faulting - cs_code
    jne fail
    cmp dword [esp+8], 0x20
    jne fail
    cmp eax, 0xdecafbad
    jne fail
    mov eax, cr2
    cmp eax, 0xabcdef00
    jne fail
    mov dword [esp+4], code_limit_return
    mov dword [esp+8], 0x10
    add esp, 4
    iretd
code_limit_return:
    jmp success
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--software", type=Path, required=True)
    parser.add_argument("--native", type=Path, required=True)
    parser.add_argument("--out", type=Path, default=ROOT / "r86chip/experiments/out/interpreter/checks")
    parser.add_argument("--case", action="append", choices=CASES)
    parser.add_argument("--engine", choices=("iss", "rtl", "both"), default="both")
    parser.add_argument("--simulator", type=Path, default=ROOT / "target/release/r86chip-sim",
                        help="gold ISS executable, pinned in the report")
    args = parser.parse_args()
    templates = {"software": args.software.resolve(), "native": args.native.resolve()}
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    engines = ("iss", "rtl") if args.engine == "both" else (args.engine,)
    inputs = [t / name for t in templates.values() for name in ("firmware.elf", "firmware.bin")]
    inputs += [args.simulator.resolve()] if "iss" in engines else []
    inputs += [templates["native"] / "obj/Vr86_core"] if "rtl" in engines else []
    results = {"scope": __doc__, "hashes": {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in inputs},
               "cases": {}}
    for name, body in CASES.items():
        if args.case and name not in args.case:
            continue
        case = out / name
        case.mkdir(exist_ok=True)
        source = "bits 32\norg 0x1000\ncli\nmov esp, 0x200000\n" + body
        source += "success:\n" + serial("R86_CHIP_TINYCORE_OK") + "jmp $\n"
        source += "fail:\n" + serial("R86_INTERP_TEST_FAIL") + "cli\nhlt\njmp $\n"
        (case / "guest.asm").write_text(source)
        subprocess.run(["nasm", "-f", "bin", case / "guest.asm", "-o", case / "guest.bin"], check=True)
        code = (case / "guest.bin").read_bytes()
        assert len(code) < 0x10000
        reports = {}
        serial_outputs = []
        for mode, template in templates.items():
            directory = case / mode
            directory.mkdir(exist_ok=True)
            for filename in ("firmware.bin", "firmware.elf"):
                shutil.copyfile(template / filename, directory / filename)
            with (directory / "guest.ram").open("wb") as memory:
                memory.truncate(128 << 20)
                memory.seek(0x1000)
                memory.write(code)
            (directory / "config.bin").write_bytes(struct.pack("<8I", 0x5238364d, 1, 0x1000, 0x90000,
                                                               128 << 20, 50000, 0, 0))
            commands = {
                "iss": [args.simulator.resolve(), "machine", directory / "firmware.elf", directory, 500000000, 60],
                "rtl": [templates["native"] / "obj/Vr86_core", directory, 2000000000, 120],
            }
            for engine in engines:
                with (directory / f"{engine}-console.log").open("w") as output:
                    subprocess.run([str(x) for x in commands[engine]], check=True, stdout=output, stderr=output)
                report = json.loads((directory / f"{engine}-run.json").read_text())
                assert report["status"] == 3 and report["marker"], (name, mode, engine, report)
                assert report["jit_native_steps"] == 0, report
                if engine == "iss":
                    assert report["bank_enters"] == 0, report
                assert report["chip_extensions"] == int(mode == "native"), report
                if mode == "native":
                    if name not in ("cold", "all-conditions"):
                        assert report["xalu_ops"] > 0, report
                    # These guests repeat a cached data access. Other guests may
                    # legitimately exercise only cold accesses or fetches.
                    if name not in ("cold", "store-smc", "rep-completion", "guest-fetch-wide-lanes"):
                        assert report["tlb_hits"] > 0 and report["tlb_fills"] > 0, report
                    if name == "guest-fetch-wide-lanes":
                        # Warm reads use XLD directly, so XTLOOK need not hit.
                        assert report["xld_hits"] >= 16 * 12, report
                    if name == "ea-fresh-sib-pop-esp":
                        assert report["xea_hits"] >= 64, report
                    if name == "ea-cross-page-restart":
                        assert report["xea_ops"] > report["xea_hits"], report
                else:
                    assert report["xalu_ops"] == report["tlb_lookups"] == 0, report
                    assert report["xld_ops"] == report["xld_hits"] == 0, report
                    assert report["xea_ops"] == report["xea_hits"] == 0, report
                if name == "guest-load-segment-change":
                    assert report["x86_fault_attempts"] == 1, report
                    if mode == "native":
                        assert report["xld_hits"] >= 32, report
                if name in ("guest-fetch-nonflat-limit", "guest-fetch-wide-page-fault", "ea-cross-page-restart"):
                    assert report["x86_fault_attempts"] == 1, report
                if name == "rep-completion":
                    assert report["x86_repeat_chunks"] == 2, report
                    assert report["x86_steps"] == report["x86_completed"] + 2, report
                if name == "tsc-observation-order":
                    assert report["x86_fault_attempts"] == 1, report
                    assert report["x86_repeat_chunks"] == 2, report
                    assert report["x86_steps"] == report["x86_completed"] + 3, report
                if name == "task-trap-completion":
                    assert report["x86_fault_attempts"] == 0, report
                    assert report["x86_steps"] == report["x86_completed"], report
                if name in ("precise-page-fault", "cross-page-fetch", "rmw-write-fault", "cr0-wp-change", "cpl-supervisor-proof"):
                    assert report["x86_fault_attempts"] == 1, report
                if name == "immediate-fetch-before-rmw":
                    assert report["x86_fault_attempts"] == 2, report
                reports[f"{mode}-{engine}"] = report
                serial_outputs.append((directory / f"{engine}-serial.log").read_bytes())
            if len(engines) == 2:
                for field in NATIVE_FIELDS:
                    assert reports[f"{mode}-iss"][field] == reports[f"{mode}-rtl"][field], (name, mode, field)
        for field in GUEST_FIELDS:
            assert len({str(r[field]) for r in reports.values()}) == 1, (name, field, reports)
        assert len(set(serial_outputs)) == 1, (name, "serial")
        results["cases"][name] = reports
        (out / "report.json").write_text(json.dumps(results, indent=2) + "\n")
        sample = reports[f"native-{engines[-1]}"]
        print(f"PASS {name}: completed={sample['x86_completed']} xalu={sample['xalu_ops']} tlb_hits={sample['tlb_hits']}", flush=True)


if __name__ == "__main__":
    main()
