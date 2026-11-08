#!/usr/bin/env python3
"""Measure the current JIT's execution cost, including its Rust runtime.

Run identical firmware with JIT off/on at two completed guest-step endpoints.
Subtract total retired counts to remove common startup work; compilation,
dispatch, interpretation, devices and invalidation within the interval count.
Host-only PC profiles additionally separate emitted code (including its frame
loads/stores and return) from linked firmware. Never label that smaller count
as total native/x86. TinyCore is a bounded early-boot diagnostic; the synthetic
register loop is deliberately favorable to JIT. Neither is a Windows average.
"""
import argparse
import csv
import hashlib
import json
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


def measure(directory, firmware, iss, config, endpoints, jit, ranges, seconds):
    samples = []
    for steps in endpoints:
        cfg = bytearray(config)
        # Disable boundary logging in both modes. Per-PC profiling is host-only.
        struct.pack_into("<III", cfg, 20, steps, 0, 2 if jit else 0)
        (directory / "config.bin").write_bytes(cfg)
        with (directory / f"iss-{steps}.log").open("w") as log:
            subprocess.run([iss, "machine", firmware, directory, "20000000000", str(seconds), "--profile"],
                           check=True, stdout=log, stderr=log)
        result = json.loads((directory / "iss-run.json").read_text())
        assert result["status"] == 4 and result["x86_steps"] == steps, result
        assert "cause: Breakpoint" in result["trap"], result
        assert result["x86_completed"] > 0, result
        assert result["bank_enters"] == result["jit_invocations"], result
        assert bool(result["jit_native_steps"]) == jit, result
        for suffix in ("run.json", "profile.csv", "serial.log"):
            (directory / f"iss-{suffix}").replace(directory / f"iss-{steps}-{suffix}")
        samples.append(result)
    interval = profile_interval(directory, samples)
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
    report = dict(samples=samples, delta=delta, interval_profile=interval,
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
    parser.add_argument("--simulator", type=Path, default=ROOT / "target/release/r86chip-sim")
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
            shutil.copyfile(template / "guest.ram", case / "guest.ram")
            config = (template / "config.bin").read_bytes()
            endpoints = (args.start, args.end)
        else:
            config = register_guest(case)
            endpoints = (10000, 10000 + 10000 * 16)
        modes = {}
        for jit in (False, True):
            directory = case / ("jit" if jit else "interp")
            directory.mkdir(exist_ok=True)
            shutil.copyfile(case / "guest.ram", directory / "guest.ram")
            modes[directory.name] = measure(directory, firmware, iss, config, endpoints, jit, ranges, args.seconds)
        for index, steps in enumerate(endpoints):
            for key in (*FIELDS, "x86_completed", "x86_fault_attempts", "x86_repeat_chunks"):
                assert modes["interp"]["samples"][index][key] == modes["jit"]["samples"][index][key], (name, steps, key)
            serial = f"iss-{steps}-serial.log"
            assert (case / "interp" / serial).read_bytes() == (case / "jit" / serial).read_bytes()
        report["cases"][name] = dict(endpoints=endpoints, modes=modes, guest_comparison_passed=True,
                                    guest_sha256=sha(case / "guest.ram"))
        (out / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print("PASS: both endpoints agree in guest state, RAM, time, counts and serial", flush=True)


if __name__ == "__main__":
    main()
