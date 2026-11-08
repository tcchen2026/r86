#!/usr/bin/env python3
"""Check observations in a published RTL checkpoint against a host prefix.

Read-only access to machine_tb.cpp's saved RAM suffix; no RTL model state is
decoded, restored or synthesized. Supports the little-endian Verilator 5.032
save02 container used by these runs. A matching prefix never completes a boot
gate: compare_machine.py must still check the native terminal EBREAK and final
guest state. An open descriptor stays valid across atomic checkpoint replacement.
"""
import argparse
import json
import os
from pathlib import Path
import struct

from compare_machine import PREFIX, boundary

RAM_BYTES = 512 << 20
CONFIG = 0x00800000
OUTPUT = 0x00900000
BOUNDARIES = 0x18000000
HEADER = b"verilatorsave02\n"
TRAILER = b"vltsaved"


def compare(directory, reference=None):
    directory = Path(directory)
    reference = Path(reference) if reference else directory
    checkpoint = directory / "rtl-checkpoint.vlt"
    differences = {}
    with checkpoint.open("rb") as saved:
        size = os.fstat(saved.fileno()).st_size
        ram_offset = size - len(TRAILER) - RAM_BYTES

        def read_at(offset, length):
            saved.seek(offset)
            data = saved.read(length)
            if len(data) != length:
                raise ValueError("truncated checkpoint")
            return data

        header = read_at(0, 60)
        if (ram_offset < 60 or not header.startswith(HEADER)
                or read_at(size - len(TRAILER), len(TRAILER)) != TRAILER
                or struct.unpack_from("<Q", header, 16)[0] != 0x52383643484B0001):
            raise ValueError("unsupported checkpoint container")
        cycles, retired = struct.unpack_from("<QQ", header, 32)
        config = read_at(ram_offset + CONFIG, 0xBC)
        if struct.unpack_from("<II", config) != (0x5238364D, 1):
            raise ValueError("checkpoint has no recognized machine configuration")
        flags = struct.unpack_from("<I", config, 28)[0]
        status, steps_lo, steps_hi = struct.unpack_from("<III", config, 0x40)
        output_length = struct.unpack_from("<I", config, 0x50)[0]
        if output_length > 1 << 20:
            raise ValueError("invalid output length")
        serial = read_at(ram_offset + OUTPUT, output_length)
        expected_serial = PREFIX + (reference / "host-serial.log").read_bytes()
        if not expected_serial.startswith(serial):
            first = next((i for i, (a, b) in enumerate(zip(serial, expected_serial))
                          if a != b), min(len(serial), len(expected_serial)))
            differences["serial"] = dict(first_firmware_offset=first)

        records, last_boundary = None, None
        if flags & 4:
            # Firmware publishes BOUNDARY_LEN after all 16 volatile word stores.
            # Ignore any next record whose length has not yet been published.
            length = struct.unpack_from("<I", config, 0xB8)[0]
            if length > 64 << 20 or length % 64:
                raise ValueError("invalid boundary trace length")
            trace = read_at(ram_offset + BOUNDARIES, length)
            oracle = (reference / "host-boundaries.bin").read_bytes()
            if len(oracle) % 64:
                raise ValueError("partial reference boundary record")
            records = length // 64
            for offset in range(0, length, 64):
                actual, expected = trace[offset:offset + 64], oracle[offset:offset + 64]
                if actual != expected:
                    differences["boundary"] = dict(record=offset // 64,
                        expected=boundary(expected) if expected else None,
                        actual=boundary(actual))
                    break
            if trace:
                last_boundary = boundary(trace[-64:])

    return dict(engine="verilator-checkpoint-observations", equal=not differences,
                complete_boot=False, scope="published observation prefix only",
                checkpoint=str(checkpoint), cycles=cycles, rv_retired=retired,
                firmware_status=status, observed_x86_steps=steps_lo | steps_hi << 32,
                guest_serial_bytes=max(0, len(serial) - len(PREFIX)),
                boundary_records=records, last_boundary=last_boundary,
                differences=differences)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--reference", type=Path)
    args = parser.parse_args()
    result = compare(args.directory, args.reference)
    (args.directory / "rtl-checkpoint-prefix.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    return 0 if result["equal"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
