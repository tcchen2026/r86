#!/usr/bin/env python3
"""Where the samples inside one function go: inline.py FIRMWARE.elf profile.bin NAME [ROWS]

Needs a firmware built with CARGO_PROFILE_RELEASE_DEBUG=line-tables-only.
Takes the samples whose symbol contains NAME and attributes each to the
innermost inlined function and to its source line.
"""
import bisect, collections, re, struct, subprocess, sys

elf, samples, wanted = sys.argv[1], open(sys.argv[2], "rb").read(), sys.argv[3]
rows = int(sys.argv[4]) if len(sys.argv) > 4 else 25
symbols = []
for line in subprocess.run(["llvm-nm", "-n", "-C", "--defined-only", elf],
                           capture_output=True, text=True, check=True).stdout.splitlines():
    address, kind, name = line.split(" ", 2)
    if kind in "tTwW":
        symbols.append((int(address, 16) & ~1, name))
starts = [address for address, _ in symbols]
total = len(samples) // 4
hits = collections.Counter()
for (pc,) in struct.iter_unpack("<I", samples[:4 * total]):
    index = bisect.bisect_right(starts, pc) - 1
    if index >= 0 and wanted in symbols[index][1]:
        hits[pc] += 1
inside = sum(hits.values())
out = subprocess.run(["llvm-addr2line", "-e", elf, "-f", "-C", "-i", "-a"],
                     input="\n".join(hex(pc) for pc in hits), capture_output=True, text=True, check=True).stdout
functions, lines = collections.Counter(), collections.Counter()
for block in re.split(r"^(?=0x)", out, flags=re.M):
    parts = block.strip().splitlines()
    if len(parts) < 3:
        continue
    count = hits[int(parts[0], 16)]
    name = re.sub(r"::h[0-9a-f]{16}$", "", parts[1])
    where = re.sub(r"^.*/(src|r86chip|cortex-m)/", r"\1/", parts[2]).split(" ")[0]
    functions[name] += count
    lines[(name, where)] += count
print(f"{inside} of {total} samples ({100 * inside / total:.1f}%) are inside *{wanted}*; shares below are of all samples")
for name, n in functions.most_common(rows):
    print(f"{100 * n / total:6.2f}%  {name[:120]}")
print()
for (name, where), n in lines.most_common(rows):
    print(f"{100 * n / total:6.2f}%  {where:44s} {name[-60:]}")
