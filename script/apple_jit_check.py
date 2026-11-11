#!/usr/bin/env python3
"""Intern/Lead Mac CI gate: first-party protected-mode ROM, both engines.

No OS image or disk is needed. The guest checks paging/PCI framebuffer effects
and prints RGBK before HLT. Native counts prevent an interpreter-only JIT pass.
The checked-in frame hash is established by the shared interpreter. Actual
MAP_JIT/protection and Apple PMU evidence comes only from the Mac runner.
"""
import argparse
from pathlib import Path
import re
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--require-host-counters", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    with tempfile.TemporaryDirectory(prefix="r86-apple-jit-") as work:
        rom = Path(work) / "guest.bin"
        subprocess.run(["nasm", "-f", "bin", str(root / "tests/roms/wasm_jit_lfb.asm"),
                        "-o", str(rom)], check=True)
        for engine in ("interp", "jit"):
            command = [str(args.binary.resolve()), "--bios", str(rom), "--no-vga-bios", "--no-cdrom",
                       "--mem-mib", "16", "--clock", "deterministic", "--steps", "10000000",
                       "--expect-frame-hash", FRAME_HASH]
            command += ["--no-jit"] if engine == "interp" else ["--cpu-engine", "jit"]
            result = subprocess.run(command, text=True, stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, timeout=180)
            print(f"{engine}:\n{result.stdout}", flush=True)
            result.check_returncode()
            assert "RGBK" in result.stdout, "guest assertions failed"
            if engine == "jit":
                native = re.search(r"native_steps: (\d+)", result.stdout)
                assert native and int(native[1]) > 3_600_000, "guest must execute native loops"
                if sys.platform == "darwin":
                    assert "jit protection:" in result.stdout, "Mac write/execute report missing"
            if args.require_host_counters:
                counters = re.search(r"host counters: .*instructions=(\d+) cycles=(\d+)", result.stdout)
                assert counters and min(map(int, counters.groups())) > 0, (
                    "Apple PMU counters unavailable; a virtualized runner may return zero. "
                    "This gate needs evidence, not substituted estimates.")


# Set from the interpreter run of the exact ROM above; both engines check it.
FRAME_HASH = "1fe40c340900687b5bcad3a5ee58672c0e9fccf140a31c8aa01774e73c2e997a"

if __name__ == "__main__":
    main()
