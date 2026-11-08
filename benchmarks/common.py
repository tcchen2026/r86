"""Shared Linux timing and report helpers for the two boot benchmarks.

Guest setup and endpoint detection belong to each benchmark. CPU time at the
endpoint is distinct from child CPU through teardown; never silently substitute
one for the other. A complete report is evidence, not an automatic acceptance.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import platform
import re
import resource
import statistics
import subprocess
import sys
from datetime import datetime, timezone


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def child_cpu() -> float:
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    return usage.ru_utime + usage.ru_stime


def endpoint_cpu(pid: int) -> float:
    stat = Path(f"/proc/{pid}/stat").read_text()
    fields = stat[stat.rfind(")") + 2:].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def output_directory(path: Path) -> Path:
    path = path.resolve()
    if path.exists() and (not path.is_dir() or any(path.iterdir())):
        raise ValueError(f"output directory must be empty: {path}")
    path.mkdir(parents=True, exist_ok=True)
    return path


def qemu_identity(path: Path, backend: str = "native") -> dict:
    config = (path.parent / "config-host.h").read_text()
    tci = bool(re.search(r"^#define CONFIG_TCG_INTERPRETER\b", config, re.M))
    tcti = bool(re.search(r"^#define CONFIG_TCG_THREADED_INTERPRETER\b", config, re.M))
    matches = {"native": not tci and not tcti, "tcti": tcti and not tci}[backend]
    if any(not re.search(rf"^#define {define} 1$", config, re.M)
           for define in ("CONFIG_TCG", "HOST_AARCH64")) or not matches:
        raise ValueError(f"QEMU must use AArch64 {backend} TCG (config-host.h beside executable)")
    return {"qemu_backend": backend, "qemu_build_config": config,
            "qemu_version": subprocess.check_output([str(path), "--version"], text=True)}


def new_report(benchmark: str, profile: str, paths: dict[str, Path], *,
               cpu: int, warmups: int, runs: int, guest_clocks: dict[str, str],
               candidate: str = "r86",
               baseline: str | None = None) -> dict:
    return {
        "schema_version": 2, "benchmark": benchmark, "profile": profile,
        "started_utc": datetime.now(timezone.utc).isoformat(),
        "host": platform.uname()._asdict(), "cpu": cpu,
        "observer_affinity": sorted(os.sched_getaffinity(0)),
        "invocation": sys.argv,
        "inputs": {name: {"path": str(path), "sha256": sha256(path)}
                   for name, path in paths.items()},
        "warmups": warmups, "measured_runs_per_engine": runs,
        "full_run_count": runs >= 2,
        "guest_clocks": guest_clocks,
        "clock_policies_match": len(set(guest_clocks.values())) == 1,
        "candidate": candidate, "baseline": baseline,
        "completed": False, "runs": [],
    }


def save_report(report: dict, directory: Path) -> None:
    (directory / "results.json").write_text(json.dumps(report, indent=2) + "\n")


def finish_report(report: dict, directory: Path) -> None:
    names = sorted({run["engine"] for run in report["runs"]})
    medians = {}
    ranges = {}
    for name in names:
        all_runs = [run for run in report["runs"] if run["engine"] == name]
        measured = [run for run in all_runs if not run["warmup"]]
        if (len(measured) != report["measured_runs_per_engine"]
                or len(all_runs) - len(measured) != report["warmups"]):
            raise ValueError(f"incomplete sample count for {name}")
        medians[name] = {}
        ranges[name] = {}
        for metric in ("elapsed_seconds", "cpu_seconds"):
            values = [run[metric] for run in measured]
            medians[name][metric] = statistics.median(values)
            ranges[name][metric] = [min(values), max(values)]
    if report["candidate"] not in medians:
        raise ValueError("missing candidate samples")
    report.update(medians=medians, ranges=ranges)
    if report["baseline"] is not None:
        baseline = medians[report["baseline"]]
        candidate = medians[report["candidate"]]
        report["ratios"] = {metric: candidate[metric] / baseline[metric]
                            for metric in candidate}
    report["completed"] = True
    save_report(report, directory)
    lines = [f"# {report['benchmark']}", "", f"Profile: `{report['profile']}`.", "",
             f"Warm-ups: {report['warmups']}; measured runs per engine: "
             f"{report['measured_runs_per_engine']}.", "",
             "| Engine | Guest clock | Wall median (s) | CPU median (s) | Wall range (s) |",
             "| --- | --- | ---: | ---: | --- |"]
    for name, metrics in medians.items():
        low, high = ranges[name]["elapsed_seconds"]
        lines.append(f"| {name} | {report['guest_clocks'][name]} | {metrics['elapsed_seconds']:.6f} | "
                     f"{metrics['cpu_seconds']:.6f} | {low:.6f}–{high:.6f} |")
    if not report["clock_policies_match"]:
        lines += ["", "Guest clock policies differ; ratios include device-time-policy differences."]
    if "ratios" in report:
        lines += ["", f"{report['candidate']} / {report['baseline']}: "
                  f"{report['ratios']['elapsed_seconds']:.6f} wall, "
                  f"{report['ratios']['cpu_seconds']:.6f} CPU."]
    lines += ["", "Input identities, commands and samples are in `results.json`.",
              "A completed run does not alone establish performance acceptance.", ""]
    (directory / "results.md").write_text("\n".join(lines))
