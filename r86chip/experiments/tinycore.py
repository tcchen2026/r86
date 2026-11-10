#!/usr/bin/env python3
"""Run the shared x86 Machine as RV32IM firmware with large simulated RAM.

--engine iss uses the Rust RV instruction simulator; --engine rtl uses the
generated Chisel B1 core under Verilator. No host x86 execution callbacks.
Host-side loading/decompression sets up a direct-kernel boot, not BIOS boot.
A missing guest boot marker returns 2; a timeout is never reported as a pass.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import struct
import subprocess
import sys
import lua_assets
import compare_machine

ROOT = Path(__file__).resolve().parents[2]
RTL = ROOT / "r86chip/rtl"
FIRMWARE = ROOT / "r86chip/firmware"


def command(args, *, cwd=ROOT, env=None):
    subprocess.run([str(a) for a in args], cwd=cwd, env=env, check=True)


def flatten_elf(path):
    blob = path.read_bytes()
    if blob[:7] != b"\x7fELF\x01\x01\x01" or struct.unpack_from("<H", blob, 18)[0] != 243:
        raise ValueError("firmware must be a little-endian RV32 ELF")
    entry, phoff = struct.unpack_from("<II", blob, 24)
    phsize, phnum = struct.unpack_from("<HH", blob, 42)
    if entry != 0x1000 or phsize != 32 or phoff + phnum * phsize > len(blob):
        raise ValueError("invalid firmware ELF entry/program headers")
    chunks = []
    for n in range(phnum):
        kind, offset, va, pa, filesz, memsz, _, _ = struct.unpack_from("<8I", blob, phoff+n*phsize)
        if kind == 1:
            if not (0x1000 <= pa == va and filesz <= memsz and pa+memsz <= 0x800000
                    and offset+filesz <= len(blob)):
                raise ValueError("firmware segment outside assigned native RAM")
            if any(pa < a+size and a < pa+memsz for a, size, _ in chunks):
                raise ValueError("overlapping firmware segments")
            chunks.append((pa, memsz, blob[offset:offset+filesz]))
    if not chunks:
        raise ValueError("firmware has no loadable segment")
    image = bytearray(max(a+size for a, size, _ in chunks)-0x1000)
    for address, _, contents in chunks:
        image[address-0x1000:address-0x1000+len(contents)] = contents
    return image


def finish(args, runner):
    command(runner)
    report = json.loads((args.out / f"{args.engine}-run.json").read_text())
    metadata = json.loads((args.out / f"{args.engine}-inputs.json").read_text())
    passed = report["marker"] and report["status"] == 3
    if metadata.get("jit"):
        passed = passed and report.get("jit_native_steps", 0) > 0 and report.get("jit_invocations", 0) > 0
        if metadata.get("lua"):
            passed = passed and report.get("jit_user_steps", 0) > 0
    if metadata.get("extensions"):
        passed = passed and report.get("chip_extensions") == 1
    if passed and metadata.get("reference"):
        comparison = compare_machine.compare(args.out, args.engine)
        if not comparison["passed"]:
            print("FAIL: reference comparison: " + json.dumps(comparison))
            return 1
    print(("PASS" if passed else "INCOMPLETE") + ": " + json.dumps(report))
    return 0 if passed else 2


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--iso", type=Path, default=ROOT / "refs/TinyCore-current.iso")
    parser.add_argument("--out", type=Path, default=ROOT / "r86chip/experiments/out/tinycore")
    parser.add_argument("--engine", choices=("iss", "rtl"), default="iss")
    parser.add_argument("--seconds", type=float, default=900)
    parser.add_argument("--rv-steps", type=int, default=300_000_000_000)
    parser.add_argument("--cycles", type=int, default=1_000_000_000_000)
    parser.add_argument("--x86-steps", type=int, default=1_300_000_000)
    parser.add_argument("--lua", action="store_true", help="require the guest Lua arithmetic, coroutine and file-I/O workload")
    parser.add_argument("--jit", action="store_true", help="enable the experimental same-core B1 translator")
    parser.add_argument("--extensions", action="store_true", help="compile the same interpreter with chip EFLAGS/TLB primitives")
    parser.add_argument("--rf-banks", type=int, choices=(1, 2), help="RTL RF banks (default: 1; the B1 JIT requires 2)")
    parser.add_argument("--boundaries", action="store_true", help="record guest state at firmware observation boundaries for differential debugging")
    parser.add_argument("--reference", action="store_true", help="also execute a separately labelled host reference")
    preserved = parser.add_mutually_exclusive_group()
    preserved.add_argument("--resume", action="store_true", help="resume the RTL checkpoint with identical inputs")
    preserved.add_argument("--prepared", action="store_true", help="run the preserved RTL executable and boot images from reset, without rebuilding")
    args = parser.parse_args()
    if args.extensions and args.jit:
        parser.error("--extensions and --jit select separate build experiments")
    if args.rf_banks is None:
        args.rf_banks = 2 if args.jit else 1
    if args.jit and args.rf_banks != 2:
        parser.error("the separate B1 JIT requires two RF banks")
    args.out = args.out.resolve()
    args.iso = args.iso.resolve()
    if (args.resume or args.prepared) and args.engine != "rtl":
        parser.error("--resume/--prepared require --engine rtl")
    if not math.isfinite(args.seconds) or min(args.seconds, args.rv_steps, args.cycles, args.x86_steps) <= 0:
        parser.error("limits must be positive")
    if args.resume or args.prepared:
        # Preserved runs retain their original executable and boot inputs;
        # rebuilding could destroy the matching firmware during other work.
        metadata = json.loads((args.out / "rtl-inputs.json").read_text())
        for path, expected in metadata["hashes"].items():
            if hashlib.sha256(Path(path).read_bytes()).hexdigest() != expected:
                raise ValueError(f"preserved input changed: {path}")
        executable = args.out / "obj/Vr86_core"
        expected = metadata.get("simulation_executable_sha256")
        if expected and hashlib.sha256(executable.read_bytes()).hexdigest() != expected:
            raise ValueError("preserved simulation executable changed")
        runner = [executable, args.out, args.cycles, args.seconds]
        if args.resume:
            runner.append("--resume")
        return finish(args, runner)
    args.out.mkdir(parents=True, exist_ok=True)
    features = "machine,r86chip" + (",r86chip-native" if args.extensions else "")
    command(["cargo", "rustc", "--release", "--no-default-features", "--features", features,
             "--bin", "r86chip-machine", "--target", "riscv32im-unknown-none-elf", "--", "-C",
             f"link-arg=-T{FIRMWARE / 'machine.ld'}", "-C", "link-arg=--no-relax", "-C", "relocation-model=static"])
    elf = ROOT / "target/riscv32im-unknown-none-elf/release/r86chip-machine"
    # Keep an immutable copy of the executable associated with these boot inputs.
    (args.out / "firmware.elf").write_bytes(elf.read_bytes())
    (args.out / "firmware.bin").write_bytes(flatten_elf(elf))
    command(["cargo", "build", "--release", "--bin", "r86chip-machine"])
    prepare = [ROOT / "target/release/r86chip-machine", args.iso, args.out, args.x86_steps]
    if args.lua:
        prepare.extend(["--lua-tree", lua_assets.prepare(ROOT / "r86chip/experiments/out/lua-assets")])
    if args.reference:
        prepare.append("--reference")
    if args.jit:
        prepare.append("--jit")
    if args.boundaries:
        prepare.append("--boundaries")
    command(prepare)
    inputs = [args.iso, args.out / "firmware.elf", args.out / "guest.ram", args.out / "config.bin"]
    metadata = dict(engine=args.engine, native_ram_mib=512, x86_ram_mib=128,
                    rf_banks=args.rf_banks,
                    lua=args.lua, jit=args.jit, extensions=args.extensions, boundaries=args.boundaries, reference=args.reference,
                    lua_packages=lua_assets.PACKAGES if args.lua else {},
                    boot="host-decompressed direct kernel; RV32IM/B1 firmware",
                    limits=dict(seconds=args.seconds, rv_steps=args.rv_steps,
                                cycles=args.cycles, x86_steps=args.x86_steps),
                    hashes={str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in inputs})
    (args.out / f"{args.engine}-inputs.json").write_text(json.dumps(metadata, indent=2) + "\n")
    if args.engine == "iss":
        command(["cargo", "build", "--release", "--no-default-features", "--features", "r86chip",
                 "--bin", "r86chip-sim"])
        runner = [ROOT / "target/release/r86chip-sim", "machine", args.out / "firmware.elf",
                  args.out, args.rv_steps, args.seconds]
    else:
        env = os.environ.copy()
        env.pop("CHISEL_FIRTOOL_PATH", None)
        emitted = args.out / "generated"
        command(["sbt", "-batch", f"runMain r86chip.Generate {args.rf_banks} {emitted}"], cwd=RTL, env=env)
        command(["verilator", "--cc", "--exe", "--build", "--savable", "--top-module", "r86_core",
                 "-O3", "-CFLAGS", "-O3 -std=c++17", "-j", "2", "--Mdir", args.out / "obj",
                 emitted / "r86_core.sv", emitted / "MulDiv.sv", RTL / "r86_regfile.v",
                 RTL / "tests/machine_tb.cpp"])
        runner = [args.out / "obj/Vr86_core", args.out, args.cycles, args.seconds]
    return finish(args, runner)


if __name__ == "__main__":
    sys.exit(main())
