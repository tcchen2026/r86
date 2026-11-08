#!/usr/bin/env python3
"""Measure linked interpreter text, including compiler-generated shared blocks.

Rust module symbols alone undercount LLVM's OUTLINED_FUNCTION_* bodies, notably
with opt-level=z. Follow their direct calls transitively from interpreter roots.
Aliases at the same address are counted once; separately linked MMU/device and
compiler runtime code is reported outside the interpreter body. No source file
is moved or excluded to satisfy the budget. Inlined helpers belong to their
containing function and therefore remain counted.

The conservative core-body count also includes CPU-state and native-leaf
functions. Read-only tables are reported separately as whole-firmware .rodata;
this measures executable text, not total firmware RAM/ROM consumption.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess


def measure(firmware):
    nm = subprocess.check_output(
        ["llvm-nm", "--print-size", "--demangle", str(firmware)], text=True)
    functions, named = {}, {}
    for line in nm.splitlines():
        match = re.fullmatch(r"([0-9a-f]+) ([0-9a-f]+) [Tt] (.+)", line)
        if not match:
            continue
        address, size, name = int(match[1], 16), int(match[2], 16), match[3]
        if size == 0:
            continue
        item = functions.setdefault(address, dict(size=size, names=[]))
        assert item["size"] == size, (address, item, size)
        item["names"].append(name)
        named[name] = address

    disassembly = subprocess.check_output(
        ["llvm-objdump", "-d", "--demangle", str(firmware)], text=True)
    calls, current = {}, None
    for line in disassembly.splitlines():
        header = re.fullmatch(r"([0-9a-f]+) <(.+)>:", line)
        if header:
            address = int(header[1], 16)
            current = address if address in functions else None
            continue
        for name in re.findall(r"<(OUTLINED_FUNCTION_\d+)(?:[+]0x[0-9a-f]+)?>", line):
            if current is not None:
                assert name in named, name
                calls.setdefault(current, set()).add(named[name])

    def roots(*fragments):
        # Older LLVM demanglers leave Rust trait paths escaped as r86..x86..
        # and a leading _$LT$. Include their bodies (including cold fault
        # formatting), not just directly named functions.
        return {address for address, item in functions.items()
                if any(fragment in name or fragment.replace("::", "..") in name
                       for name in item["names"]
                       for fragment in fragments)}

    def with_outlines(addresses):
        seen, pending = set(addresses), list(addresses)
        while pending:
            for child in calls.get(pending.pop(), ()):
                if child not in seen:
                    seen.add(child)
                    pending.append(child)
        return seen

    def size(addresses):
        return sum(functions[address]["size"] for address in addresses)

    interpreter = roots("r86::x86::interp::")
    if not interpreter:
        raise ValueError("ELF has no named interpreter functions; use an unstripped firmware")
    cpu = roots("r86::x86::cpu::")
    leaves = roots("r86::chipinterp::")
    owned = with_outlines(interpreter)
    core = with_outlines(interpreter | cpu | leaves)
    outlines = roots("OUTLINED_FUNCTION_")
    sections = subprocess.check_output(["llvm-size", "-A", str(firmware)], text=True)
    text_bytes = int(re.search(r"^\.text\s+(\d+)", sections, re.M)[1])
    rodata_bytes = int(re.search(r"^\.rodata\s+(\d+)", sections, re.M)[1])
    assert size(core) <= text_bytes
    return dict(
        scope=__doc__, firmware_sha256=hashlib.sha256(firmware.read_bytes()).hexdigest(),
        main_loop_bytes=size(roots("r86::x86::interp::Interpreter::run_batch_inner::")),
        interpreter_named_bytes=size(interpreter),
        interpreter_outline_bytes=size(owned - interpreter),
        interpreter_text_bytes=size(owned),
        cpu_named_bytes=size(cpu), native_leaf_named_bytes=size(leaves),
        conservative_core_body_bytes=size(core),
        mmu_named_bytes=size(roots("r86::x86::mmu::")),
        all_outlined_bytes=size(outlines),
        unattributed_text_bytes=text_bytes - size(set(functions)),
        firmware_text_bytes=text_bytes, firmware_rodata_bytes=rodata_bytes,
        outline_functions=[dict(address=f"0x{address:08x}", **functions[address])
                           for address in sorted(core & outlines)],
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("firmware", type=Path)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--budget-kib", type=int, default=256)
    parser.add_argument("--check", action="store_true", help="fail if conservative body exceeds budget")
    args = parser.parse_args()
    if args.budget_kib <= 0:
        parser.error("--budget-kib must be positive")
    report = measure(args.firmware)
    report["budget_bytes"] = args.budget_kib * 1024
    report["budget_passed"] = report["conservative_core_body_bytes"] <= report["budget_bytes"]
    if args.out:
        args.out.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k not in ("scope", "outline_functions")}, indent=2))
    if args.check and not report["budget_passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
