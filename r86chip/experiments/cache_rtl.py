#!/usr/bin/env python3
"""Run native fixtures on core + caches + a slow burst RAM; compare with the ISS.

Builds `r86_system` (Cache.scala) with rtl/tests/system_tb.sv and runs the
xlate_rtl.py fixtures and benchmarks plus rtl.py's programs (those whose RAM
is a whole number of cache lines and that need no injected fetch error). Each
run must end with the ISS's trap, retired count, written registers and RAM;
the RAM is the back-end model's content after the cache wrote its dirty lines
back. Three set bits (16 lines of 32 bytes per cache, two ways) force
evictions and write-backs in these small programs; ten is 64 KiB per cache.

Usage: cache_rtl.py --out DIR --simulator r86chip-sim
"""
import argparse
import json
from pathlib import Path
import subprocess

import rtl
import xlate_rtl

HERE = Path(__file__).resolve().parent
RTL = HERE.parent / "rtl"


def build(out, set_bits):
    directory = out / f"sys{set_bits}"
    subprocess.run(["sbt", "-batch", f"runMain r86chip.GenerateSystem 2 {set_bits} {set_bits} {directory}"],
                   cwd=RTL, check=True, capture_output=True)
    sources = sorted(directory.glob("*.sv"))
    subprocess.run(["verilator", "--binary", "--timing", "--top-module", "system_tb", "-Wno-TIMESCALEMOD",
                    "-Wno-WIDTH", "-j", "2", "--Mdir", directory / "obj", *sources,
                    RTL / "r86_ram.v", RTL / "tests/system_tb.sv"],
                   check=True, capture_output=True)
    return directory / "obj/Vsystem_tb"


def run(executable, path, gold, ram, latency, gaps, seed):
    log = subprocess.run([str(executable), f"+IMAGE={path.with_suffix('.hex')}", f"+SEED={seed}",
                          f"+LATENCY={latency}", f"+GAPS={gaps}", f"+RAM_BYTES={len(ram)}"],
                         text=True, capture_output=True)
    label = f"{path.name} latency={latency} gaps={gaps}"
    assert log.returncode == 0, (label, log.stdout[-1500:], log.stderr[-500:])
    registers = [[None] * 32 for _ in range(2)]
    retired, trap, memory, count = 0, None, None, None
    for line in log.stdout.splitlines():
        parts = line.split()
        if not parts:
            continue
        if parts[0] == "R":
            retired += 1
            if int(parts[4], 16):
                rd = int(parts[5], 16)
                registers[rd // 32][rd % 32] = int(parts[6], 16)
        elif parts[0] == "T":
            trap = tuple(int(x, 16) for x in parts[1:4])
        elif parts[0] == "M":
            memory = parts[1]
        elif parts[0] == "C":
            count = tuple(int(x) for x in parts[1:4])
    assert trap == (gold["pc"], gold["cause"], gold["value"]), (label, trap, gold["pc"], gold["cause"], gold["value"])
    assert retired == gold["retired_native"], (label, retired, gold["retired_native"])
    for bank in range(2):
        for reg, value in enumerate(registers[bank]):
            if value is not None:
                assert value == gold["registers"][bank][reg], (label, bank, reg, hex(value))
    assert memory == gold["ram_fnv64"], (label, "RAM after sync")
    return count


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--simulator", type=Path, required=True)
    parser.add_argument("--only")
    args = parser.parse_args()
    out = args.out.resolve()
    images = out / "images"
    images.mkdir(parents=True, exist_ok=True)
    fixtures = list(xlate_rtl.cases())
    fixtures += [(name, image) for name, image, fault_pc, _ in rtl.cases()
                 if fault_pc < 0 and len(image) % 32 == 0]
    benches = [(f"bench-{name}-{n}", build_image(n)) for name, _, build_image in xlate_rtl.benchmarks()
               for n in (100, 300)]
    golds = {}
    for name, ram in fixtures + benches:
        path = images / name
        path.with_suffix(".bin").write_bytes(ram)
        path.with_suffix(".hex").write_text("\n".join(f"{x:02x}" for x in ram) + "\n")
        golds[name] = json.loads(subprocess.run(
            [str(args.simulator), "native", str(path.with_suffix(".bin"))],
            text=True, capture_output=True, check=True).stdout)
    runs = 0
    small, full = build(out, 3), build(out, 10)
    for name, ram in fixtures + benches:
        if args.only and args.only not in name:
            continue
        for executable, latency, gaps, seed in ((small, 0, 0, 1), (small, 5, 1, 2), (full, 9, 1, 3)):
            run(executable, images / name, golds[name], ram, latency, gaps, seed)
            runs += 1
    print(f"PASS: {len(fixtures) + len(benches)} images, {runs} runs equal the ISS, including RAM after write-back")
    # Cost of generated code with the intended cache size and a 24-clock RAM.
    for name, source, _ in xlate_rtl.benchmarks():
        points = []
        for n in (100, 300):
            cycles, retired, requests = run(full, images / f"bench-{name}-{n}", golds[f"bench-{name}-{n}"],
                                            b"\0" * xlate_rtl.SIZE, 24, 0, 1)
            points.append((xlate_rtl.BUDGET - golds[f"bench-{name}-{n}"]["registers"][0][20], cycles, requests))
        x86, clocks, requests = (points[1][k] - points[0][k] for k in range(3))
        print(f"bench {name}: {clocks / x86:.2f} clocks per x86 instruction with caches "
              f"({requests} RAM requests in the measured part)")


if __name__ == "__main__":
    main()
