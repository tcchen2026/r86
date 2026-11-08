#!/usr/bin/env python3
"""Prepare and identify the identical TinyCore firmware ISO used by both engines.

This runs outside the timing window. The primary ISO boot files contain the
serial command line and R86_BOOTLOCAL_OK bootlocal marker; the source ISO is
preserved. Transfer the resulting ISO and preparation.json to the test host.
"""

import argparse
import json
from pathlib import Path

from run_tiny386 import common_r86_args, prepare_tiny386_iso
from common import output_directory, sha256


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--r86", type=Path, default="target/release/r86-run")
    parser.add_argument("--iso", type=Path, default="refs/TinyCore-current.iso")
    parser.add_argument("--bios", type=Path, default="firmware/seabios/bios.bin")
    parser.add_argument("--vga-bios", type=Path, default="firmware/seabios/vgabios-stdvga.bin")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    paths = {name: getattr(args, name).resolve(strict=True)
             for name in ("r86", "iso", "bios", "vga_bios")}
    try:
        output = output_directory(args.output)
    except ValueError as error:
        parser.error(str(error))
    prepared = output / "tinycore-marker.iso"
    prepare_tiny386_iso(paths["r86"], paths["iso"], prepared, paths["bios"],
                       paths["vga_bios"], output / "prepare.log")
    command = common_r86_args(paths["r86"], paths["iso"], paths["bios"], paths["vga_bios"])
    command += ["--dump-prepared-iso", str(prepared), "--steps", "0"]
    report = {"command": command,
              "inputs": {name: {"path": str(path), "sha256": sha256(path)}
                         for name, path in paths.items()},
              "prepared_iso": {"path": str(prepared), "sha256": sha256(prepared)}}
    (output / "preparation.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report["prepared_iso"]))


if __name__ == "__main__":
    main()
