#!/usr/bin/env python3
"""Build the vendored SeaBIOS ROMs without modifying the source tree.

Requires GNU make, GCC with x86 -m32/-m16 support, binutils, patch and Python 3.
The default output is firmware/seabios; --output-dir allows a comparison build.
"""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parent.parent
VERSION = "rel-1.17.0"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, default=ROOT / "firmware/seabios")
    parser.add_argument("--jobs", type=int, default=min(os.cpu_count() or 1, 8))
    args = parser.parse_args()
    if args.jobs < 1:
        parser.error("--jobs must be positive")
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    hashes = []
    (ROOT / "tmp").mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="seabios-build-", dir=ROOT / "tmp") as work:
        source = Path(work) / "seabios"
        shutil.copytree(ROOT / "vendor/seabios", source)
        subprocess.run(
            ["patch", "-p1", "--batch"], cwd=source,
            input=(ROOT / "firmware/seabios/acpi-features.patch").read_bytes(),
            check=True,
        )
        # SeaBIOS's release version mechanism suppresses host/time suffixes.
        (source / ".version").write_text(VERSION + "\n", encoding="ascii")
        # The handheld VGA BIOS adds the CardputerZero's 320x170 VBE mode; it is
        # built last so the patch never reaches the standard ROMs.
        for config, target, filename, patch in (
            ("bios", "bios.bin", "bios.bin", None),
            ("vga", "vgabios.bin", "vgabios-stdvga.bin", None),
            ("vga", "vgabios.bin", "vgabios-handheld.bin", "handheld-modes.patch"),
        ):
            if patch:
                subprocess.run(
                    ["patch", "-p1", "--batch"], cwd=source,
                    input=(ROOT / "firmware/seabios" / patch).read_bytes(),
                    check=True,
                )
            name = Path(filename).stem
            build = source / f"out-{name}"
            build.mkdir()
            config_path = build / ".config"
            shutil.copyfile(ROOT / f"firmware/seabios/{config}.config", config_path)
            command = [
                "make", "-C", str(source), f"-j{args.jobs}",
                f"OUT=out-{name}/", f"KCONFIG_CONFIG={config_path}",
                "PYTHON=python3", f"EXTRAVERSION=-r86-{2 if config == 'bios' else 1}",
            ]
            subprocess.run(command + ["olddefconfig"], check=True)
            subprocess.run(command + [f"out-{name}/{target}"], check=True)
            rom = build / target
            shutil.copyfile(rom, output / filename)
            digest = hashlib.sha256(rom.read_bytes()).hexdigest()
            hashes.append(f"{digest}  {filename}\n")
    (output / "SHA256SUMS").write_text("".join(hashes), encoding="ascii")
    print("".join(hashes), end="")


if __name__ == "__main__":
    main()
