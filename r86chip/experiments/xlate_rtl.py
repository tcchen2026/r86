#!/usr/bin/env python3
"""Compare the translator words on the Chisel core with the gold ISS.

Each fixture is a native image run on `r86chip-sim native` and on rtl.py's
Verilator core_tb under three bus schedules and Icarus under one four-state
schedule. The RTL must end with the same
trap, retired count, RAM and every register it wrote. This checks end state,
not bus order; rtl.py's event oracle does not model these words.

Fixture environment: guest linear page p is backed by native page
0x10000 + (p % 16) * 4096. A real RV service handler at 0x0c00 fills a missing
data mapping with the rights stored at native 0x7fc and retries; any other
reason, or a second service at the same PC, skips the word. x25..x31 belong to
the handler; x26 counts services.

Usage: xlate_rtl.py --rtl RTL_PY_OUT --simulator r86chip-sim [--banks 2]
The optional --bench runs the hand-translated loops instead of the fixtures.
"""
import argparse
import json
from pathlib import Path
import random
import subprocess

SIZE = 131072
HANDLER = 0x0c00
BANK1 = 0x0e00
RIGHTS = 0x7fc


def r(op, f3, f7, rd=0, rs1=0, rs2=0):
    return op | rd << 7 | f3 << 12 | rs1 << 15 | rs2 << 20 | f7 << 25


def i(op, f3, rd, rs1, imm):
    return op | rd << 7 | f3 << 12 | rs1 << 15 | (imm & 0xfff) << 20


def s(op, f3, rs1, rs2, imm):
    return op | (imm & 31) << 7 | f3 << 12 | rs1 << 15 | rs2 << 20 | ((imm >> 5) & 127) << 25


def b(f3, rs1, rs2, off, op=0x63):
    return (((off >> 12) & 1) << 31 | ((off >> 5) & 63) << 25 | rs2 << 20 | rs1 << 15 |
            f3 << 12 | ((off >> 1) & 15) << 8 | ((off >> 11) & 1) << 7 | op)


def jal(rd, off):
    return (((off >> 20) & 1) << 31 | ((off >> 1) & 1023) << 21 |
            ((off >> 11) & 1) << 20 | ((off >> 12) & 255) << 12 | rd << 7 | 0x6f)


XFGET = lambda rd: 0x0b | rd << 7 | 5 << 12
XFLOAD = lambda rs: 0x0b | 4 << 12 | rs << 15
EBREAK = 0x00100073


class Program:
    def __init__(self, seed=1):
        self.mem = bytearray(SIZE)
        rng = random.Random(seed)
        for n in range(0x10000, SIZE):
            self.mem[n] = rng.getrandbits(8)
        self.mem[RIGHTS:RIGHTS + 4] = (3).to_bytes(4, "little")
        self.pc = 0x1000
        # Handler; see the module docstring.
        self.at(HANDLER,
                i(0x13, 0, 26, 26, 1),
                r(0x2b, 7, 1, rd=28, rs1=1), r(0x2b, 7, 1, rd=29, rs1=3),
                r(0x2b, 7, 1, rd=30, rs1=0),
                i(0x13, 0, 31, 0, 4),
                b(1, 29, 31, 48),              # reason != 4: skip
                b(0, 30, 25, 44),              # same PC again: skip
                i(0x13, 0, 25, 30, 0),
                i(0x13, 1, 31, 28, 16), i(0x13, 5, 31, 31, 28), i(0x13, 1, 31, 31, 12),
                0x37 | 27 << 7 | 0x10000, r(0x33, 0, 0, rd=31, rs1=31, rs2=27),
                i(0x03, 2, 27, 0, RIGHTS), r(0x33, 6, 0, rd=31, rs1=31, rs2=27),
                r(0x2b, 1, 0, rs1=28, rs2=31),
                r(0x2b, 7, 2),                 # XGRET x0: retry
                i(0x13, 0, 30, 30, 4), r(0x2b, 7, 2, rs1=30))
        assert self.mem[HANDLER + 5 * 4 + 48:][:4] == i(0x13, 0, 30, 30, 4).to_bytes(4, "little")
        # Flat context: region, PE, six RWX segments, service vector.
        self.li(5, 0x10000); self.li(6, 0x20000); self.emit(r(0x2b, 4, 0, rs1=5, rs2=6))
        self.li(5, 1); self.emit(r(0x2b, 5, 0, rs1=5, rs2=5))
        self.li(6, 0xffffffff)
        for segment in range(6):
            self.emit(r(0x2b, 6, 7, rd=segment, rs1=0, rs2=6))
        self.li(5, HANDLER); self.emit(r(0x2b, 7, 0, rs1=5))
        for reg in range(25, 32):
            self.emit(i(0x13, 0, reg, 0, 0))

    def at(self, address, *words):
        for n, word in enumerate(words):
            self.mem[address + 4 * n:address + 4 * n + 4] = (word & 0xffffffff).to_bytes(4, "little")

    def emit(self, *words):
        self.at(self.pc, *words)
        self.pc += 4 * len(words)
        assert 0x1000 < self.pc <= 0x10000

    def li(self, rd, value):
        value &= 0xffffffff
        self.emit(0x37 | rd << 7 | ((value + 0x800) & 0xfffff000), i(0x13, 0, rd, rd, value))

    def fill(self, linear, rights=3):
        self.li(5, linear); self.li(6, (0x10000 + ((linear >> 12) % 16 << 12)) | rights)
        self.emit(r(0x2b, 1, 0, rs1=5, rs2=6))

    def finish(self):
        self.emit(EBREAK)
        return bytes(self.mem)


def cases():
    out = []
    rng = random.Random(86)

    # Displacement loads and stores: every funct3, both segments, every byte
    # alignment, word crossings, prefilled hits and handler fills.
    for segment, (load, store) in enumerate(((0x07, 0x27), (0x47, 0x77))):
        p = Program(segment + 2)
        p.fill(0x5000)
        p.li(1, 0x5800); p.li(2, 0x9abcdef1); p.li(3, 0x7040)
        for base, reg in ((0x5800, 1), (0x7040, 3)):       # page 7 comes from the handler
            for off in (0, 1, 2, 3, 5, 6, 7, -1, -2, -3, -4, 0x7fc, -0x800):
                for f3 in range(8):
                    p.emit(i(load, f3, 8 + f3, reg, off))
                for f3 in (0, 1, 2):
                    p.emit(s(store, f3, reg, 8 + f3, off + 8), i(load, 2, 20, reg, off + 8))
                p.emit(i(0x33, 4, 2, 2, 0) | 9 << 20)        # x2 ^= x9
                p.emit(s(store, 2, reg, 2, off))
        out.append((f"disp-{segment}", p.finish()))

    # PUSH forms, including push of the base register and a load right behind.
    p = Program(5)
    p.li(4, 0x8010); p.li(9, 0x11111111)
    for n in range(12):
        p.emit(s(0x77, 6, 4, 9, -4), i(0x13, 0, 9, 9, 0x123), i(0x47, 2, 10 + n % 4, 4, 0))
    p.emit(s(0x77, 6, 4, 4, -4), i(0x47, 2, 14, 4, 0))       # push esp stores the old value
    p.emit(s(0x77, 5, 4, 9, -2), s(0x27, 4, 4, 9, -1), i(0x47, 3, 15, 4, 0))
    p.emit(s(0x77, 6, 0, 9, 0x40))                           # x0 base: write discarded
    for n in range(6):                                       # pops
        p.emit(i(0x47, 2, 16 + n, 4, 0), i(0x13, 0, 4, 4, 4))
    out.append(("push", p.finish()))

    # Service reasons: page crossing (3), segment limit and rights (2), a
    # read-only mapping written (4 twice, then skipped), context (1).
    p = Program(6)
    p.li(1, 0x5ffd); p.li(2, 0x55aa55aa)
    p.emit(i(0x07, 2, 8, 1, 0), i(0x07, 5, 9, 1, 2), i(0x07, 5, 10, 1, 1), s(0x27, 2, 1, 2, 1),
           s(0x27, 1, 1, 2, 2), s(0x27, 1, 1, 2, 1), i(0x07, 4, 11, 1, 2))
    p.fill(0x6000)
    p.li(5, 0x6000); p.li(6, 3); p.emit(r(0x2b, 6, 3, rd=3, rs1=5, rs2=6))        # DS base 0x6000, limit 3, RW
    p.li(1, 0)
    for off in range(0, 6):
        p.emit(i(0x07, 4, 12, 1, off), i(0x07, 5, 13, 1, off), i(0x07, 2, 14, 1, off),
               s(0x27, 0, 1, 2, off), s(0x27, 2, 1, 2, off))
    p.emit(i(0x07, 2, 15, 1, -1))
    p.li(6, 0xffffffff); p.emit(r(0x2b, 6, 1, rd=3, rs1=0, rs2=6))               # DS read only
    p.li(1, 0x5000); p.emit(i(0x07, 2, 16, 1, 0), s(0x27, 2, 1, 2, 0), i(0x07, 3, 17, 1, 0))
    p.emit(r(0x2b, 6, 7, rd=3, rs1=0, rs2=6))
    p.li(5, 1); p.emit(s(0x23, 2, 0, 5, RIGHTS)); p.li(1, 0xa000)                                  # handler fills read only
    p.emit(i(0x07, 2, 18, 1, 4), s(0x27, 2, 1, 2, 4), i(0x07, 3, 19, 1, 4), i(0x07, 2, 20, 1, 8))
    p.li(5, 9); p.emit(r(0x2b, 5, 0, rs1=5, rs2=5), i(0x07, 2, 21, 1, 4))        # VM set: reason 1
    out.append(("service", p.finish()))

    # Second-level table: pages that share an l1 row alternate, so every access
    # refills l1 from l2 and replays. Then invalidation, collision and flush.
    p = Program(7)
    pages = [0x1000, 0x41000, 0x81000, 0xc1000, 0x2000, 0x42000]
    for page in pages:
        p.fill(page)
    p.li(2, 0x01020304)
    for round in range(3):
        for n, page in enumerate(pages):
            p.li(1, page + 0x10 * round)
            p.emit(i(0x07, 2, 8 + n, 1, 3), s(0x27, 2, 1, 2, 5), i(0x13, 0, 2, 2, 0x111),
                   r(0x2b, 0, 0, rd=14 + n, rs1=1, rs2=2 + 4))                   # XTLOOK write dword
    p.li(1, 0x41000); p.emit(r(0x2b, 2, 0, rs1=1))                               # XTINVAL
    for n, page in enumerate(pages):
        p.li(1, page); p.emit(r(0x2b, 0, 0, rd=8 + n, rs1=1, rs2=2), i(0x07, 4, 14 + n, 1, 0))
    p.fill(0x401000)                                                             # l2 collision with 0x1000
    for n, page in enumerate((0x1000, 0x401000, 0x81000)):
        p.li(1, page); p.emit(r(0x2b, 0, 0, rd=20 + n, rs1=1, rs2=2), i(0x07, 2, 8 + n, 1, 0))
    p.emit(r(0x2b, 3, 0))                                                        # XTFLUSH
    for n, page in enumerate(pages):
        p.li(1, page); p.emit(r(0x2b, 0, 0, rd=8 + n, rs1=1, rs2=2), i(0x07, 2, 14 + n, 1, 0))
    out.append(("l2", p.finish()))

    # XALUI, XINC, XDEC and XSH against values at the flag boundaries.
    values = [0, 1, 2, 0xf, 0x10, 0x7f, 0x80, 0xff, 0x7fff, 0x8000, 0x7fffffff, 0x80000000,
              0xfffffffe, 0xffffffff, 0x12345678, 0x87654321]
    for half in range(2):
        p = Program(8)
        # The RF is not reset; initialize the fixture's checksum accumulators.
        p.emit(i(0x13, 0, 20, 0, 0), i(0x13, 0, 21, 0, 0))
        for value in values[half::2]:
            p.li(1, value)
            for carry in (0, 1):
                for kind in range(7):
                    for imm in (0, 1, -1, 0x7ff, -0x800, 0x10):
                        p.emit(i(0x13, 0, 3, 0, carry), XFLOAD(3),
                               i(0x1b, kind, 8, 1, imm), XFGET(9),
                               r(0x33, 4, 0, rd=20, rs1=20, rs2=8), r(0x33, 0, 0, rd=21, rs1=21, rs2=9),
                               i(0x13, 1, 21, 21, 1))
                for imm in (1, -1):
                    p.emit(i(0x13, 0, 3, 0, carry), XFLOAD(3), i(0x1b, 7, 8, 1, imm), XFGET(9),
                           r(0x33, 4, 0, rd=20, rs1=20, rs2=8), r(0x33, 0, 0, rd=21, rs1=21, rs2=9))
        out.append((f"alui-{half}", p.finish()))
    for kind in range(5):
        p = Program(9)
        for reg in range(20, 24):
            p.emit(i(0x13, 0, reg, 0, 0))
        for value in values:
            p.li(1, value)
            for count in list(range(0, 32, 3)) + [1, 31]:
                p.emit(i(0x13, 0, 3, 0, 0x8d5 if count & 1 else 0), XFLOAD(3),
                       r(0x3b, kind, 1, rd=8, rs1=1, rs2=count), XFGET(9),
                       i(0x13, 0, 4, 0, count + 32), r(0x3b, kind, 0, rd=10, rs1=1, rs2=4), XFGET(11),
                       r(0x33, 4, 0, rd=20, rs1=20, rs2=8), r(0x33, 0, 0, rd=21, rs1=21, rs2=9),
                       r(0x33, 4, 0, rd=22, rs1=22, rs2=10), r(0x33, 0, 0, rd=23, rs1=23, rs2=11),
                       i(0x13, 1, 21, 21, 1), i(0x13, 1, 23, 23, 1))
        out.append((f"shift-{kind}", p.finish()))

    # Jump table: hit, miss, a colliding EIP, invalidation, flush. x8 counts
    # misses and x10 every probe.
    p = Program(10)
    p.emit(i(0x13, 0, 8, 0, 0), i(0x13, 0, 10, 0, 0))

    def probe(eip, fill):
        p.li(1, eip)
        if fill:
            p.li(2, p.pc + 5 * 4)
            p.emit(r(0x6b, 1, 0, rs1=1, rs2=2))
        p.emit(r(0x6b, 0, 0, rs1=1), i(0x13, 0, 8, 8, 1), i(0x13, 0, 10, 10, 1))
    e0, e1, e2, e3 = 0x401000, 0x401004, 0x401400, 3
    # A probe without a fill must miss: a hit would jump back to an older site.
    probe(e0, True); probe(e2, False); probe(e2, True); probe(e0, False)
    probe(e1, True); p.li(1, e1); p.emit(r(0x6b, 2, 0, rs1=1)); probe(e1, False)
    probe(e1, True); probe(e3, True); p.emit(r(0x6b, 3, 0))
    probe(e1, False); probe(e3, False); probe(e3, True)
    out.append(("jump-table", p.finish()))

    # Return stack: ten XPUSHRET keep the newest eight; returns pop them in
    # order (x22 records the landings), then fall back to the table (E1) and
    # to a miss (E0). Then a mismatching EIP, an empty stack and a flush.
    p = Program(13)
    for reg in range(20, 25):
        p.emit(i(0x13, 0, reg, 0, 0))
    p.fill(0x8000)
    eip = lambda k: 0x00402000 + 0x40 * k + (k & 3)
    li2 = lambda rd, value: [0x37 | rd << 7 | ((value + 0x800) & 0xfffff000), i(0x13, 0, rd, rd, value)]
    push = lambda label: (lambda labels, pc: s(0x77, 7, 12, 5, (labels[label] - pc) // 4))
    goto = lambda label: (lambda labels, pc: jal(0, labels[label] - pc))
    fill = lambda label: (lambda labels, pc: [0x37 | 6 << 7 | ((labels[label] + 0x800) & 0xfffff000),
                                             i(0x13, 0, 6, 6, labels[label])][0])
    xjtr = r(0x6b, 0, 0, rs1=16, rs2=1)
    items = li2(12, 0x8800) + li2(5, eip(1)) + ["fill1", fill("L1"),
            (lambda labels, pc: i(0x13, 0, 6, 6, labels["L1"])), r(0x6b, 1, 0, rs1=5, rs2=6)]
    for k in range(10):
        items += li2(5, eip(k)) + [push(f"L{k}")]
    items += li2(16, eip(9)) + [xjtr, i(0x13, 0, 20, 20, 0x100), goto("END1")]
    for k in range(9, -1, -1):
        items += [f"L{k}", i(0x13, 0, 21, 21, 1), i(0x13, 1, 22, 22, 4), i(0x13, 0, 22, 22, k)]
        items += li2(16, eip(k - 1) if k else 0x7777) + [xjtr, goto("END1")]
    items += ["END1"] + li2(5, 0x500000) + ["fillb", fill("LB"),
              (lambda labels, pc: i(0x13, 0, 6, 6, labels["LB"])), r(0x6b, 1, 0, rs1=5, rs2=6)]
    items += li2(5, 0x600000) + [push("LA")] + li2(16, 0x500000) + [xjtr, i(0x13, 0, 24, 24, 0x10), goto("END2")]
    items += ["LB", i(0x13, 0, 23, 23, 1)] + li2(16, 0x600000) + [xjtr, i(0x13, 0, 24, 24, 1)]
    items += li2(5, 0x600000) + [push("LA"), r(0x6b, 3, 0)] + li2(16, 0x600000) + [xjtr, i(0x13, 0, 24, 24, 2)]
    items += li2(5, 0x600000) + [push("LA"), s(0x77, 6, 12, 5, -4)] + li2(16, 0x600000) + [xjtr, i(0x13, 0, 24, 24, 4)]
    items += [goto("END2"), "LA", i(0x13, 0, 23, 23, 0x100), "END2"]
    labels, pc = {}, p.pc
    for item in items:
        if isinstance(item, str):
            labels[item] = pc
        else:
            pc += 4
    for item in items:
        if not isinstance(item, str):
            p.emit(item(labels, p.pc) if callable(item) else item)
    out.append(("return-stack", p.finish()))

    # Budget: bank 1 loops on XTICK until it expires; bank 0 reads the record.
    p = Program(11)
    p.li(1, 100); p.emit(r(0x6b, 5, 0, rs1=1))
    p.li(31, BANK1); p.emit(i(0x13, 0, 9, 0, 0), r(0x7b, 7, 0, rd=5))
    for _ in range(3):
        p.emit(r(0x7b, 4, 0, rs1=31),
               r(0x7b, 6, 1, rd=10, rs1=0), r(0x7b, 6, 1, rd=11, rs1=1), r(0x6b, 6, 0, rd=12),
               r(0x7b, 6, 0, rd=13, rs1=5),
               r(0x33, 0, 0, rd=9, rs1=9, rs2=12), r(0x33, 0, 0, rd=9, rs1=9, rs2=13),
               i(0x13, 0, 1, 0, 50), r(0x6b, 5, 0, rs1=1))
    p.emit(i(0x6b, 4, 0, 0, 1))                              # XTICK in bank 0 is illegal
    p.at(BANK1, i(0x6b, 4, 0, 0, 7), i(0x13, 0, 5, 5, 1), jal(0, -8))
    out.append(("tick", bytes(p.mem)))

    # Reserved encodings stop without effects.
    illegal = [s(0x27, 3, 1, 2, 0), s(0x77, 7, 1, 2, 0), i(0x1b, 7, 8, 1, 2), i(0x1b, 7, 8, 1, 0),
               r(0x3b, 5, 0, rd=8, rs1=1, rs2=2), r(0x3b, 0, 2, rd=8, rs1=1, rs2=2),
               r(0x6b, 0, 0, rd=1, rs1=1), r(0x6b, 0, 1, rs1=1), r(0x6b, 1, 1, rs1=1, rs2=2),
               r(0x6b, 2, 0, rd=1, rs1=1), r(0x6b, 3, 0, rs1=1), r(0x6b, 5, 0, rd=1, rs1=1),
               r(0x6b, 6, 0, rd=1, rs1=1), r(0x6b, 7, 0), i(0x6b, 4, 0, 0, 0)]
    for n, word in enumerate(illegal):
        p = Program(12)
        p.li(1, 0x5000); p.li(2, 0x1234); p.emit(word, s(0x27, 2, 1, 2, 0))
        out.append((f"illegal-{n}", p.finish()))
    p = Program(12)
    p.li(1, 0x5000); p.li(2, 0x1002); p.emit(r(0x6b, 1, 0, rs1=1, rs2=2))        # misaligned target
    out.append(("illegal-target", p.finish()))

    # A split store ending in the last native RAM word, then the same store
    # with that second word outside RAM. The latter must expose native faults
    # and any partial write; the harness does not discard the first bus beat.
    for fault in (False, True):
        p = Program(14)
        p.fill(0xf000)
        p.li(1, 0xfffb); p.li(2, 0x89abcdef)
        p.emit(s(0x27, 2, 1, 2, 0), i(0x07, 2, 8, 1, 0))
        image = p.finish()
        out.append(("store-split-ram-" + ("fault" if fault else "last-word"),
                    image[:-4] if fault else image))

    # Consecutive flushes cross the 8-bit epoch wrap and its RAM sweep. Probe
    # old rows before refilling, then exercise hits, collisions and misses.
    p = Program(15)
    p.li(8, 0); p.li(10, 0)
    p.at(0x0b00, EBREAK)  # an incorrectly surviving entry has an observable target
    for eip in (0x401000, 0x401004):
        p.li(1, eip); p.li(2, 0x0b00); p.emit(r(0x6b, 1, 0, rs1=1, rs2=2))
    p.emit(*([r(0x6b, 3, 0)] * 300))
    for eip, fill in ((0x401000, False), (0x401004, False), (0x401000, True),
                      (0x401400, True), (0x401000, False), (0x401004, True)):
        p.li(1, eip)
        if fill:
            p.li(2, p.pc + 5 * 4); p.emit(r(0x6b, 1, 0, rs1=1, rs2=2))
        p.emit(r(0x6b, 0, 0, rs1=1), i(0x13, 0, 8, 8, 1), i(0x13, 0, 10, 10, 1))
    out.append(("jump-flush-300", p.finish()))

    p = Program(16)
    pages = (0x1000, 0x41000, 0x8000)
    for page in pages:
        p.fill(page)
    p.emit(*([r(0x2b, 3, 0)] * 300))
    for n, page in enumerate(pages):
        p.li(1, page + 4); p.emit(r(0x2b, 0, 0, rd=8 + n, rs1=1, rs2=2))
    for n, page in enumerate(pages):
        p.fill(page)
        p.li(1, page + 4)
        p.emit(r(0x2b, 0, 0, rd=12 + n, rs1=1, rs2=2), i(0x07, 2, 16 + n, 1, 1))
    # A second-level collision must retire just the selected old page.
    p.fill(0x401000)
    for n, page in enumerate((0x1000, 0x401000, 0x41000)):
        p.li(1, page + 4); p.emit(r(0x2b, 0, 0, rd=20 + n, rs1=1, rs2=2))
    out.append(("mapping-flush-300", p.finish()))

    # SS has a nonzero, unaligned base and an inclusive small limit. DS stays
    # flat. At the limit, qualify the requested bytes, not whole native words.
    p = Program(17)
    p.fill(0x6000)
    p.li(5, 0x6003); p.li(6, 0x20); p.emit(r(0x2b, 6, 3, rd=2, rs1=5, rs2=6))
    p.li(1, 0); p.li(2, 0x9180ff7f)
    for off in (0, 1, 2, 3, 0x1d, 0x1e, 0x20, 0x21, -1):
        for kind in range(8):
            p.emit(i(0x47, kind, 8 + kind, 1, off))
        for kind in range(3):
            p.emit(s(0x77, kind, 1, 2, off))
    p.emit(r(0x2b, 6, 1, rd=2, rs1=5, rs2=6))  # SS read-only
    p.emit(i(0x47, 2, 16, 1, 0), i(0x47, 3, 17, 1, 0), s(0x77, 2, 1, 2, 0))
    out.append(("ss-based-limit", p.finish()))
    return out


CODE1 = 0x8000
BUDGET = 10_000_000


def benchmarks():
    """Hand-translated x86 loops in the block shape of design/TRANSLATOR.md.

    Bank-1 x8..x15 are EAX..EDI. Each block starts with XTICK, so the budget
    consumed is the number of x86 instructions executed. Returns (name, x86
    source, builder(iterations) -> image).
    """
    xalu = lambda kind, rd, rs1, rs2: r(0x0b, 0, 8 | kind, rd=rd, rs1=rs1, rs2=rs2)
    xjcc = lambda cc, off: b(3, cc, 0, off, op=0x7b)
    tick = lambda n: i(0x6b, 4, 0, 0, n)
    leave = lambda reason: i(0x7b, 5, 0, 0, reason)

    def build(code, regs, pages, fills=()):
        def image(iterations):
            p = Program(20)
            for page in pages:
                p.fill(page)
            p.li(1, BUDGET); p.emit(r(0x6b, 5, 0, rs1=1))
            for n, value in enumerate(regs):
                p.li(5, iterations if value is None else value)
                p.emit(r(0x7b, 7, 0, rd=8 + n, rs1=5))
            for eip, index in fills:
                p.li(5, eip); p.li(6, CODE1 + 4 * index); p.emit(r(0x6b, 1, 0, rs1=5, rs2=6))
            p.li(31, CODE1)
            p.emit(r(0x7b, 4, 0, rs1=31), r(0x6b, 6, 0, rd=20), r(0x7b, 6, 1, rd=22, rs1=0))
            p.emit(*[r(0x7b, 6, 0, rd=8 + n, rs1=8 + n) for n in range(8)], XFGET(21), EBREAK)
            p.at(CODE1, *code)
            return bytes(p.mem)
        return image

    out = []
    # ECX is the iteration count (None) in every register list: EAX ECX EDX EBX ESP EBP ESI EDI.
    out.append(("sum", "mov eax,[esi]; add ebx,eax; add esi,4; dec ecx; jnz", build([
        tick(5), i(0x07, 2, 8, 14, 0), xalu(0, 11, 11, 8), i(0x1b, 0, 14, 14, 4),
        i(0x1b, 7, 9, 9, -1), xjcc(5, -20), leave(1)],
        [0, None, 0, 0, 0x8800, 0x8900, 0x4000, 0x5000], [0x4000])))
    ret_eip = 0x00401020
    out.append(("call", "push ebx; call f; pop ebx; dec ecx; jnz / f: mov eax,[esp+4]; "
                "add eax,[ebp+8]; mov [edi],eax; ret", build([
        tick(2), s(0x77, 6, 12, 11, -4), 0x37 | 1 << 7 | 0x401000, i(0x13, 0, 1, 1, 0x20),
        s(0x77, 7, 12, 1, 2), jal(0, 28),
        tick(3), i(0x47, 2, 11, 12, 0), i(0x13, 0, 12, 12, 4), i(0x1b, 7, 9, 9, -1),
        xjcc(5, -40), leave(1),
        tick(4), i(0x47, 2, 8, 12, 4), i(0x47, 2, 1, 13, 8), xalu(0, 8, 8, 1),
        s(0x27, 2, 15, 8, 0), i(0x47, 2, 16, 12, 0), i(0x13, 0, 12, 12, 4),
        r(0x6b, 0, 0, rs1=16, rs2=1), leave(2)],
        [0, None, 0, 0x1234, 0x8800, 0x8900, 0x4000, 0x5000], [0x5000, 0x8000], [(ret_eip, 6)])))
    out.append(("mixed", "mov eax,[ebp-8]; cmp eax,100; jl +; sub eax,100; +: shl eax,2; "
                "lea edx,[eax+eax*2+1]; mov [ebp-8],edx; inc dword [ebx]; xor eax,edx; dec ecx; jnz", build([
        tick(3), i(0x47, 2, 8, 13, -8), i(0x1b, 5, 0, 8, 100), xjcc(0xc, 12),
        tick(1), i(0x1b, 5, 8, 8, 100),
        tick(7), r(0x3b, 0, 1, rd=8, rs1=8, rs2=2), r(0x33, 2, 0x10, rd=10, rs1=8, rs2=8),
        i(0x13, 0, 10, 10, 1), s(0x77, 2, 13, 10, -8),
        i(0x07, 3, 1, 11, 0), i(0x1b, 7, 1, 1, 1), s(0x27, 2, 11, 1, 0),
        xalu(6, 8, 8, 10), i(0x1b, 7, 9, 9, -1), xjcc(5, -64), leave(1)],
        [0, None, 0, 0x5010, 0x8800, 0x8900, 0x4000, 0x5000], [0x5000, 0x8000])))
    return out


def fnv(data):
    digest = 0xcbf29ce484222325
    for byte in data:
        digest = ((digest ^ byte) * 0x100000001b3) & ((1 << 64) - 1)
    return digest


def run_rtl(executable, path, ram, seed, latency, log_path):
    log = subprocess.run([*map(str, executable), f"+IMAGE={path.with_suffix('.hex')}", f"+SEED={seed}",
                          f"+LATENCY={latency}", "+FAULT_PC=ffffffff", f"+RAM_BYTES={len(ram)}"],
                         text=True, capture_output=True)
    log_path.write_text(log.stdout + log.stderr)
    if log.returncode:
        raise RuntimeError(log.stdout[-3000:] + log.stderr[-2000:])
    memory = bytearray(ram)
    registers = [[None] * 32 for _ in range(2)]
    retired = 0
    trap = cycles = None
    for line in log.stdout.splitlines():
        parts = line.split()
        if not parts:
            continue
        if parts[0] == "R":
            retired += 1
            if int(parts[4], 16):
                rd = int(parts[5], 16)
                registers[rd // 32][rd % 32] = int(parts[6], 16)
        elif parts[0] == "S":
            address, strobes, data = (int(x, 16) for x in parts[1:4])
            for n in range(4):
                if strobes >> n & 1:
                    memory[address + n] = data >> (8 * n) & 255
        elif parts[0] == "T":
            trap = tuple(int(x, 16) for x in parts[1:4])
        elif parts[0] == "C":
            cycles = int(parts[1])
    return registers, memory, retired, trap, cycles


def check(name, ram, out, executables, simulator, rows, engine):
    """Compare every selected schedule, retaining failures without skipping later fixtures."""
    path = out / name
    path.with_suffix(".bin").write_bytes(ram)
    path.with_suffix(".hex").write_text("\n".join(f"{x:02x}" for x in ram) + "\n")
    gold = json.loads(subprocess.run([str(simulator), "native", str(path.with_suffix(".bin"))],
                                     text=True, capture_output=True, check=True).stdout)
    summary = None
    schedules = (("verilator", 1, 0), ("verilator", 2, 7),
                 ("verilator", 0x8632, 7), ("iverilog", 0x8632, 7))
    for simulator_name, seed, latency in schedules:
        if engine not in ("all", simulator_name):
            continue
        label = f"{name} {simulator_name} seed={seed} latency={latency}"
        log_path = out / f"{name}-{simulator_name}-s{seed}.log"
        row = dict(fixture=name, engine=simulator_name, seed=seed, latency=latency, log=str(log_path))
        try:
            registers, memory, retired, trap, cycles = run_rtl(
                executables[simulator_name], path, ram, seed, latency, log_path)
            assert trap == (gold["pc"], gold["cause"], gold["value"]), (
                "trap", trap, (gold["pc"], gold["cause"], gold["value"]))
            assert retired == gold["retired_native"], ("retired", retired, gold["retired_native"])
            for bank in range(2):
                for reg, value in enumerate(registers[bank]):
                    if value is not None:
                        assert value == gold["registers"][bank][reg], (
                            "register", bank, reg, hex(value), hex(gold["registers"][bank][reg]))
            assert f"{fnv(memory):016x}" == gold["ram_fnv64"], (
                "memory", f"{fnv(memory):016x}", gold["ram_fnv64"])
            row.update(passed=True, retired=retired, cycles=cycles)
            if latency == 0:
                summary = (retired, cycles)
        except (RuntimeError, AssertionError, ValueError) as error:
            row.update(passed=False, error=str(error))
            print(f"FAIL {label}: {error}", flush=True)
        rows.append(row)
    return gold, *(summary or (None, None))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rtl", type=Path, required=True)
    parser.add_argument("--simulator", type=Path, required=True)
    parser.add_argument("--banks", type=int, choices=(1, 2), default=2)
    parser.add_argument("--only")
    parser.add_argument("--engine", choices=("all", "verilator", "iverilog"), default="all")
    parser.add_argument("--bench", action="store_true", help="only the hand-translated loops")
    args = parser.parse_args()
    out = args.rtl / "xlate"
    out.mkdir(exist_ok=True)
    executables = {"verilator": [args.rtl / f"b{args.banks}/obj/Vcore_tb"],
                   "iverilog": ["vvp", args.rtl / f"b{args.banks}/core.vvp"]}
    rows = []
    for name, ram in ([] if args.bench else cases()):
        if args.only and args.only not in name:
            continue
        start = len(rows)
        gold, retired, cycles = check(name, ram, out, executables, args.simulator, rows, args.engine)
        passed = sum(row["passed"] for row in rows[start:])
        print(f"{name}: {passed}/{len(rows) - start} schedules pass, ISS retired {gold['retired_native']}, "
              f"services {gold['registers'][0][26]}, stop pc={gold['pc']:x} cause={gold['cause']}", flush=True)
    # Cost of generated code alone: the difference between two iteration
    # counts removes setup, the reset sweep and the exit.
    for name, source, image in (benchmarks() if args.bench else []):
        if args.only and args.only not in name:
            continue
        points = []
        for iterations in (100, 300):
            gold, retired, cycles = check(f"bench-{name}-{iterations}", image(iterations), out,
                                          executables, args.simulator, rows, args.engine)
            assert gold["registers"][0][22] == 1 and gold["registers"][0][9] == 0, (name, gold["registers"][0])
            if cycles is not None:
                points.append((BUDGET - gold["registers"][0][20], retired, cycles))
        if len(points) != 2:
            continue
        x86, words, clocks = (points[1][n] - points[0][n] for n in range(3))
        print(f"bench {name}: {words / x86:.2f} native words and {clocks / x86:.2f} zero-wait clocks "
              f"per x86 instruction ({x86} x86, {words} words, {clocks} clocks)\n  {source}")
    (out / "results.json").write_text(json.dumps(rows, indent=2) + "\n")
    failures = sum(not row["passed"] for row in rows)
    print(f"{'FAIL' if failures else 'PASS'}: {len(rows) - failures}/{len(rows)} RTL runs")
    if failures:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
