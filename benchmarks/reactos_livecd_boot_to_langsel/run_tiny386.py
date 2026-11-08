#!/usr/bin/env python3
"""Run the matched ReactOS language-dialog gate for r86 and tiny386."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import subprocess
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from common import (finish_report, new_report, output_directory, save_report, sha256)


SAMPLE_RE = re.compile(
    r"emulator=(?P<emulator>r86|tiny386) wall_ms=(?P<wall>\d+) "
    r"cpu_ms=(?P<cpu>\d+) poll=(?P<poll>\d+)"
)


def require_file(path: pathlib.Path, label: str) -> pathlib.Path:
    path = path.resolve()
    if not path.is_file():
        raise SystemExit(f"{label} not found: {path}")
    return path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--r86", type=pathlib.Path, default="target/release/r86-run")
    parser.add_argument("--tiny386", type=pathlib.Path, default="refs/tiny386/tiny386")
    parser.add_argument("--iso", type=pathlib.Path, default="refs/ReactOS.iso")
    parser.add_argument("--bios", type=pathlib.Path, default="firmware/seabios/bios.bin")
    parser.add_argument(
        "--vga-bios",
        type=pathlib.Path,
        default="firmware/seabios/vgabios-stdvga.bin",
    )
    parser.add_argument("--runs", type=int, default=2)
    parser.add_argument("--warmups", type=int, default=0)
    parser.add_argument("--display", default=":99")
    parser.add_argument("--cpu", type=int, default=1)
    parser.add_argument("--xvfb-cpu", type=int, default=2)
    parser.add_argument("--output", "--out-dir", dest="out_dir", type=pathlib.Path,
                        default="tmp/benchmarks/reactos_livecd_boot_to_langsel/tiny386")
    parser.add_argument("--timeout", type=int, default=600)
    args = parser.parse_args()
    if args.runs < 1 or args.warmups < 0 or args.timeout < 1:
        parser.error("--runs must be positive and --warmups must be non-negative")

    r86 = require_file(args.r86, "r86 binary")
    tiny386 = require_file(args.tiny386, "tiny386 binary")
    iso = require_file(args.iso, "ReactOS ISO")
    bios = require_file(args.bios, "BIOS")
    vga_bios = require_file(args.vga_bios, "VGA BIOS")
    shell = require_file(pathlib.Path(__file__).with_name("run_sdl.sh"), "runner")
    try:
        out_dir = output_directory(args.out_dir)
    except ValueError as error:
        parser.error(str(error))

    def one(emulator: str, label: str) -> dict[str, float | int | str]:
        sample_dir = out_dir / f"{label}-{emulator}"
        env = os.environ.copy()
        env.update(
            {
                "R86_BINARY": str(r86),
                "TINY386_BINARY": str(tiny386),
                "REACTOS_ISO": str(iso),
                "BIOS": str(bios),
                "VGA_BIOS": str(vga_bios),
                "DISPLAY_NUMBER": args.display,
                "R86_CPU": str(args.cpu),
                "TINY386_CPU": str(args.cpu),
                "XVFB_CPU": str(args.xvfb_cpu),
                "OUT_DIR": str(sample_dir),
                "TIMEOUT_SECONDS": str(args.timeout),
                "BENCH_R86": "1" if emulator == "r86" else "0",
                "BENCH_TINY386": "1" if emulator == "tiny386" else "0",
            }
        )
        sample_dir.mkdir()
        observer_log = sample_dir / "observer.log"
        with observer_log.open("w") as output:
            result = subprocess.run(
                [str(shell)], env=env, text=True,
                stdout=output, stderr=subprocess.STDOUT,
            )
        observed = observer_log.read_text()
        if result.returncode != 0:
            raise RuntimeError(f"{emulator} observer exited {result.returncode}; inspect {observer_log}")
        match = SAMPLE_RE.search(observed)
        if match is None or match.group("emulator") != emulator:
            raise RuntimeError(f"missing {emulator} sample; inspect {observer_log}")
        frame = sample_dir / f"{emulator}-matched.png"
        return {
            "engine": emulator,
            "command": [str(shell)], "environment": {key: env[key] for key in (
                "R86_BINARY", "TINY386_BINARY", "REACTOS_ISO", "BIOS", "VGA_BIOS",
                "DISPLAY_NUMBER", "R86_CPU", "TINY386_CPU", "XVFB_CPU", "OUT_DIR",
                "TIMEOUT_SECONDS", "BENCH_R86", "BENCH_TINY386")},
            "log": str(sample_dir / f"{emulator}.log"), "frame": str(frame),
            "observer_log": str(observer_log),
            "elapsed_seconds": int(match.group("wall")) / 1000,
            "cpu_seconds": int(match.group("cpu")) / 1000,
            "polls": int(match.group("poll")),
            "frame_sha256": sha256(frame),
        }

    paths = {"r86": r86, "tiny386": tiny386, "iso": iso, "bios": bios, "vga_bios": vga_bios}
    report = new_report("reactos_livecd_boot_to_langsel", "sdl-tiny386", paths,
                        cpu=args.cpu, warmups=args.warmups, runs=args.runs, baseline="tiny386",
                        guest_clocks={"r86": "deterministic", "tiny386": "realtime"})
    report.update(endpoint="complete language dialog including Next and Cancel",
                  mem_mib=128, xvfb_cpu=args.xvfb_cpu, observation_sleep_ms=250)
    save_report(report, out_dir)
    for index in range(args.warmups + args.runs):
        order = ("r86", "tiny386") if index % 2 == 0 else ("tiny386", "r86")
        for emulator in order:
            sample = one(emulator, f"{index:02d}")
            sample.update(round=index, warmup=index < args.warmups)
            report["runs"].append(sample)
            save_report(report, out_dir)
            print(json.dumps(sample), flush=True)
    finish_report(report, out_dir)
    print(out_dir / "results.md")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
