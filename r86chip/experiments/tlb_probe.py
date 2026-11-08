#!/usr/bin/env python3
"""Measure warm translation primitives, not complete x86 interpretation.

Requires rtl.py --gowin (or rtl.py) for the current generated core and ISS.
Compare two unrolled lengths to cancel setup and terminal-trap overhead. The
full decode/segment/event/miss costs are deliberately NOT represented here and
must be added before claiming an interpreter instruction budget or throughput.
"""
import hashlib
import json
from pathlib import Path
import struct

import rtl

OUT = rtl.HERE / "out/tlb/cost"


def image(kind, count):
    p = rtl.Program()
    p.li(1, 0x10000); p.li(2, 0x18000)
    p.emit(rtl.it(0x2b, 0, 4, 1, 2))
    p.li(1, 0x4000); p.li(2, 0x10003)
    p.emit(rtl.it(0x2b, 0, 1, 1, 2))
    p.li(31, rtl.MASK); p.li(8, 1); p.li(6, 123)
    p.emit(rtl.it(0x0b, 0, 4, 0, 0))
    mode = {"probe": 2, "load": 2, "store": 6, "rmw": 14}[kind]
    for _ in range(count):
        p.emit(rtl.it(0x2b, 5, 0, 1, mode))
        if kind != "probe":
            # Service branches are not taken in this warm-only fixture. If any
            # lookup unexpectedly misses, exact retirement/data counts below fail.
            p.emit(rtl.br(0, 5, 31, 20 if kind == "rmw" else 8))
        if kind in ("load", "rmw"):
            p.emit(rtl.it(0x03, 6, 2, 5, 0))
        if kind == "rmw":
            p.emit(0x0b | 6 << 7 | 6 << 15 | 8 << 20)
        if kind in ("store", "rmw"):
            p.emit(rtl.st(2, 5, 6, 0))
        if kind == "rmw":
            p.emit(rtl.it(0x0b, 0, 6, 0, 0))
    struct.pack_into("<I", p.mem, 0x10000, 0x1234)
    return p.finish()


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    executable = rtl.OUT / "b2/obj/Vcore_tb"
    rows = []
    for kind, words in (("probe", 1), ("load", 3), ("store", 3), ("rmw", 6)):
        endpoints = []
        for count in (16, 1024):
            data = image(kind, count)
            path = OUT / f"{kind}-{count}.hex"
            path.write_text("\n".join(f"{b:02x}" for b in data) + "\n")
            path.with_suffix(".bin").write_bytes(data)
            expected, memory = rtl.reference(data, 2)
            rtl.compare_iss(path, expected, memory)
            log = rtl.run([executable, f"+IMAGE={path}", "+LATENCY=0", "+SEED=1"])
            path.with_suffix(".log").write_text(log)
            actual = [(line[0], *[int(v, 16) for v in line.split()[1:]])
                      for line in log.splitlines() if line.startswith(("R ", "D ", "S ", "T "))]
            assert actual == expected, (kind, count)
            cycles, retired, *_ = map(int, next(s for s in log.splitlines() if s.startswith("C ")).split()[1:])
            reads = sum(e[0] == "D" and e[2] == 0 for e in expected)
            stores = sum(e[0] == "S" for e in expected)
            assert reads == (count if kind in ("load", "rmw") else 0)
            assert stores == (count if kind in ("store", "rmw") else 0)
            endpoints.append(dict(count=count, cycles=cycles, retired=retired,
                                  image_sha256=hashlib.sha256(data).hexdigest()))
        small, large = endpoints
        delta = large["count"] - small["count"]
        assert large["retired"] - small["retired"] == words * delta
        rows.append(dict(operation=kind, native_words=words,
                         clocks=(large["cycles"]-small["cycles"])/delta, endpoints=endpoints))
    report = dict(scope="warm guest-TLB primitive only, zero-wait native RAM; no x86 decoder/segments/events/services",
                  executable_sha256=hashlib.sha256(executable.read_bytes()).hexdigest(),
                  iss_sha256=hashlib.sha256((rtl.ROOT / "target/release/r86chip-sim").read_bytes()).hexdigest(),
                  results=rows)
    (OUT / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    for row in rows:
        print(f"{row['operation']}: {row['native_words']} RV, {row['clocks']:g} clocks")


if __name__ == "__main__":
    main()
