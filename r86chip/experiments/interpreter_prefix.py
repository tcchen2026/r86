#!/usr/bin/env python3
"""Compare a bounded TinyCore startup prefix, including native firmware setup.

Use matching tinycore.py outputs as --software and --native. This is an early-kernel diagnostic,
NOT the fixed full boot/Lua/ReactOS acceptance corpus or its measurement interval.
Both builds execute the same x86 interpreter source with JIT disabled on the ISS.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import struct
import subprocess

from jit_checks import ROOT
from interpreter_checks import GUEST_FIELDS


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--software", type=Path, required=True)
    parser.add_argument("--native", type=Path, required=True)
    parser.add_argument("--out", type=Path, default=ROOT / "r86chip/experiments/out/interpreter/prefix")
    parser.add_argument("--steps", type=int, default=1000000)
    args = parser.parse_args()
    if not 0 < args.steps < 1 << 32:
        parser.error("--steps must fit a positive u32")
    templates = {"software": args.software.resolve(), "native": args.native.resolve()}
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    iss = ROOT / "target/release/r86chip-sim"
    paths = [t / name for t in templates.values() for name in ("firmware.elf", "guest.ram", "config.bin")] + [iss]
    assert (templates["software"] / "guest.ram").read_bytes() == (templates["native"] / "guest.ram").read_bytes(), "guest inputs differ"
    report = {"scope": __doc__, "steps": args.steps,
              "hashes": {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in paths}, "modes": {}}
    for mode, template in templates.items():
        directory = out / mode
        directory.mkdir(exist_ok=True)
        for name in ("guest.ram", "firmware.elf", "firmware.bin"):
            shutil.copyfile(template / name, directory / name)
        config = bytearray((template / "config.bin").read_bytes())
        # Boundary recording is diagnostic firmware work and remains counted.
        struct.pack_into("<III", config, 20, args.steps, 0, 4)
        (directory / "config.bin").write_bytes(config)
        with (directory / "iss-console.log").open("w") as output:
            subprocess.run([str(iss), "machine", str(directory / "firmware.elf"), str(directory),
                            "10000000000", "120"], check=True, stdout=output, stderr=output)
        result = json.loads((directory / "iss-run.json").read_text())
        assert result["status"] == 4 and result["x86_steps"] == args.steps, result
        assert result["x86_completed"] > 0 and result["jit_native_steps"] == result["bank_enters"] == 0, result
        cost = result["retired_native"] / result["x86_completed"]
        assert result["chip_extensions"] == int(mode == "native")
        report["modes"][mode] = {"retired_native_per_completed_x86": cost, "result": result}
        print(f"{mode}: {cost:.2f} retired native/completed x86", flush=True)
    for field in GUEST_FIELDS:
        assert report["modes"]["native"]["result"][field] == report["modes"]["software"]["result"][field], field
    for suffix in ("serial.log", "boundaries.bin"):
        assert (out / "native" / f"iss-{suffix}").read_bytes() == (out / "software" / f"iss-{suffix}").read_bytes(), suffix
    report["guest_comparison_passed"] = True
    (out / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print("PASS guest state, RAM, completion counts, serial and boundary trace agree", flush=True)


if __name__ == "__main__":
    main()
