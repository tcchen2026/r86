#!/usr/bin/env python3
"""Exercise same-core JIT validity, fault boundaries and timer interrupts.

Use a --jit build of tinycore.py as --template. The same RV firmware is run with
JIT disabled/enabled under the selected ISS/RTL engines. These are synthetic protected-mode
guests, distinct from the TinyCore boot gate; each guest checks its own results.
"""
import argparse
import json
from pathlib import Path
import os
import shutil
import struct
import subprocess

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "r86chip/experiments/out/jit-checks"
PAGING = """
    cld
    mov edi, 0x4000
    mov eax, 3
    mov ecx, 1024
.page:
    stosd
    add eax, 4096
    loop .page
    mov dword [0x3000], 0x4003
    mov dword [0x3004], 0x5003
    mov dword [0x5004], 0x1003
    mov eax, 0x3000
    mov cr3, eax
    mov eax, cr0
    or eax, 0x80000000
    mov cr0, eax
"""
SMC = """
    mov ecx, 32
.warm:
    call target
    cmp eax, 1
    jne fail
    dec ecx
    jnz .warm
    mov byte [target + ALIAS + 1], 7
    mov ecx, 32
.changed:
    call target
    cmp eax, 7
    jne fail
    dec ecx
    jnz .changed
    jmp success
target:
    mov eax, 1
    ret
"""
REMAP = """
    mov byte [0x8000], 0xb8
    mov dword [0x8001], 1
    mov byte [0x8005], 0xc3
    mov byte [0x9000], 0xb8
    mov dword [0x9001], 2
    mov byte [0x9005], 0xc3
    mov ecx, 32
.warm:
    call 0x8000
    cmp eax, 1
    jne fail
    dec ecx
    jnz .warm
    mov dword [0x4020], 0x9003
    invlpg [0x8000]
    mov ecx, 32
.new_mapping:
    call 0x8000
    cmp eax, 2
    jne fail
    dec ecx
    jnz .new_mapping
    jmp success
"""
NEGATIVE = """
    ; MUL stays interpreted; replacing it must retire a negatively cached entry.
    mov word [0x8000], 0xe0f7
    mov byte [0x8002], 0xc3
    mov ecx, 32
.warm:
    mov eax, 7
    call 0x8000
    cmp eax, 49
    jne fail
    dec ecx
    jnz .warm
    mov word [0x8000], 0x9090
    mov ecx, 32
.changed:
    mov eax, 7
    call 0x8000
    cmp eax, 7
    jne fail
    dec ecx
    jnz .changed
    jmp success
"""

IMMEDIATE_BORROW = """
    mov eax, 0x1234ffff
    mov ecx, 32
.warm:
    call target
    jc fail
    jnz fail
    cmp eax, 0x1234ffff
    jne fail
    dec ecx
    jnz .warm
    jmp success
target:
    cmp ax, byte -1
    ret
"""


def interrupt_tables(vector, handler):
    # Match enter_protected_flat's cached CS=0x10 and SS=0x18. Install actual
    # descriptors so exception delivery and IRET exercise shared x86 semantics.
    return f"""
    mov dword [0x6010], 0x0000ffff
    mov dword [0x6014], 0x00cf9b00
    mov dword [0x6018], 0x0000ffff
    mov dword [0x601c], 0x00cf9300
    mov word [0x6200], 31
    mov dword [0x6202], 0x6000
    lgdt [0x6200]
    mov word [0x7000 + {vector}*8], {handler}
    mov word [0x7002 + {vector}*8], 0x10
    mov word [0x7004 + {vector}*8], 0x8e00
    mov word [0x7006 + {vector}*8], 0
    mov word [0x6208], 2047
    mov dword [0x620a], 0x7000
    lidt [0x6208]
"""


FAULT = """
    mov dword [0x5800], 0xa003
    mov dword [0xa000], 0x11223344
    mov ecx, 32
.warm:
    call target
    cmp ebx, 0x11223344
    jne fail
    dec ecx
    jnz .warm
    mov dword [0x5800], 0
    invlpg [0x600000]
    mov ebx, 0xdecafbad
    call target
    cmp dword [0x10000], 1
    jne fail
    cmp eax, 0x80000000
    jne fail
    cmp ebx, 0x11223344
    jne fail
    jmp success
target:
    mov eax, 0x7fffffff
    add eax, 1
faulting:
    mov ebx, [0x600000]
    ret
fault_handler:
    cmp eax, 0x80000000
    jne fail
    cmp ebx, 0xdecafbad
    jne fail
    cmp dword [esp], 0
    jne fail
    cmp dword [esp+4], faulting
    jne fail
    push eax
    mov eax, cr2
    cmp eax, 0x600000
    jne fail
    mov eax, [esp+16]
    and eax, 0x8d5
    cmp eax, 0x894
    jne fail
    pop eax
    inc dword [0x10000]
    mov dword [0x5800], 0xa003
    invlpg [0x600000]
    add esp, 4
    iretd
"""
FETCH_FAULT = """
    ; INC EAX fits the first page; MOV EDX,imm32 crosses into the next.
    mov word [0x8ffc], 0xba40
    mov dword [0x8ffe], 0x12345678
    mov byte [0x9002], 0xc3
    mov ecx, 32
.warm:
    mov eax, 0x7fffffff
    call 0x8ffc
    cmp edx, 0x12345678
    jne fail
    dec ecx
    jnz .warm
    mov dword [0x4024], 0
    invlpg [0x9000]
    mov eax, 0x7fffffff
    mov edx, 0xdecafbad
    stc
    call 0x8ffc
    cmp dword [0x10000], 1
    jne fail
    cmp eax, 0x80000000
    jne fail
    cmp edx, 0x12345678
    jne fail
    jmp success
fault_handler:
    cmp eax, 0x80000000
    jne fail
    cmp edx, 0xdecafbad
    jne fail
    ; Assert non-present, supervisor, non-write, non-reserved fault bits.
    ; The shared model also reports its instruction-fetch classification.
    test dword [esp], 0xf
    jne fail
    cmp dword [esp+4], 0x8ffd
    jne fail
    push eax
    mov eax, cr2
    cmp eax, 0x9000
    jne fail
    mov eax, [esp+16]
    and eax, 0x8d5
    cmp eax, 0x895
    jne fail
    pop eax
    inc dword [0x10000]
    mov dword [0x4024], 0x9003
    invlpg [0x9000]
    add esp, 4
    iretd
"""
TIMER = """
    mov al, 0x11
    out 0x20, al
    mov al, 0x20
    out 0x21, al
    mov al, 4
    out 0x21, al
    mov al, 1
    out 0x21, al
    mov al, 0xfe
    out 0x21, al
    mov al, 0xff
    out 0xa1, al
    mov al, 0x34
    out 0x43, al
    mov al, 16
    out 0x40, al
    mov al, 0
    out 0x40, al
    ; Warm while IF=0 until IRQ0 is pending. STI must inhibit delivery for
    ; exactly the following instruction, even when the next entry is hot.
    mov ecx, 64
    mov edi, 0
    mov ebx, 0
.warm:
    add edi, 1
    add edi, 2
    sub edi, 1
    dec ecx
    jnz .warm
    sti
    mov ebx, 0x12345678
irq_start:
    mov eax, 0
    mov ecx, 2048
.loop:
    add eax, 1
    add eax, 2
    sub eax, 1
    dec ecx
    jnz .loop
    cli
    cmp eax, 4096
    jne fail
    cmp dword [0x10000], 4
    jb fail
    jmp success
irq_handler:
    pushad
    cmp dword [0x10000], 0
    jne .subsequent
    cmp ebx, 0x12345678
    jne fail
    cmp dword [esp+32], irq_start
    jne fail
.subsequent:
    inc dword [0x10000]
    mov al, 0x20
    out 0x20, al
    popad
    iretd
"""
CASES = {
    "cold": "times 64 nop\njmp success\n",
    # Preserve CMP's flags through aliased byte/word moves and all Zba scales.
    # Repeated entries ensure these execute in generated blocks after warming.
    "lanes-scaled-branches": """
    mov ecx, 96
.loop:
    mov eax, 0xa1b2c3d4
    mov ebx, 0x10203040
    mov esi, 0x80000001
    cmp esi, 0x80000002
    mov ah, al
    mov al, ah
    mov bl, al
    mov ax, bx
    lea edx, [esi+esi*2]
    lea edi, [esi+esi*4]
    lea esi, [esi+esi*8-8]
    jnc fail
    jz fail
    jo fail
    jnp fail
    jns fail
    cmp eax, 0xa1b230d4
    jne fail
    cmp edx, 0x80000003
    jne fail
    cmp edi, 0x80000005
    jne fail
    cmp esi, 0x80000001
    jne fail
    dec ecx
    jnz .loop
    jmp success
""",
    "smc": "%define ALIAS 0\n" + SMC,
    "physical-alias": "%define ALIAS 0x400000\n" + PAGING + SMC,
    "invlpg-remap": PAGING + REMAP,
    "negative-cache": NEGATIVE,
    "immediate-borrow": IMMEDIATE_BORROW,
    "precise-page-fault": interrupt_tables(14, "fault_handler") + PAGING + FAULT,
    "cross-page-fetch": interrupt_tables(14, "fault_handler") + PAGING + FETCH_FAULT,
    "timer-boundary": interrupt_tables(32, "irq_handler") + TIMER,
}


# Cross-page XJT entries must expire on both explicit and physical remapping.
CASES["cr3-remap"] = PAGING + REMAP.replace("invlpg [0x8000]", "mov eax, cr3\n    mov cr3, eax")
# Both MUL and operand-size RET are negative entries on this page. Unlike a
# native RET, they never install a jump proof there. INVLPG still expires them.
CASES["negative-invlpg"] = PAGING + """
    mov word [0x8000], 0xe0f7
    mov word [0x8002], 0xc366
    mov byte [0x9000], 0xb8
    mov dword [0x9001], 2
    mov word [0x9005], 0xc366
    mov ecx, 32
.warm:
    mov eax, 7
    call 0x8000
    add esp, 2
    cmp eax, 49
    jne fail
    dec ecx
    jnz .warm
    mov dword [0x4020], 0x9003
    invlpg [0x8000]
    mov ecx, 32
.remapped:
    call 0x8000
    add esp, 2
    cmp eax, 2
    jne fail
    dec ecx
    jnz .remapped
    jmp success
"""
CASES["a20-remap"] = """
    mov byte [0x8000], 0xb8
    mov dword [0x8001], 1
    mov byte [0x8005], 0xc3
    mov byte [0x108000], 0xb8
    mov dword [0x108001], 2
    mov byte [0x108005], 0xc3
    mov ecx, 32
.warm:
    call 0x108000
    cmp eax, 2
    jne fail
    dec ecx
    jnz .warm
    xor eax, eax
    out 0x92, al
    mov ecx, 32
.alias:
    call 0x108000
    cmp eax, 1
    jne fail
    dec ecx
    jnz .alias
    mov al, 2
    out 0x92, al
    jmp success
"""


# Runtime-specific service boundaries added with resident memory/stack lowering.
CASES["split-stack-call"] = """
    mov ecx, 64
.loop:
    mov ebp, esp
    mov esp, 0x200002
    mov eax, 0x89abcdef
    call target
    cmp esp, 0x200002
    jne fail
    cmp edx, 0x89abcdef
    jne fail
    mov esp, ebp
    dec ecx
    jnz .loop
    jmp success
target:
    push eax
    pop edx
    ret
"""
CASES["based-fs-reload"] = """
    mov dword [0x6020], 0x000000ff
    mov dword [0x6024], 0x00409301
    mov word [0x6200], 39
    mov dword [0x6202], 0x6000
    lgdt [0x6200]
    mov dword [0x10080], 0x12345678
    mov dword [0x20080], 0x89abcdef
    mov ax, 0x20
    mov fs, ax
    mov ecx, 32
.first:
    call target
    cmp edx, 0x12345678
    jne fail
    dec ecx
    jnz .first
    mov dword [0x6024], 0x00409302
    mov fs, ax
    mov ecx, 32
.second:
    call target
    cmp edx, 0x89abcdef
    jne fail
    dec ecx
    jnz .second
    jmp success
target:
    mov edx, [fs:0x80]
    ret
"""
CASES["rmw-write-fault"] = interrupt_tables(14, "fault_handler") + PAGING + """
    mov dword [0x5800], 0xa003
    mov dword [0xa000], 7
    mov eax, cr0
    or eax, 0x10000
    mov cr0, eax
    mov ecx, 32
.warm:
    call target
    dec ecx
    jnz .warm
    mov dword [0x5800], 0xa001
    invlpg [0x600000]
    call target
    cmp dword [0x10000], 1
    jne fail
    cmp dword [0xa000], 40
    jne fail
    jmp success
target:
    mov eax, 0x7fffffff
    stc
    inc eax
faulting:
    add dword [0x600000], byte 1
    ret
fault_handler:
    cmp eax, 0x80000000
    jne fail
    cmp dword [esp], 3
    jne fail
    cmp dword [esp+4], faulting
    jne fail
    cmp dword [0xa000], 39
    jne fail
    push eax
    mov eax, [esp+16]
    and eax, 0x8d5
    cmp eax, 0x895
    jne fail
    pop eax
    inc dword [0x10000]
    mov dword [0x5800], 0xa003
    invlpg [0x600000]
    add esp, 4
    iretd
"""


# Three same-page entry addresses collide in the two-probe EIP index. Each
# target is warmed before the next displacement; stale direct links must never
# call freed/reused native storage. Revisit all three after warming.
CASES["link-collision"] = """
    mov dword [0x2000], 0xc301c083
    mov dword [0x2444], 0xc302c083
    mov dword [0x2888], 0xc303c083
    xor eax, eax
    mov edx, 8
.round:
    mov ecx, 32
.first:
    call 0x2000
    dec ecx
    jnz .first
    mov ecx, 32
.second:
    call 0x2444
    dec ecx
    jnz .second
    mov ecx, 32
.third:
    call 0x2888
    dec ecx
    jnz .third
    dec edx
    jnz .round
    cmp eax, 1536
    jne fail
    jmp success
"""


def serial(text):
    # Unroll the observation code so the cold case cannot become hot merely
    # because a serial print loop revisits the same guest PC.
    return "mov dx, 0x3f8\n" + "".join(f"mov al, {byte}\nout dx, al\n" for byte in text.encode())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--template", type=Path, default=OUT.parent / "tinycore-jit-checks-template")
    parser.add_argument("--out", type=Path, default=OUT)
    parser.add_argument("--case", action="append", choices=CASES)
    parser.add_argument("--engine", action="append", choices=("iss", "rtl"), default=None)
    parser.add_argument("--simulator", type=Path, default=ROOT / os.environ.get("CARGO_TARGET_DIR", "target") / "release/r86chip-sim")
    args = parser.parse_args()
    template = args.template.resolve()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    engines = args.engine or ["iss", "rtl"]
    results = {}
    for name, body in CASES.items():
        if args.case and name not in args.case:
            continue
        case = out / name
        case.mkdir(exist_ok=True)
        source = "bits 32\norg 0x1000\ncli\nmov esp, 0x200000\n" + body
        source += "success:\n" + serial("R86_CHIP_TINYCORE_OK") + "jmp $\n"
        source += "fail:\n" + serial("R86_B1_TEST_FAIL") + "cli\nhlt\njmp $\n"
        (case / "guest.asm").write_text(source)
        subprocess.run(["nasm", "-f", "bin", str(case / "guest.asm"), "-o", str(case / "guest.bin")], check=True)
        code = (case / "guest.bin").read_bytes()
        assert len(code) < 4096
        guest = case / "guest.ram"
        with guest.open("wb") as memory:
            memory.truncate(128 << 20)
            memory.seek(0x1000)
            memory.write(code)
        reports = {}
        for jit in (False, True):
            directory = case / ("jit" if jit else "interp")
            directory.mkdir(exist_ok=True)
            for filename in ("firmware.bin", "firmware.elf"):
                shutil.copyfile(template / filename, directory / filename)
            target = directory / "guest.ram"
            target.unlink(missing_ok=True)
            target.hardlink_to(guest)
            (directory / "config.bin").write_bytes(struct.pack("<8I", 0x5238364d, 1, 0x1000, 0x90000,
                                                                   128 << 20, 50000, 0, 2 if jit else 0))
            commands = {
                "iss": [args.simulator.resolve(), "machine", directory / "firmware.elf", directory, 500000000, 60],
                "rtl": [template / "obj/Vr86_core", directory, 1000000000, 120],
            }
            for engine in engines:
                command = commands[engine]
                with (directory / f"{engine}-console.log").open("w") as output:
                    subprocess.run([str(x) for x in command], check=True, stdout=output, stderr=output)
                report = json.loads((directory / f"{engine}-run.json").read_text())
                assert report["status"] == 3 and report["marker"], (name, jit, engine, report)
                if name == "cold":
                    assert report["jit_native_steps"] == 0, report
                elif jit:
                    assert report["jit_native_steps"] > 100 and report["jit_compiled_blocks"] > 0, report
                    if name in ("smc", "physical-alias", "negative-cache"):
                        assert report["jit_invalidations"] > 0, report
                reports[(jit, engine)] = report
            if "iss" in engines and "rtl" in engines:
                for field in ("ram_fnv64", "guest_ram_fnv64", "jit_native_steps", "jit_invocations"):
                    assert reports[(jit, "iss")][field] == reports[(jit, "rtl")][field], (name, jit, field)
                assert reports[(jit, "iss")]["rv_instructions"] == reports[(jit, "rtl")]["rv_instructions"] + 1
            (directory / "guest.ram").unlink()
        for field in ("guest_ram_fnv64", "x86_steps", "x86_regs", "eflags", "eip", "tsc"):
            assert len({str(r[field]) for r in reports.values()}) == 1, (name, field, reports)
        results[name] = {f"{'jit' if j else 'interp'}-{e}": r for (j, e), r in reports.items()}
        sample = reports[(True, engines[0])]
        print(f"PASS {name}: x86={sample['x86_steps']} native={sample['jit_native_steps']}", flush=True)
        guest.unlink()
        (out / "report.json").write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
