#!/usr/bin/env python3
"""Compare a completed RV machine run with its independent host x86 reference.

--prefix permits matched bounded endpoints, without treating them as a boot pass.
Native wall time and implementation counters are deliberately not guest state.
"""
import argparse
import json
from pathlib import Path
import struct

PREFIX = b"R86 RV32IM interpreter starting\nR86 guest machine ready\n"
FIELDS = ("x86_steps", "eip", "x86_regs", "eflags", "tsc", "guest_ram_fnv64")


def boundary(record):
    words = struct.unpack("<16I", record)
    return dict(x86_steps=words[0] | words[1] << 32,
                tsc=words[2] | words[3] << 32, eip=words[4], eflags=words[5],
                x86_regs=words[6:14], cr0=words[14], cr3=words[15])


def compare(directory, engine, reference=None, prefix=False):
    directory = Path(directory)
    reference = Path(reference) if reference else directory
    actual = json.loads((directory / f"{engine}-run.json").read_text())
    expected = json.loads((reference / "host-run.json").read_text())
    metadata = json.loads((directory / f"{engine}-inputs.json").read_text())
    differences = {}
    native_finished = (actual.get("trap") == 1 and actual.get("trap_cause") == 3
                       if engine == "rtl" else "cause: Breakpoint" in actual.get("trap", ""))
    if not native_finished or actual.get("status") not in (3, 4):
        differences["native_stop"] = "comparison requires the firmware's terminal EBREAK, not a live observation"
    for field in FIELDS:
        if field not in actual or field not in expected or actual[field] != expected[field]:
            differences[field] = dict(expected=expected.get(field), actual=actual.get(field))
    serial = (directory / f"{engine}-serial.log").read_bytes()
    reference_serial = (reference / "host-serial.log").read_bytes()
    if not serial.startswith(PREFIX):
        differences["firmware_console_prefix"] = "missing"
    else:
        serial = serial[len(PREFIX):]
        if serial != reference_serial:
            offset = next((i for i, (a, b) in enumerate(zip(serial, reference_serial)) if a != b),
                          min(len(serial), len(reference_serial)))
            differences["serial"] = dict(first_offset=offset, expected_bytes=len(reference_serial),
                                         actual_bytes=len(serial))
    records = None
    if metadata.get("boundaries"):
        trace = (directory / f"{engine}-boundaries.bin").read_bytes()
        oracle = (reference / "host-boundaries.bin").read_bytes()
        if len(trace) % 64 or len(oracle) % 64:
            raise ValueError("partial boundary record")
        records = len(trace) // 64
        for offset in range(0, max(len(trace), len(oracle)), 64):
            a, b = trace[offset:offset+64], oracle[offset:offset+64]
            if a != b:
                differences["boundary"] = dict(record=offset // 64,
                    expected=boundary(b) if b else None, actual=boundary(a) if a else None)
                break
    complete = actual["status"] == 3 and actual["marker"] and expected["marker"]
    if metadata.get("jit") and complete:
        for field in ("jit_native_steps", "jit_invocations") + (("jit_user_steps",) if metadata.get("lua") else ()):
            if actual.get(field, 0) == 0:
                differences[field] = "expected observed native execution"
    result = dict(equal=not differences, complete_boot=bool(complete),
                  passed=not differences and (complete or prefix),
                  compared_fields=FIELDS, serial_bytes=len(serial), boundary_records=records,
                  differences=differences)
    (directory / f"{engine}-comparison.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--engine", choices=("iss", "rtl"), required=True)
    parser.add_argument("--reference", type=Path)
    parser.add_argument("--prefix", action="store_true")
    args = parser.parse_args()
    result = compare(args.directory, args.engine, args.reference, args.prefix)
    print(json.dumps(result, indent=2))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
