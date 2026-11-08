#!/usr/bin/env python3
"""Compare ReactOS cold firmware boots on ARM64 with QEMU TCG or TCTI.

Build --example reactos_boot_benchmark with --features jit. Both engines boot
the unchanged ISO without input, with 128 MiB, the same BIOS/VGA ROM and full
frame observation every 2000 ms by default. The endpoint checks the language and
keyboard controls, Next and Cancel; preserve each matching PPM for inspection.
The r86 example runs continuously and reports its endpoint before writing the
frame. QEMU runs continuously and exposes full frames through QMP screendump.
The observation paths differ: record that limitation with timing results.
On the RPi, QMP screendumps every 500 ms substantially delayed QEMU's wall
endpoint versus 1000/2000 ms while CPU time stayed close. Use the lower-impact
cadence and report its endpoint quantization; frequent screenshots can create a
false wall-time win. Keep CPU time and preserved frames alongside every sample.

Process creation, asset loading and compilation are timed. Guest clocks default
to realtime. Deterministic SoftMMU/monitor runs are separate engine comparisons;
no icount or guest/image-specific execution shortcut is used. Each
run starts a new process, including warm-ups, and engine order alternates.
Profile builds are unsuitable for acceptance. Device and CPUID differences
remain; inspect screenshots, logs and guest steps before interpreting ratios.

QEMU defaults to native TCG. --qemu-backend tcti explicitly selects the no-JIT
comparison and requires CONFIG_TCG_THREADED_INTERPRETER in its build identity.
Use --engine interp for a pair of interpreter-only builds. Both observation
paths retain the same cadence and complete-dialog endpoint.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import re
import selectors
import socket
import subprocess
import tempfile
import time
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from common import (child_cpu, endpoint_cpu, finish_report, new_report,
                    output_directory, qemu_identity, save_report, sha256)
from endpoint import matching_frame

MARKER = re.compile(rb"REACTOS_LANGUAGE_DIALOG elapsed=([\d.]+) steps=(\d+) polls=(\d+)")


def qmp_read(stream):
    while True:
        line = stream.readline()
        if not line:
            raise RuntimeError("QMP closed")
        result = json.loads(line)
        if "error" in result:
            raise RuntimeError(f"QMP: {result}")
        if "return" in result or "QMP" in result:
            return result


def qmp(stream, command, **arguments):
    stream.write(json.dumps({"execute": command, "arguments": arguments}).encode() + b"\n")
    return qmp_read(stream)


def run(args, name: str, directory: Path) -> dict:
    directory.mkdir()
    frame = directory / "matched.ppm"
    log = directory / "emulator.log"
    with tempfile.TemporaryDirectory(prefix="r86-reactos-") as control:
        sock_path = str(Path(control) / "qmp")
        if name == "r86":
            # EL1 uses the same assets, observer cadence and external endpoint.
            # The fourth argument selects its monitor ELF rather than an engine.
            mode = str(args.monitor) if getattr(args, "monitor", None) else args.engine
            command = [str(args.r86), str(args.iso), str(args.bios), str(args.vga_bios),
                       mode, str(frame), str(int(args.timeout))]
            command.append(str(args.frame_ms))
            if not args.monitor:
                command.append(args.clock)
        else:
            command = [str(args.qemu), "-accel", "tcg,thread=single", "-machine", "pc",
                       "-cpu", "pentium2", "-smp", "1", "-m", "128", "-bios", str(args.bios),
                       "-nodefaults", "-device", f"VGA,romfile={args.vga_bios}",
                       "-drive", f"file={args.iso},if=ide,index=0,media=cdrom,readonly=on",
                       "-boot", "d", "-display", "none", "-monitor", "none", "-serial", "none",
                       "-qmp", f"unix:{sock_path},server=on,wait=off", "-no-reboot"]
        (directory / "command.json").write_text(json.dumps(command, indent=2) + "\n")
        result = {"command": command, "log": str(log), "frame": str(frame)}
        before = child_cpu()
        started = time.monotonic()
        with log.open("wb") as output:
            process = subprocess.Popen(
                command, stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE if name == "r86" else output, stderr=subprocess.STDOUT,
                start_new_session=True, preexec_fn=lambda: os.sched_setaffinity(0, {args.cpu}),
            )
            try:
                if name == "r86":
                    with selectors.DefaultSelector() as selector:
                        selector.register(process.stdout, selectors.EVENT_READ)
                        tail = b""
                        while time.monotonic() - started < args.timeout:
                            if not selector.select(1):
                                continue
                            data = os.read(process.stdout.fileno(), 65536)
                            if not data:
                                break
                            output.write(data)
                            output.flush()
                            tail = (tail + data)[-65536:]
                            match = MARKER.search(tail)
                            if match and "elapsed_seconds" not in result:
                                result.update(elapsed_seconds=time.monotonic() - started,
                                              cpu_seconds=endpoint_cpu(process.pid),
                                              steps=int(match[2]), polls=int(match[3]),
                                              internal_elapsed_seconds=float(match[1]))
                    if "elapsed_seconds" not in result:
                        raise RuntimeError(f"r86 endpoint missing; inspect {log}")
                    if process.wait(timeout=5) != 0 or not matching_frame(frame):
                        raise RuntimeError(f"r86 final frame/run failed; inspect {directory}")
                else:
                    with socket.socket(socket.AF_UNIX) as sock:
                        while True:
                            try:
                                sock.connect(sock_path)
                                break
                            except (FileNotFoundError, ConnectionRefusedError):
                                if process.poll() is not None or time.monotonic() - started > 15:
                                    raise RuntimeError(f"QEMU control unavailable; inspect {log}")
                                time.sleep(0.01)
                        sock.settimeout(15)
                        with sock.makefile("rwb", buffering=0) as stream:
                            qmp_read(stream)
                            qmp(stream, "qmp_capabilities")
                            polls = 0
                            last_frame = started
                            while time.monotonic() - started < args.timeout:
                                time.sleep(max(0, args.frame_ms / 1000 - (time.monotonic() - last_frame)))
                                last_frame = time.monotonic()
                                qmp(stream, "screendump", filename=str(frame))
                                polls += 1
                                if matching_frame(frame):
                                    result.update(elapsed_seconds=time.monotonic() - started,
                                                  cpu_seconds=endpoint_cpu(process.pid), polls=polls)
                                    break
                    if "elapsed_seconds" not in result:
                        raise RuntimeError(f"QEMU endpoint missing; inspect {directory}")
            finally:
                if process.poll() is None:
                    process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                if process.stdout:
                    process.stdout.close()
        result.update(cpu_total_seconds=child_cpu() - before, frame_sha256=sha256(frame))
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("iso", "bios", "vga-bios"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--r86", type=Path)
    parser.add_argument("--monitor", type=Path,
                        help="EL1 monitor ELF; --r86 must be reactos_hard_benchmark")
    parser.add_argument("--qemu", type=Path)
    parser.add_argument("--qemu-backend", choices=["native", "tcti"], default="native")
    parser.add_argument("--output", "--out-dir", type=Path, required=True)
    parser.add_argument("--engine", choices=["interp", "jit"], default="jit")
    parser.add_argument("--clock", choices=["realtime", "deterministic"], default="realtime")
    parser.add_argument("--only", choices=["r86", "qemu"])
    parser.add_argument("--cpu", type=int, default=3)
    parser.add_argument("--warmups", type=int, default=0)
    parser.add_argument("--runs", type=int, default=2)
    parser.add_argument("--timeout", type=float, default=600)
    parser.add_argument("--frame-ms", type=int, default=2000)
    args = parser.parse_args()
    if (args.engine == "jit" or args.only != "r86") and platform.machine() != "aarch64":
        parser.error("JIT and QEMU comparison require an ARM64 host")
    if "R86_BENCH_STEPS" in os.environ or "R86_BENCH_STATE" in os.environ:
        parser.error("fixed-step checkpoints are not timing samples; run the Rust observer directly")
    if args.monitor and args.clock != "deterministic":
        parser.error("the no_std monitor requires --clock deterministic")
    if args.only == "qemu" and args.clock != "realtime":
        parser.error("QEMU uses realtime clocks (no icount)")
    if args.monitor and (args.engine != "jit" or args.only == "qemu"):
        parser.error("--monitor requires the r86 JIT engine")
    if args.runs < 1 or args.warmups < 0 or args.timeout < 1 or args.frame_ms < 1:
        parser.error("invalid run count, timeout, or frame interval")
    engines = (args.only,) if args.only else ("r86", "qemu")
    for engine in engines:
        if getattr(args, engine) is None:
            parser.error(f"--{engine} is required for the selected profile")
    names = ("iso", "bios", "vga_bios") + engines
    if args.monitor:
        names += ("monitor",)
    paths = {name: getattr(args, name).resolve(strict=True) for name in names}
    for name, path in paths.items():
        setattr(args, name, path)
    try:
        qemu_info = qemu_identity(args.qemu, args.qemu_backend) if "qemu" in engines else {}
        args.output = output_directory(args.output)
    except ValueError as error:
        parser.error(str(error))
    report = new_report("reactos_livecd_boot_to_langsel",
                        f"headless-{args.only}-only" if args.only else "headless-qemu", paths,
                        cpu=args.cpu, warmups=args.warmups, runs=args.runs,
                        candidate=args.only or "r86", baseline=None if args.only else "qemu",
                        guest_clocks={name: args.clock if name == "r86" else "realtime"
                                      for name in engines})
    report.update(engine=args.engine, mem_mib=128,
                  endpoint="complete language dialog including Next and Cancel",
                  observation_ms=args.frame_ms, **qemu_info)
    save_report(report, args.output)
    for index in range(args.warmups + args.runs):
        order = (args.only,) if args.only else (("r86", "qemu") if index % 2 == 0 else ("qemu", "r86"))
        for name in order:
            print(f"starting round={index} engine={name}", flush=True)
            result = run(args, name, args.output / f"{index:02d}-{name}")
            result.update(engine=name, round=index, warmup=index < args.warmups)
            report["runs"].append(result)
            save_report(report, args.output)
            print(json.dumps(result), flush=True)
    finish_report(report, args.output)
    print(json.dumps({key: report[key] for key in ("medians", "ratios") if key in report}), flush=True)


if __name__ == "__main__":
    main()
