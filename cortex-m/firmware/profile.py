#!/usr/bin/env python3
"""Flat profile of an r86-cortex-m run: profile.py FIRMWARE.elf profile.bin

profile.bin holds one sampled program counter per word. Prints the share of
samples per group and per function. Translated code has no symbols; its
samples are those inside the CODE region of abi.rs.
"""
import bisect, collections, re, struct, subprocess, sys

elf, samples = sys.argv[1], open(sys.argv[2], "rb").read()
CODE = (0x6600_0000, 0x6680_0000)
symbols = []
for line in subprocess.run(["llvm-nm", "-n", "-C", "--defined-only", elf],
                           capture_output=True, text=True, check=True).stdout.splitlines():
    address, kind, name = line.split(" ", 2)
    if kind in "tTwW":
        symbols.append((int(address, 16) & ~1, re.sub(r"::h[0-9a-f]{16}$", "", name)))
starts = [address for address, _ in symbols]

def group(name):
    for prefix, label in (
        ("r86::x86::jit::t32::lower", "JIT: compile"), ("r86::x86::jit::t32::asm", "JIT: compile"),
        ("r86::x86::jit::decode", "JIT: compile"), ("r86::x86::jit::compile", "JIT: compile"),
        ("r86::x86::jit::t32", "JIT: runtime"), ("r86::x86::jit", "JIT: runtime"),
        ("r86::x86::interp", "interpreter"), ("r86::x86::mmu", "paging and memory"),
        ("r86::x86::mem", "paging and memory"), ("r86::x86", "CPU, other"),
        ("r86::machine", "machine and bus"), ("r86::devices", "devices"), ("r86::", "r86, other"),
        ("compiler_builtins", "64-bit and float helpers"), ("__aeabi", "64-bit and float helpers"),
        ("mem", "memcpy/memset"), ("dlmalloc", "allocator"), ("hashbrown", "hash map"),
        ("core::", "core library"), ("alloc::", "alloc library"),
    ):
        if name.startswith(prefix) or name.startswith("<" + prefix):
            return label
    return "other"

functions, groups = collections.Counter(), collections.Counter()
count = len(samples) // 4
for (pc,) in struct.iter_unpack("<I", samples[:4 * count]):
    if CODE[0] <= pc < CODE[1]:
        name, label = "<translated code>", "translated code"
    else:
        index = bisect.bisect_right(starts, pc) - 1
        name = symbols[index][1] if index >= 0 else hex(pc)
        label = group(name)
    functions[name] += 1
    groups[label] += 1
print(f"{count} samples")
for label, n in groups.most_common():
    print(f"{100 * n / count:6.2f}%  {label}")
print()
for name, n in functions.most_common(int(sys.argv[3]) if len(sys.argv) > 3 else 30):
    print(f"{100 * n / count:6.2f}%  {name[:110]}")
