#!/usr/bin/env python3
"""Measure the current JIT's execution cost, including its Rust runtime.

Run identical firmware with JIT off/on at two completed guest-step endpoints.
Subtract total retired counts to remove common startup work; compilation,
dispatch, interpretation, devices and invalidation within the interval count.
Host-only PC profiles additionally separate emitted code (including block heads and exits) from linked firmware. Never label that smaller count
as total native/x86. TinyCore is a bounded early-boot diagnostic; the synthetic
register loop is deliberately favorable to JIT. Neither is a Windows average.
"""
import argparse
from collections import Counter
import csv
import hashlib
import json
import os
import re
from jit_diagnostics import HISTOGRAM_FLAGS, histogram, difference
from pathlib import Path
import shutil
import struct
import subprocess

from compare_machine import FIELDS
from interpreter_cost import profile_interval
from jit_checks import PAGING, ROOT
from tinycore import flatten_elf


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def executable_ranges(elf):
    """All linked executable segments; the JIT alone executes allocated RAM."""
    flatten_elf(elf)  # Use the existing firmware ELF boundary validator.
    blob = elf.read_bytes()
    phoff = struct.unpack_from("<I", blob, 28)[0]
    phsize, phnum = struct.unpack_from("<HH", blob, 42)
    ranges = []
    for i in range(phnum):
        kind, _, va, _, _, size, flags, _ = struct.unpack_from("<8I", blob, phoff + i * phsize)
        if kind == 1 and flags & 1:
            ranges.append((va, va + size))
    assert ranges
    return ranges


def register_guest(directory):
    # Fifteen useful instructions plus the loop branch fit one 16-x86 block.
    # They exercise arithmetic flags, partial-register merges and scaled LEA.
    group = "mov eax, ebx\nadd eax, edx\nmov ah, cl\nlea esi, [ebx+edx*4+4]\nxor eax, ecx\n"
    source = ("bits 32\norg 0x1000\ncli\nmov esp, 0x200000\n" + PAGING
              + "mov ebx, 3\nmov ecx, 7\nmov edx, 5\nmain_loop:\n"
              + group * 3 + "jmp main_loop\n")
    asm = directory / "guest.asm"
    asm.write_text(source)
    subprocess.run(["nasm", "-f", "bin", asm, "-o", directory / "guest.bin"], check=True)
    code = (directory / "guest.bin").read_bytes()
    assert len(code) < 4096
    with (directory / "guest.ram").open("wb") as memory:
        memory.truncate(128 << 20)
        memory.seek(0x1000)
        memory.write(code)
    return struct.pack("<8I", 0x5238364d, 1, 0x1000, 0x90000, 128 << 20, 0, 0, 0)


def attribute(directory, firmware, ranges, enters):
    """Exclusive PC counts, retaining negative endpoint deltas.

    Generated code and the four-byte service stub execute in bank 1; all other
    linked text executes in bank 0. Prefer the innermost runtime frame for
    inlined runtime work, otherwise the innermost r86 function. Debug info lets
    the same ELF expose inline frames without inserting firmware instructions.
    Out-of-line callees retain their own counts (no inferred inclusive cost).
    """
    rows = list(csv.DictReader((directory / "interval-profile.csv").open()))
    linked = [r for r in rows if any(lo <= int(r["pc"], 16) < hi for lo, hi in ranges)]
    cache_rows = list(csv.DictReader((directory / "interval-cache-profile.csv").open()))
    counts = {r["pc"]: int(r["retired_delta"]) for r in linked}
    cache_counts = {r["pc"]: [int(r[k]) for k in CACHE_FIELDS] for r in cache_rows}
    pcs = sorted(set(counts) | {pc for pc in cache_counts
                               if any(lo <= int(pc, 16) < hi for lo, hi in ranges)})
    output = subprocess.run(["llvm-symbolizer", "--output-style=JSON", "--inlines",
                             "--no-debuginfod", f"--obj={firmware}"],
                            input="".join(pc + "\n" for pc in pcs),
                            text=True, capture_output=True, check=True)
    symbols = [json.loads(line) for line in output.stdout.splitlines()]
    assert len(pcs) == len(symbols)
    functions, service, service_pcs = Counter(), 0, set()
    cache_functions = {}
    for pc, symbol in zip(pcs, symbols):
        names = [re.sub(r"::h[0-9a-f]{16}$", "", s["FunctionName"])
                 for s in symbol.get("Symbol", []) if s.get("FunctionName")]
        count = counts.get(pc, 0)
        name = next((n for n in names if n.startswith("r86::chipjit::runtime::")),
                    next((n for n in names if n.startswith("r86::")), names[0] if names else "unknown"))
        name = name.split("::_$u7b$$u7b$closure")[0].split("::{{closure}}")[0]
        dest = cache_functions.setdefault(name, [0] * 6)
        for n, fills in enumerate(cache_counts.get(pc, [0] * 6)): dest[n] += fills
        if "r86::chipjit::native::service" in names:
            service += count
            service_pcs.add(pc)
            continue
        functions[name] += count
    cache_functions["generated-code"] = [sum(f[n] for pc, f in cache_counts.items()
                                              if not any(lo <= int(pc, 16) < hi for lo, hi in ranges))
                                          for n in range(6)]
    assert len(service_pcs) <= 1, "bank-1 service must remain one XLEAVE word"
    bank0 = sum(functions.values())
    assert bank0 + service == sum(int(r["retired_delta"]) for r in linked)
    result = dict(bank0_native=bank0, bank1_service_native=service,
                  bank0_native_per_xenter=bank0 / enters if enters else None,
                  runtime_functions=[(n, c) for n, c in functions.most_common()
                                     if n.startswith("r86::chipjit::runtime::")],
                  functions=functions.most_common(), attribution=attribute.__doc__,
                  cache_fields=CACHE_FIELDS,
                  cache_functions=sorted(cache_functions.items(), key=lambda p: -sum(p[1])))
    (directory / "functions.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


CACHE_FIELDS = ("i_firmware", "i_heap", "i_guest", "d_firmware", "d_heap", "d_guest")


def cache_interval(directory, endpoints):
    profiles = []
    for steps in endpoints:
        with (directory / f"iss-{steps}-cache-profile.csv").open() as source:
            profiles.append({r["pc"]: [int(r[k]) for k in CACHE_FIELDS] for r in csv.DictReader(source)})
    a, b = profiles
    with (directory / "interval-cache-profile.csv").open("w") as output:
        writer = csv.writer(output)
        writer.writerow(("pc", *CACHE_FIELDS))
        for pc in sorted(a.keys() | b.keys()):
            delta = [y - x for x, y in zip(a.get(pc, [0] * 6), b.get(pc, [0] * 6))]
            if any(delta): writer.writerow((pc, *delta))


def measure(directory, firmware, iss, config, endpoints, jit, ranges, seconds):
    samples = []
    for steps in endpoints:
        cfg = bytearray(config)
        # Disable boundary logging in both modes. Per-PC profiling is host-only.
        struct.pack_into("<III", cfg, 20, steps, 0, HISTOGRAM_FLAGS | (2 if jit else 0))
        (directory / "config.bin").write_bytes(cfg)
        with (directory / f"iss-{steps}.log").open("w") as log:
            subprocess.run([iss, "machine", firmware, directory, "20000000000", str(seconds), "--profile", "--cache-stats"],
                           check=True, stdout=log, stderr=log)
        result = json.loads((directory / "iss-run.json").read_text())
        assert result["status"] == 4 and result["x86_steps"] == steps, result
        assert "cause: Breakpoint" in result["trap"], result
        assert result["x86_completed"] > 0, result
        assert result["bank_enters"] == result["jit_invocations"], result
        assert bool(result["jit_native_steps"]) == jit, result
        for suffix in ("run.json", "profile.csv", "cache-profile.csv", "serial.log"):
            (directory / f"iss-{suffix}").replace(directory / f"iss-{steps}-{suffix}")
        result["histogram"] = histogram(directory)
        samples.append(result)
    interval = profile_interval(directory, samples)
    cache_interval(directory, endpoints)
    delta = {key: samples[1][key] - samples[0][key] for key in (
        "retired_native", "x86_steps", "x86_completed", "jit_native_steps", "jit_invocations",
        "jit_compiled_blocks", "jit_invalidations", "x86_fault_attempts", "x86_repeat_chunks")}
    with (directory / interval).open() as stream:
        emitted = sum(int(row["retired_delta"]) for row in csv.DictReader(stream)
                      if not any(lo <= int(row["pc"], 16) < hi for lo, hi in ranges))
    assert 0 <= emitted <= delta["retired_native"]
    assert jit or emitted == 0
    completed = delta["x86_completed"]
    assert completed > 0
    translated = delta["jit_native_steps"]
    cache = {}
    for side in ("instruction", "data"):
        cache[side] = {region: samples[1]["cache_estimate"][side]["fills"][region]
                      - samples[0]["cache_estimate"][side]["fills"][region]
                      for region in ("firmware", "heap", "guest")}
    cache["line_fills"] = sum(sum(cache[side].values()) for side in ("instruction", "data"))
    cache["estimated_clocks"] = 1.15 * delta["retired_native"] + 45 * cache["line_fills"]
    costs = attribute(directory, firmware, ranges, delta["jit_invocations"])
    assert sum(sum(f) for _, f in costs["cache_functions"]) == cache["line_fills"]
    hist = difference(samples[0]["histogram"], samples[1]["histogram"])
    assert costs["bank1_service_native"] == hist["leaves"]["service"]
    report = dict(samples=samples, delta=delta, interval_profile=interval,
                  histogram=hist,
                  cache_estimate=cache,
                  estimated_clocks_per_x86=cache["estimated_clocks"] / completed,
                  bank0_native=costs["bank0_native"],
                  bank0_native_per_xenter=costs["bank0_native_per_xenter"],
                  bank1_service_native=costs["bank1_service_native"],
                  runtime_top5=costs["runtime_functions"][:5],
                  cache_top5=costs["cache_functions"][:5],
                  native_per_completed_x86=delta["retired_native"] / completed,
                  emitted_native=emitted, firmware_native=delta["retired_native"] - emitted,
                  jit_coverage=translated / completed,
                  emitted_native_per_translated_x86=emitted / translated if translated else None,
                  translated_x86_per_invocation=translated / delta["jit_invocations"] if translated else None)
    print(f"{directory.parent.name}/{directory.name}: {report['native_per_completed_x86']:.2f} "
          f"total native/completed x86; JIT coverage {report['jit_coverage']:.2%}; "
          f"emitted native/translated x86 {report['emitted_native_per_translated_x86']}", flush=True)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--template", required=True, type=Path, help="current tinycore.py --jit output")
    parser.add_argument("--out", type=Path, default=ROOT / "r86chip/experiments/out/jit-cost")
    parser.add_argument("--simulator", type=Path, default=ROOT / os.environ.get("CARGO_TARGET_DIR", "target") / "release/r86chip-sim")
    parser.add_argument("--case", action="append", choices=("tinycore", "register-loop"))
    parser.add_argument("--start", type=int, default=1000000, help="first TinyCore endpoint")
    parser.add_argument("--end", type=int, default=2000000, help="second TinyCore endpoint")
    parser.add_argument("--seconds", type=int, default=120)
    args = parser.parse_args()
    if not 0 < args.start < args.end < 1 << 32 or args.seconds <= 0:
        parser.error("require 0 < start < end < 2**32 and positive seconds")
    template, out, iss = args.template.resolve(), args.out.resolve(), args.simulator.resolve()
    out.mkdir(parents=True, exist_ok=True)
    firmware = out / "firmware.elf"
    shutil.copyfile(template / "firmware.elf", firmware)
    (out / "firmware.objdump").write_text(subprocess.check_output(
        ["llvm-objdump", "-d", "--demangle", "--mattr=+m,+zba", firmware], text=True))
    ranges = executable_ranges(firmware)
    report = dict(scope=__doc__, hashes={str(p): sha(p) for p in (firmware, iss, Path(__file__))},
                  executable_segments=ranges, cases={})
    for name in args.case or ("tinycore", "register-loop"):
        case = out / name
        case.mkdir(exist_ok=True)
        if name == "tinycore":
            (case / "guest.ram").unlink(missing_ok=True)
            (case / "guest.ram").hardlink_to(template / "guest.ram")
            config = (template / "config.bin").read_bytes()
            endpoints = (args.start, args.end)
        else:
            config = register_guest(case)
            endpoints = (10000, 10000 + 10000 * 16)
        modes = {}
        for jit in (False, True):
            directory = case / ("jit" if jit else "interp")
            directory.mkdir(exist_ok=True)
            (directory / "guest.ram").unlink(missing_ok=True)
            (directory / "guest.ram").hardlink_to(case / "guest.ram")
            modes[directory.name] = measure(directory, firmware, iss, config, endpoints, jit, ranges, args.seconds)
            (directory / "guest.ram").unlink()
        for index, steps in enumerate(endpoints):
            for key in (*FIELDS, "x86_completed", "x86_fault_attempts", "x86_repeat_chunks"):
                assert modes["interp"]["samples"][index][key] == modes["jit"]["samples"][index][key], (name, steps, key)
            serial = f"iss-{steps}-serial.log"
            assert (case / "interp" / serial).read_bytes() == (case / "jit" / serial).read_bytes()
        report["cases"][name] = dict(endpoints=endpoints, modes=modes, guest_comparison_passed=True,
                                    guest_sha256=sha(case / "guest.ram"))
        (case / "guest.ram").unlink()
        (out / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print("PASS: both endpoints agree in guest state, RAM, time, counts and serial", flush=True)


if __name__ == "__main__":
    main()
