#!/usr/bin/env python3
"""Compare two ARM64 KVM monitor builds on the same TinyCore boot workload.

Run on the ARM64 host after copying the locally built binaries and ISO. Each
process is pinned to one CPU; order alternates over two measured pairs. Timing
includes the host VMM and all its threads. Fixed-step snapshots are available
for checking CPU, RAM, and device state before measuring full boots.
"""

import argparse
import json
import pathlib
import re
import resource
import subprocess
import time
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from common import (finish_report, new_report, output_directory, save_report, sha256)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runner", type=pathlib.Path, required=True)
    parser.add_argument("--candidate-runner", type=pathlib.Path)
    parser.add_argument("--baseline", type=pathlib.Path, required=True)
    parser.add_argument("--candidate", type=pathlib.Path, required=True)
    parser.add_argument("--iso", type=pathlib.Path, required=True)
    parser.add_argument("--output", "--out-dir", dest="out_dir", type=pathlib.Path, required=True)
    parser.add_argument("--cpu", type=int, default=2)
    parser.add_argument("--runs", type=int, default=2)
    parser.add_argument("--warmups", type=int, default=0)
    parser.add_argument("--fixed-steps", type=int)
    parser.add_argument("--snapshots", action="store_true")
    args = parser.parse_args()
    if args.runs < 1 or args.warmups < 0:
        parser.error("runs must be positive and warmups nonnegative")
    if args.fixed_steps is not None and args.fixed_steps < 1:
        parser.error("--fixed-steps must be positive")
    if args.snapshots and args.fixed_steps is None:
        parser.error("snapshots require a fixed instruction count")
    paths = {name: getattr(args, name).resolve() for name in ("runner", "baseline", "candidate", "iso")}
    paths["candidate_runner"] = (args.candidate_runner or args.runner).resolve()
    try:
        out = output_directory(args.out_dir)
    except ValueError as error:
        parser.error(str(error))
    report = new_report("tinycore_boot", "direct-kvm-fixed" if args.fixed_steps else "direct-kvm",
                        paths, cpu=args.cpu, warmups=args.warmups, runs=args.runs,
                        guest_clocks={"candidate": "deterministic", "baseline": "deterministic"},
                        candidate="candidate", baseline="baseline")
    report.update(fixed_steps=args.fixed_steps, mem_mib=256,
                  endpoint="fixed instruction boundary" if args.fixed_steps else "serial marker")
    save_report(report, out)
    for pair in range(args.warmups + args.runs):
        warmup = pair < args.warmups
        order = ("baseline", "candidate") if pair % 2 == 0 else ("candidate", "baseline")
        for name in order:
            label = f"{'warmup' if warmup else 'run'}-{pair + 1}-{name}"
            log = out / (label + ".log")
            command = [
                "taskset", "-c", str(args.cpu), str(paths["candidate_runner" if name == "candidate" else "runner"]),
                "--tinycore-null-monitor-smoke", "--create-vm", "--register-memslots",
                "--ram-mib", "256", "--monitor-mib", "16", "--shared-mib", "16", "--trace-mib", "16",
                "--iso", str(paths["iso"]), "--monitor-elf", str(paths[name]),
                "--host-decompress-kernel", "--step-limit", str(args.fixed_steps or 1_300_000_000),
                "--timer-poll-interval", "10000", "--bootlocal-command", "echo R86_BOOTLOCAL_OK",
                "--timeout-secs", "600", "--stall-timeout-secs", "120",
            ]
            if args.fixed_steps is None:
                command += ["--serial-success-marker", "R86_BOOTLOCAL_OK"]
            snapshot = out / (label + ".snap")
            if args.snapshots:
                command += ["--snapshot-out", str(snapshot)]
            before = resource.getrusage(resource.RUSAGE_CHILDREN)
            started = time.monotonic()
            with log.open("w") as stream:
                result = subprocess.run(command, stdout=stream, stderr=subprocess.STDOUT)
            elapsed = time.monotonic() - started
            after = resource.getrusage(resource.RUSAGE_CHILDREN)
            text = log.read_text()
            fields = dict(re.findall(r"^([a-z0-9_.]+)=(.*)$", text, re.MULTILINE))
            assert result.returncode == 0, (label, result.returncode, str(log))
            assert fields["hardmmu.shadow.errors"] == "0", label
            assert fields["hardmmu.bus.native_read_descriptor_aborts"] == "0", label
            assert fields["hardmmu.stats.parts_seen"] == "0x000000ff", label
            assert int(fields["hardmmu.bus.native_reads"].split("/")[1]) > 0, label
            if args.fixed_steps is None:
                assert fields["serial.success_marker_seen"] == "true", label
            else:
                assert int(fields["monitor.stop.steps"]) == args.fixed_steps, label
            record = {
                "label": label, "engine": name, "round": pair, "warmup": warmup, "command": command,
                "elapsed_seconds": elapsed,
                "cpu_seconds": after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime,
                "steps": int(fields["monitor.stop.steps"]),
                "native_reads": fields["hardmmu.bus.native_reads"],
                "stage1_installs": int(fields["hardmmu.shadow.stage1_installs"]),
                "flushes": fields["hardmmu.shadow.flushes"], "log": str(log),
            }
            if args.snapshots:
                record["snapshot_sha256"] = sha256(snapshot)
            if "hardmmu.shadow.contexts" in fields:
                record["contexts"] = fields["hardmmu.shadow.contexts"]
            if report["runs"]:
                first = report["runs"][0]
                assert record["steps"] == first["steps"], (label, "guest instruction count changed")
                if args.snapshots:
                    assert record["snapshot_sha256"] == first["snapshot_sha256"], (
                        label, "CPU/RAM/device snapshot changed"
                    )
            report["runs"].append(record)
            save_report(report, out)
            print(json.dumps(record), flush=True)
    finish_report(report, out)
    print(json.dumps(report["ratios"]), flush=True)


if __name__ == "__main__":
    main()
