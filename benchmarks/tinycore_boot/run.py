#!/usr/bin/env python3
"""Compare cold TinyCore firmware boots against qemu-utm's ARM64 TCG JIT.

Run on the RPi. Supply one prepared ISO containing a bootlocal serial marker and
use the identical BIOS, VGA ROM, RAM size and command line for both emulators.
The producer must record how the ISO was prepared; this harness never rewrites
it or substitutes direct-kernel/host-decompressed boot. QEMU uses one Pentium II
vCPU and native TCG, not TCI, TCTI, KVM or icount. The machines are not identical
device/CPUID implementations, so retain full serial logs and inspect differing
guest behavior before attributing a result to JIT efficiency.

Timing starts before process creation and ends when the marker is received.
CPU time is sampled from /proc at that endpoint; total child CPU through cleanup
is also reported. Runs alternate order, each starts with an empty process/code
cache. Guest devices use realtime by default; deterministic runs are a separate
engine comparison. Defaults are two measured runs per engine, without
warm-ups. Counters and tracing must be collected separately. A faster timing does not by itself
prove native coverage, architectural correctness or completion of the JIT goal.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import platform
import selectors
import subprocess
import time
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from common import (child_cpu, endpoint_cpu, finish_report, new_report,
                    output_directory, qemu_identity, save_report)

CMDLINE = "console=ttyS0,115200n8 loglevel=3 nozswap nofstab"


def run(command: list[str], cpu: int, marker: bytes, log: pathlib.Path,
        timeout: float) -> dict:
    before = child_cpu()
    start = time.monotonic()
    process = subprocess.Popen(
        command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, start_new_session=True,
        preexec_fn=lambda: os.sched_setaffinity(0, {cpu}),
    )
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    tail = b""
    reached = False
    cpu_at_marker = None
    elapsed = None
    try:
        with log.open("wb") as output:
            while time.monotonic() - start < timeout:
                events = selector.select(min(1.0, max(0.0, timeout - (time.monotonic() - start))))
                if not events:
                    if process.poll() is not None:
                        break
                    continue
                data = os.read(process.stdout.fileno(), 65536)
                if not data:
                    break
                output.write(data)
                tail = (tail + data)[-max(65536, len(marker) * 2):]
                if marker in tail:
                    elapsed = time.monotonic() - start
                    cpu_at_marker = endpoint_cpu(process.pid)
                    reached = True
                    break
    finally:
        selector.close()
        if process.poll() is None:
            process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        process.stdout.close()
    total_cpu = child_cpu() - before
    if not reached:
        raise RuntimeError(f"boot marker missing after {time.monotonic() - start:.3f}s; inspect {log}")
    return {
        "elapsed_seconds": elapsed,
        "cpu_seconds": cpu_at_marker,
        "cpu_sample_source": "proc-at-marker",
        "cpu_total_seconds": total_cpu,
        "exit_seconds": time.monotonic() - start,
        "log": str(log), "command": command,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--r86", type=pathlib.Path, required=True)
    parser.add_argument("--qemu", type=pathlib.Path)
    parser.add_argument("--iso", type=pathlib.Path, required=True)
    parser.add_argument("--bios", type=pathlib.Path, required=True)
    parser.add_argument("--vga-bios", type=pathlib.Path, required=True)
    parser.add_argument("--output", "--out-dir", type=pathlib.Path, required=True)
    parser.add_argument("--engine", choices=["interp", "jit"], default="jit")
    parser.add_argument("--clock", choices=["realtime", "deterministic"], default="realtime")
    parser.add_argument("--only", choices=["r86"])
    parser.add_argument("--cpu", type=int, default=3)
    parser.add_argument("--mem-mib", type=int, default=128)
    parser.add_argument("--warmups", type=int, default=0)
    parser.add_argument("--runs", type=int, default=2)
    parser.add_argument("--timeout", type=float, default=300)
    parser.add_argument("--marker", default="R86_BOOTLOCAL_OK")
    args = parser.parse_args()
    if (args.engine == "jit" or not args.only) and platform.machine() != "aarch64":
        parser.error("JIT and native QEMU comparison require an ARM64 host")
    if args.runs < 1 or args.warmups < 0 or args.timeout <= 0 or not args.marker or args.mem_mib < 1:
        parser.error("invalid run count, timeout, RAM size, or marker")
    if not args.only and args.qemu is None:
        parser.error("--qemu is required unless --only r86 is selected")
    names = ("r86", "iso", "bios", "vga_bios") + (() if args.only else ("qemu",))
    paths = {name: getattr(args, name).resolve(strict=True) for name in names}
    for name, path in paths.items():
        setattr(args, name, path)
    try:
        qemu_info = {} if args.only else qemu_identity(args.qemu)
        args.output = output_directory(args.output)
    except ValueError as error:
        parser.error(str(error))
    r86 = [str(args.r86), "--firmware-display", "serial",
           "--iso", str(args.iso), "--bios", str(args.bios), "--vga-bios", str(args.vga_bios),
           "--mem-mib", str(args.mem_mib), "--cmdline", CMDLINE,
           "--steps", "10000000000", "--expect-serial", args.marker,
           "--cpu-engine", args.engine, "--clock", args.clock]
    commands = {"r86": r86}
    if not args.only:
        commands["qemu"] = [str(args.qemu), "-accel", "tcg,thread=single", "-machine", "pc",
            "-cpu", "pentium2", "-smp", "1", "-m", str(args.mem_mib),
            "-bios", str(args.bios), "-nodefaults", "-device", f"VGA,romfile={args.vga_bios}",
            "-drive", f"file={args.iso},if=ide,index=0,media=cdrom,readonly=on",
            "-boot", "d", "-display", "none", "-monitor", "none", "-serial", "stdio", "-no-reboot"]
    report = new_report("tinycore_boot", "firmware-r86" if args.only else "firmware-qemu",
                        paths, cpu=args.cpu, warmups=args.warmups, runs=args.runs,
                        baseline=None if args.only else "qemu",
                        guest_clocks={"r86": args.clock, **({} if args.only else {"qemu": "realtime"})})
    report.update(engine=args.engine, mem_mib=args.mem_mib, cmdline=CMDLINE,
                  marker=args.marker, endpoint="serial marker", **qemu_info)
    save_report(report, args.output)
    for index in range(args.warmups + args.runs):
        order = ("r86",) if args.only else (("r86", "qemu") if index % 2 == 0 else ("qemu", "r86"))
        for name in order:
            result = run(commands[name], args.cpu, args.marker.encode(),
                         args.output / f"{index:02d}-{name}.log", args.timeout)
            result.update(engine=name, warmup=index < args.warmups, round=index)
            report["runs"].append(result)
            save_report(report, args.output)
            print(json.dumps({key: result[key] for key in
                             ("engine", "round", "warmup", "elapsed_seconds", "cpu_seconds")}), flush=True)
    finish_report(report, args.output)
    print(json.dumps({key: report[key] for key in ("medians", "ratios") if key in report}), flush=True)


if __name__ == "__main__":
    main()
