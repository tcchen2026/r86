#!/usr/bin/env python3
"""Measure retired RV instructions per completed x86 instruction, without JIT.

Each protected-mode guest enables paging, then repeats 32 copies of a selected
instruction group and one jump. Endpoints differing by whole iterations cancel common
firmware/device/page-table setup. The result includes the steady execution loop,
software decoding, memory checks and scheduled runtime work. It is a synthetic
cost diagnosis, not a Windows workload average, hardware CPI or a boot result.
--profile also records host-only per-PC retirement differences for that exact
interval. Both endpoint profiles are retained, including their startup work.
"""
import argparse
import csv
import hashlib
import json
from pathlib import Path
import struct
import subprocess

from jit_checks import PAGING, ROOT

CASES = {
    "nop": "nop",
    "mov-register": "mov eax, ebx",
    "add-register": "add eax, ebx",
    "load": "mov eax, [0x10000]",
    "store": "mov [0x10000], eax",
    "memory-rmw": "add dword [0x10000], 1",
    # Opcode-encoded operands/conditions exercise a different dispatch shape
    # from ModR/M ALU loops. Keep their results separate, never invent a guest
    # workload average by giving these artificial loops equal weight.
    "inc-register": "inc eax",
    "mov-immediate": "mov eax, 7",
    "push-register": "push eax",
    "push-pop-register": "push eax\npop eax",
    # Both outcomes land on the next instruction; only the condition decoding
    # and branch path differ. Exact bytes force the short Jcc encoding.
    "jcc-conditions": "\n".join(f"db {0x70 + cc}, 0" for cc in range(16)),
}
GROUP_INSTRUCTIONS = {"push-pop-register": 2, "jcc-conditions": 16}


def profile_interval(directory, samples):
    """Subtract endpoint profiles, and require every retired word to be present.

    The sample endpoints can take different final branches, so individual PCs
    may have negative deltas. Retain those; clipping them would inflate cost.
    This is the same synthetic interval used by the headline count, not a new
    definition of which firmware instructions count toward the OS objective.
    """
    profiles = []
    for sample in samples:
        path = directory / f"iss-{sample['x86_steps']}-profile.csv"
        with path.open() as source:
            counts = {int(row["pc"], 16): int(row["retired"])
                      for row in csv.DictReader(source)}
        assert sum(counts.values()) == sample["retired_native"], path
        profiles.append(counts)
    before, after = profiles
    delta = {pc: after.get(pc, 0) - before.get(pc, 0)
             for pc in before.keys() | after.keys()}
    assert sum(delta.values()) == samples[1]["retired_native"] - samples[0]["retired_native"]
    path = directory / "interval-profile.csv"
    with path.open("w") as output:
        writer = csv.writer(output)
        writer.writerow(("pc", "retired_delta"))
        writer.writerows((f"0x{pc:08x}", count) for pc, count in sorted(delta.items()) if count)
    return path.name


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--firmware", required=True, type=Path)
    parser.add_argument("--out", type=Path, default=ROOT / "r86chip/experiments/out/interpreter-cost")
    parser.add_argument("--case", action="append", choices=CASES)
    parser.add_argument("--profile", action="store_true", help="retain both native PC profiles and their difference")
    parser.add_argument("--simulator", type=Path, default=ROOT / "target/release/r86chip-sim",
                        help="pinned gold ISS executable")
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    firmware = args.firmware.resolve()
    iss = args.simulator.resolve()
    report = dict(scope=__doc__, firmware_sha256=hashlib.sha256(firmware.read_bytes()).hexdigest(),
                  iss_sha256=hashlib.sha256(iss.read_bytes()).hexdigest(), cases={})
    for name, body in CASES.items():
        if args.case and name not in args.case:
            continue
        directory = out / name
        directory.mkdir(exist_ok=True)
        source = "bits 32\norg 0x1000\ncli\nmov esp, 0x200000\n" + PAGING
        source += "mov ebx, 3\nmain_loop:\n" + (body + "\n") * 32 + "jmp main_loop\n"
        asm = directory / "guest.asm"
        asm.write_text(source)
        subprocess.run(["nasm", "-f", "bin", asm, "-o", directory / "guest.bin"], check=True)
        code = (directory / "guest.bin").read_bytes()
        assert len(code) < 4096
        # Sparse on disk; the ISS still executes against its full native RAM.
        with (directory / "guest.ram").open("wb") as memory:
            memory.truncate(128 << 20)
            memory.seek(0x1000)
            memory.write(code)
        samples = []
        loop_instructions = 32 * GROUP_INSTRUCTIONS.get(name, 1) + 1
        for steps in (10_000, 10_000 + 1000 * loop_instructions):
            (directory / "config.bin").write_bytes(struct.pack("<8I", 0x5238364d, 1,
                0x1000, 0x90000, 128 << 20, steps, 0, 0))
            with (directory / f"iss-{steps}.log").open("w") as log:
                subprocess.run([str(iss), "machine", str(firmware), str(directory),
                                "200000000", "120"] + (["--profile"] if args.profile else []),
                               stdout=log, stderr=log, check=True)
            result = json.loads((directory / "iss-run.json").read_text())
            assert result["status"] == 4 and result["x86_steps"] == steps, result
            assert result["bank_enters"] == 0 and result["jit_native_steps"] == 0, result
            assert result["x86_completed"] == steps, "use current firmware built with the r86chip feature"
            (directory / f"iss-{steps}.json").write_text(json.dumps(result, indent=2) + "\n")
            if args.profile:
                (directory / "iss-profile.csv").replace(directory / f"iss-{steps}-profile.csv")
            samples.append(result)
        completed = samples[1]["x86_completed"] - samples[0]["x86_completed"]
        cost = (samples[1]["retired_native"] - samples[0]["retired_native"]) / completed
        report["cases"][name] = dict(retired_native_per_completed_x86=cost,
                                    guest_loop_instructions=loop_instructions,
                                    chip_extensions=samples[0]["chip_extensions"], samples=samples)
        if args.profile:
            report["cases"][name]["interval_profile"] = profile_interval(directory, samples)
        (out / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"{name}: {cost:.2f} retired RV/completed x86, chip extensions={samples[0]['chip_extensions']} (paging on, no JIT)", flush=True)


if __name__ == "__main__":
    main()
