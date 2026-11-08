#!/usr/bin/env python3
"""Run the matched TinyCore SoftMMU gate for r86 and tiny386.

The harness pins both emulators to one host CPU, uses the same BIOS, VGA BIOS,
RAM size, source ISO, kernel command line, and bootlocal marker, and alternates
run order. r86 prepares the ISO in memory; the same preparation is dumped for
tiny386 so both guests consume identical primary ISO9660 boot files.

This is a machine-level benchmark, not a same-instruction trace. r86 exposes
its Deschutes CPUID profile while tiny386 uses its closest family-6 profile;
the report records that limitation explicitly.
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import pathlib
import pty
import resource
import selectors
import subprocess
import termios
import time
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from common import (finish_report, new_report, output_directory, save_report)

from tiny386_config import benchmark_config


MARKER = b"R86_BOOTLOCAL_OK"
CMDLINE = "console=ttyS0,115200n8 loglevel=3 nozswap nofstab"
BOOTLOCAL_COMMAND = "echo R86_BOOTLOCAL_OK"
DEFAULT_ENDPOINT = 1_975_715_720


def require_file(path: pathlib.Path, label: str) -> pathlib.Path:
    path = path.resolve()
    if not path.is_file():
        raise SystemExit(f"{label} not found: {path}")
    return path


def child_affinity(cpu: int):
    def apply() -> None:
        os.sched_setaffinity(0, {cpu})

    return apply


def child_tty_and_affinity(cpu: int, tty_fd: int):
    def apply() -> None:
        os.setsid()
        fcntl.ioctl(tty_fd, termios.TIOCSCTTY, 0)
        os.sched_setaffinity(0, {cpu})

    return apply


def child_cpu_seconds() -> float:
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    return usage.ru_utime + usage.ru_stime


def process_cpu_seconds(pid: int) -> float:
    raw = pathlib.Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    fields = raw[raw.rfind(")") + 2 :].split()
    ticks = int(fields[11]) + int(fields[12])
    return ticks / os.sysconf("SC_CLK_TCK")


def common_r86_args(
    r86: pathlib.Path,
    iso: pathlib.Path,
    bios: pathlib.Path,
    vga_bios: pathlib.Path,
) -> list[str]:
    return [
        str(r86),
        "--clock",
        "deterministic",
        "--iso",
        str(iso),
        "--firmware-display",
        "serial",
        "--bios",
        str(bios),
        "--vga-bios",
        str(vga_bios),
        "--mem-mib",
        "128",
        "--cmdline",
        CMDLINE,
        "--bootlocal-command",
        BOOTLOCAL_COMMAND,
    ]


def prepare_tiny386_iso(
    r86: pathlib.Path,
    source_iso: pathlib.Path,
    prepared_iso: pathlib.Path,
    bios: pathlib.Path,
    vga_bios: pathlib.Path,
    log_path: pathlib.Path,
) -> None:
    command = common_r86_args(r86, source_iso, bios, vga_bios)
    command.extend(["--dump-prepared-iso", str(prepared_iso), "--steps", "0"])
    with log_path.open("wb") as log:
        result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
    if result.returncode != 0 or not prepared_iso.is_file():
        raise RuntimeError(f"failed to prepare tiny386 ISO; see {log_path}")


def write_tiny386_config(
    path: pathlib.Path,
    prepared_iso: pathlib.Path,
    bios: pathlib.Path,
    vga_bios: pathlib.Path,
) -> None:
    path.write_text(
        benchmark_config(prepared_iso, bios, vga_bios, serial=True),
        encoding="utf-8",
    )


def run_r86(
    r86: pathlib.Path,
    source_iso: pathlib.Path,
    bios: pathlib.Path,
    vga_bios: pathlib.Path,
    endpoint: int,
    timeout_seconds: float,
    cpu: int,
    log_path: pathlib.Path,
) -> dict[str, float | int | str]:
    command = common_r86_args(r86, source_iso, bios, vga_bios)
    command.extend(["--steps", str(endpoint)])
    cpu_before = child_cpu_seconds()
    started = time.monotonic_ns()
    with log_path.open("wb") as log:
        result = subprocess.run(
            command,
            stdout=log,
            stderr=subprocess.STDOUT,
            preexec_fn=child_affinity(cpu),
            timeout=timeout_seconds,
        )
    elapsed = (time.monotonic_ns() - started) / 1_000_000_000
    cpu_elapsed = child_cpu_seconds() - cpu_before
    output = log_path.read_bytes()
    if result.returncode != 0 or MARKER not in output:
        raise RuntimeError(f"r86 missed the marker; see {log_path}")
    return {
        "engine": "r86",
        "command": command, "log": str(log_path),
        "elapsed_seconds": elapsed,
        "cpu_seconds": cpu_elapsed,
        "endpoint_steps": endpoint,
    }


def terminate(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()


def run_tiny386(
    tiny386: pathlib.Path,
    config: pathlib.Path,
    cpu: int,
    timeout_seconds: float,
    log_path: pathlib.Path,
) -> dict[str, float | str]:
    master_fd, slave_fd = pty.openpty()
    started = time.monotonic_ns()
    process = subprocess.Popen(
        [str(tiny386), str(config)],
        stdin=slave_fd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        preexec_fn=child_tty_and_affinity(cpu, slave_fd),
    )
    os.close(slave_fd)
    assert process.stdout is not None
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    observed = bytearray()
    deadline = time.monotonic() + timeout_seconds
    matched = False
    cpu_elapsed = 0.0
    wall_elapsed = 0.0
    try:
        with log_path.open("wb") as log:
            while time.monotonic() < deadline:
                for key, _ in selector.select(timeout=0.005):
                    chunk = os.read(key.fileobj.fileno(), 65536)
                    if not chunk:
                        continue
                    log.write(chunk)
                    observed.extend(chunk)
                    if len(observed) > 4096:
                        del observed[:-4096]
                    if MARKER in observed:
                        wall_elapsed = (time.monotonic_ns() - started) / 1_000_000_000
                        cpu_elapsed = process_cpu_seconds(process.pid)
                        matched = True
                        break
                if matched or process.poll() is not None:
                    break
    finally:
        terminate(process)
        selector.close()
        os.close(master_fd)
    if not matched:
        raise RuntimeError(f"tiny386 missed the marker; see {log_path}")
    return {
        "engine": "tiny386",
        "command": [str(tiny386), str(config)], "log": str(log_path),
        "elapsed_seconds": wall_elapsed,
        "cpu_seconds": cpu_elapsed,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--r86", type=pathlib.Path, default="target/release/r86-run")
    parser.add_argument(
        "--tiny386",
        type=pathlib.Path,
        default="refs/tiny386/tiny386_headless",
    )
    parser.add_argument("--iso", type=pathlib.Path, default="refs/TinyCore-current.iso")
    parser.add_argument("--bios", type=pathlib.Path, default="firmware/seabios/bios.bin")
    parser.add_argument(
        "--vga-bios",
        type=pathlib.Path,
        default="firmware/seabios/vgabios-stdvga.bin",
    )
    parser.add_argument("--cpu", type=int, default=1)
    parser.add_argument("--runs", type=int, default=2)
    parser.add_argument("--warmups", type=int, default=0)
    parser.add_argument("--endpoint", type=int, default=DEFAULT_ENDPOINT)
    parser.add_argument("--timeout", "--timeout-seconds", dest="timeout_seconds", type=float, default=120.0)
    parser.add_argument(
        "--output", "--out-dir", dest="out_dir",
        type=pathlib.Path,
        default="tmp/benchmarks/tinycore_boot/tiny386",
    )
    args = parser.parse_args()
    if args.runs < 1 or args.warmups < 0 or args.timeout_seconds <= 0 or args.endpoint < 1:
        parser.error("--runs must be positive and --warmups must be non-negative")

    r86 = require_file(args.r86, "r86 binary")
    tiny386 = require_file(args.tiny386, "tiny386 binary")
    source_iso = require_file(args.iso, "TinyCore ISO")
    bios = require_file(args.bios, "BIOS")
    vga_bios = require_file(args.vga_bios, "VGA BIOS")
    try:
        out_dir = output_directory(args.out_dir)
    except ValueError as error:
        parser.error(str(error))
    prepared_iso = out_dir / "TinyCore-prepared.iso"
    tiny_config = out_dir / "tiny386.ini"
    prepare_tiny386_iso(
        r86,
        source_iso,
        prepared_iso,
        bios,
        vga_bios,
        out_dir / "prepare.log",
    )
    write_tiny386_config(tiny_config, prepared_iso, bios, vga_bios)

    def one(emulator: str, label: str) -> dict[str, float | int | str]:
        log_path = out_dir / f"{label}-{emulator}.log"
        if emulator == "r86":
            return run_r86(
                r86,
                source_iso,
                bios,
                vga_bios,
                args.endpoint,
                args.timeout_seconds,
                args.cpu,
                log_path,
            )
        return run_tiny386(tiny386, tiny_config, args.cpu, args.timeout_seconds, log_path)

    identities = {"r86": r86, "tiny386": tiny386, "source_iso": source_iso,
                  "prepared_iso": prepared_iso, "bios": bios, "vga_bios": vga_bios}
    report = new_report("tinycore_boot", "firmware-tiny386", identities,
                        cpu=args.cpu, warmups=args.warmups, runs=args.runs, baseline="tiny386",
                        guest_clocks={"r86": "deterministic", "tiny386": "realtime"})
    report.update(endpoint="bootlocal serial marker / calibrated r86 step boundary",
                  endpoint_steps=args.endpoint, mem_mib=128, cmdline=CMDLINE,
                  bootlocal_command=BOOTLOCAL_COMMAND,
                  cpu_profiles={"r86": "Deschutes family 6 model 5", "tiny386": "gen=6, fpu=1"})
    save_report(report, out_dir)
    for index in range(args.warmups + args.runs):
        warmup = index < args.warmups
        order = ("r86", "tiny386") if index % 2 == 0 else ("tiny386", "r86")
        for emulator in order:
            sample = one(emulator, f"{index:02d}")
            sample.update(round=index, warmup=warmup)
            report["runs"].append(sample)
            save_report(report, out_dir)
            print(json.dumps(sample), flush=True)
    finish_report(report, out_dir)
    print(out_dir / "results.md")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
