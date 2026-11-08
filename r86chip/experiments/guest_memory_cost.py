#!/usr/bin/env python3
"""Measure guest-memory primitives on the one-bank RTL, with the ISS as gold.

Warm operations include segment/context/TLB checks; the cold example includes a
real RV refill stub reading known RAM metadata. EA cases consume original
SIB/displacement bytes and the native software GPR array. This does not include
an x86 page walk, opcode decoder, guest boundary/time work or complete service ABI. These
are component costs, not the <=20 native/x86 acceptance test.
"""
import argparse
import hashlib
import json
from pathlib import Path
import struct

import rtl

OUT = rtl.HERE / "out/guest-memory/cost"
FETCH_CASES = {"fetch16-in-word": (1, 1), "fetch16-two-words": (1, 3),
               "fetch32-aligned": (2, 0), "fetch32-two-words": (2, 3)}
# raw ModR/M, code lane, original bytes, expected data-bus reads. Both the
# address operation and its EAINFO consumer are included in each measurement.
EA_CASES = {
    "ea-base": (0x00, 0, b"", 1),
    "ea-disp8": (0x40, 0, b"\x80", 2),
    "ea-disp32-aligned": (0x80, 0, b"\x78\x56\x34\x12", 2),
    "ea-disp32-split": (0x80, 3, b"\x78\x56\x34\x12", 3),
    "ea-sib": (0x04, 0, b"\x48", 3),
    "ea-sib-disp32": (0x84, 0, b"\x88\x78\x56\x34\x12", 5),
    "ea-absolute": (0x05, 0, b"\x78\x56\x34\x12", 1),
    "ea-rejected": (0x84, 0, b"\x88\x78\x56\x34\x12", 0),
}


def custom(op, f, d=0, a=0, b=0, k=0):
    return op | d << 7 | f << 12 | a << 15 | b << 20 | k << 25


def image(kind, count):
    p = rtl.Program()
    p.li(1, 0x10000); p.li(2, 0x18000); p.emit(custom(0x2b, 4, a=1, b=2))
    p.li(1, 0x37); p.emit(custom(0x2b, 5, a=1))
    p.li(2, rtl.MASK); p.emit(custom(0x2b, 6, 3, 0, 2, 3))
    p.li(1, 0x4000); p.li(2, 0x10003); p.emit(custom(0x2b, 1, a=1, b=2))
    p.li(8, 1); p.li(6, 123); p.li(23, 0x9000)
    p.li(2, 0xf000); p.emit(custom(0x2b, 7, a=2))
    p.emit(custom(0x0b, 4))
    if kind in FETCH_CASES:
        width, lane = FETCH_CASES[kind]
        p.li(2, rtl.MASK); p.emit(custom(0x2b, 6, 1, 0, 2, 4))
        p.li(2, 0x10004); p.emit(custom(0x2b, 1, a=1, b=2, k=1))
        p.li(1, 0x4000 + lane)
    if kind in EA_CASES:
        raw, lane, _, _ = EA_CASES[kind]
        p.li(2, rtl.MASK); p.emit(custom(0x2b, 6, 1, 0, 2, 4))
        p.li(2, 0x10004); p.emit(custom(0x2b, 1, a=1, b=2, k=1))
        p.li(9, 0x8000); p.emit(custom(0x7b, 2, a=9))
        p.li(10, raw); p.li(11, 0x4000+lane)
        if kind == "ea-rejected":
            p.emit(custom(0x2b, 3))
    for _ in range(count):
        if kind in EA_CASES:
            p.emit(custom(0x7b, 0, 12, 10, 11), custom(0x7b, 1, 13))
            continue
        if kind in FETCH_CASES:
            p.emit(custom(0x5b, width, 6, 1, k=49))
            continue
        if kind == "refill-stub":
            p.emit(custom(0x2b, 2, a=1))
        if kind != "store":
            p.emit(custom(0x5b, 2, 6, 1, k=11 if kind == "rmw" else 3))
        if kind == "rmw":
            p.emit(custom(0x0b, 0, 6, 6, 8))
        if kind in ("store", "rmw"):
            p.emit(custom(0x5b, 6, a=1, b=6, k=35 if kind == "rmw" else 3))
    p.finish()
    assert p.pc < 0x9000
    struct.pack_into("<I", p.mem, 0x10000, 0x1234)
    struct.pack_into("<I", p.mem, 0x9000, 0x10003)
    if kind in EA_CASES:
        _, lane, code, _ = EA_CASES[kind]
        struct.pack_into("<8I", p.mem, 0x8000, *range(1, 9))
        p.mem[0x10000+lane:0x10000+lane+len(code)] = code
    p.pc = 0xf000
    p.emit(custom(0x2b, 7, 20, 1, k=1))  # suspended guest offset
    p.emit(rtl.it(0x03, 21, 2, 23, 0))   # native metadata load
    p.emit(custom(0x2b, 1, a=20, b=21), custom(0x2b, 7, k=2))
    return bytes(p.mem)


def main():
    global OUT
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=OUT)
    parser.add_argument("--rtl-out", type=Path, default=rtl.OUT)
    parser.add_argument("--simulator", type=Path, default=rtl.SIMULATOR)
    args = parser.parse_args()
    OUT, rtl.OUT, rtl.SIMULATOR = args.out.resolve(), args.rtl_out.resolve(), args.simulator.resolve()
    OUT.mkdir(parents=True, exist_ok=True)
    executable = rtl.OUT / "b1/obj/Vcore_tb"
    rows = []
    for kind, words in (("load", 1), ("store", 1), ("rmw", 3), ("refill-stub", 7),
                        *((name, 1) for name in FETCH_CASES),
                        *((name, 2) for name in EA_CASES)):
        endpoints = []
        for count in (16, 1024):
            data = image(kind, count)
            path = OUT / f"{kind}-{count}.hex"
            path.write_text("\n".join(f"{b:02x}" for b in data) + "\n")
            path.with_suffix(".bin").write_bytes(data)
            expected, memory = rtl.reference(data, 1)
            rtl.compare_iss(path, expected, memory)
            log = rtl.run([executable, f"+IMAGE={path}", "+LATENCY=0", "+SEED=1"])
            path.with_suffix(".log").write_text(log)
            actual = [(s[0], *[int(v, 16) for v in s.split()[1:]])
                      for s in log.splitlines() if s.startswith(("R ", "D ", "S ", "T "))]
            assert actual == expected, (kind, count)
            cycles, retired, *_ = map(int, next(s for s in log.splitlines() if s.startswith("C ")).split()[1:])
            reads = (EA_CASES[kind][3] if kind in EA_CASES else
                     2 if kind == "refill-stub" or kind.endswith("two-words") else int(kind != "store"))
            assert sum(e[0] == "D" and e[2] == 0 for e in expected) == count * reads
            assert sum(e[0] == "S" for e in expected) == count * int(kind in ("store", "rmw"))
            endpoints.append(dict(count=count, cycles=cycles, retired=retired,
                                  image_sha256=hashlib.sha256(data).hexdigest()))
        small, large = endpoints
        delta = large["count"] - small["count"]
        assert large["retired"] - small["retired"] == words * delta
        rows.append(dict(operation=kind, native_words=words,
                         clocks=(large["cycles"]-small["cycles"])/delta,
                         data_read_beats=reads, endpoints=endpoints))
    report = dict(scope=__doc__, banks=1,
                  executable_sha256=hashlib.sha256(executable.read_bytes()).hexdigest(),
                  iss_sha256=hashlib.sha256(rtl.SIMULATOR.read_bytes()).hexdigest(),
                  results=rows)
    (OUT / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    for row in rows:
        print(f"{row['operation']}: {row['native_words']} RV, {row['clocks']:g} clocks")


if __name__ == "__main__":
    main()
