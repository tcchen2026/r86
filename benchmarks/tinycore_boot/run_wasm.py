#!/usr/bin/env python3
"""Two alternating cold firmware boots: native interpreter vs Node Wasm.

Use a prepared TinyCore ISO with the standard serial marker (same producer and
command line as run.py). No JIT guest backend, snapshots, code cache or warmups.
Default Wasm ISO reads use a buffer, matching r86-run; --async-io explicitly
measures Asyncify plus actual asynchronous file reads. Node/V8 measurements do
not establish Safari/WebKit or Apple-device performance. Both build artifacts,
JS host sources, firmware and ISO are identified by SHA-256 in the report.
"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from common import finish_report, new_report, output_directory, save_report
from run import CMDLINE, run

ROOT = Path(__file__).resolve().parents[2]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--native", type=Path, required=True)
    p.add_argument("--wasm", type=Path, required=True)
    p.add_argument("--iso", type=Path, required=True)
    p.add_argument("--node", default=shutil.which("node"))
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--bios", type=Path, default=ROOT / "firmware/seabios/bios.bin")
    p.add_argument("--vga-bios", type=Path, default=ROOT / "firmware/seabios/vgabios-stdvga.bin")
    p.add_argument("--cpu", type=int, default=3)
    p.add_argument("--runs", type=int, default=2)
    p.add_argument("--timeout", type=int, default=600)
    p.add_argument("--clock", choices=["realtime", "deterministic"], default="realtime")
    p.add_argument("--async-io", action="store_true")
    p.add_argument("--marker", default="R86_BOOTLOCAL_OK")
    a = p.parse_args()
    if a.runs < 1 or a.timeout < 1 or not a.marker or not a.node:
        p.error("invalid runs, timeout, marker or Node executable")
    paths = {name: getattr(a, name).resolve(strict=True) for name in ("native", "wasm", "iso", "bios", "vga_bios")}
    paths.update(cli=ROOT / "script/wasm_cli.cjs", js_host=ROOT / "web/wasm.mjs")
    directory = output_directory(a.output)
    shared = ["--iso", str(paths["iso"]), "--bios", str(paths["bios"]), "--vga-bios", str(paths["vga_bios"]),
              "--boot-order", "cdrom", "--firmware-display", "serial", "--mem-mib", "128",
              "--clock", a.clock, "--steps", "10000000000", "--expect-serial", a.marker]
    commands = {
        "native": [str(paths["native"]), *shared, "--cpu-engine", "interp", "--cmdline", CMDLINE],
        "wasm": [a.node, str(paths["cli"]), *shared, "--wasm", str(paths["wasm"]), "--timeout", str(a.timeout)],
    }
    if a.async_io:
        commands["wasm"].append("--async-io")
    report = new_report("tinycore_boot", "native-vs-node-wasm", paths,
                        cpu=a.cpu, warmups=0, runs=a.runs, candidate="wasm", baseline="native",
                        guest_clocks={name: a.clock for name in commands})
    report.update(node_versions=json.loads(subprocess.check_output([a.node, "-p", "JSON.stringify(process.versions)"], text=True)),
                  mem_mib=128, marker=a.marker, cmdline=CMDLINE, async_io=a.async_io,
                  endpoint="serial marker", limitations="Node/V8 on this host; not Safari or Apple hardware")
    save_report(report, directory)
    for index in range(a.runs):
        for name in (("native", "wasm") if index % 2 == 0 else ("wasm", "native")):
            command = list(commands[name])
            if name == "wasm":
                command += ["--report", str(directory / f"{index:02d}-wasm.json")]
            result = run(command, a.cpu, a.marker.encode(), directory / f"{index:02d}-{name}.log", a.timeout)
            result.update(engine=name, round=index, warmup=False)
            report["runs"].append(result)
            save_report(report, directory)
            print(json.dumps({k: result[k] for k in ("engine", "round", "elapsed_seconds", "cpu_seconds")}), flush=True)
    finish_report(report, directory)
    print(json.dumps(report["medians"]), flush=True)


if __name__ == "__main__":
    main()
