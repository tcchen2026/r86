#!/usr/bin/env python3
"""Two alternating cold ReactOS boots: ARM64 guest JIT versus Node Wasm JIT.

Unchanged ISO, identical firmware, 128 MiB guest RAM, realtime clocks and full
frame observation. Time includes process creation, loading and compilation.
The Wasm cache budget excludes guest RAM and buffered ISO data. Engine-private
memory is not equal to emitted Wasm bytes: retain Node RSS and accounting too.
All child threads inherit the same CPU affinity, including V8 compilation.
"""
import argparse
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import signal
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from common import child_cpu, endpoint_cpu, finish_report, new_report, output_directory, save_report, sha256
from endpoint import matching_frame

ROOT = Path(__file__).resolve().parents[2]
MARKER = re.compile(rb"(?:REACTOS_LANGUAGE_DIALOG|R86_WASM_FRAME) elapsed=([\d.]+) steps=(\d+) polls=(\d+)")


def run(command, cpu, directory, timeout):
    directory.mkdir()
    result = {"command": command, "log": str(directory / "emulator.log"), "frame": str(directory / "matched.ppm")}
    started = time.monotonic()
    before = child_cpu()
    with (directory / "emulator.log").open("wb") as log:
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, start_new_session=True,
                                   preexec_fn=lambda: os.sched_setaffinity(0, {cpu}))
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                tail = b""
                while time.monotonic() - started < timeout:
                    if not selector.select(0.5):
                        continue
                    data = os.read(process.stdout.fileno(), 65536)
                    if not data:
                        break
                    log.write(data); log.flush()
                    tail = (tail + data)[-65536:]
                    match = MARKER.search(tail)
                    if match and "elapsed_seconds" not in result:
                        result.update(elapsed_seconds=time.monotonic() - started,
                                      cpu_seconds=endpoint_cpu(process.pid),
                                      internal_elapsed_seconds=float(match[1]), steps=int(match[2]), polls=int(match[3]))
            if "elapsed_seconds" not in result:
                raise RuntimeError(f"complete language dialog missing: {directory}")
            if process.wait(timeout=10) != 0 or not matching_frame(directory / "matched.ppm"):
                raise RuntimeError(f"failed run/frame: {directory}")
            result.update(total_cpu_seconds=child_cpu() - before, frame_sha256=sha256(directory / "matched.ppm"))
            if (directory / "node.json").exists():
                result["node_report"] = json.loads((directory / "node.json").read_text())
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL); process.wait()
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ("native", "wasm", "iso", "output"):
        p.add_argument(f"--{name}", type=Path, required=True)
    p.add_argument("--node", default=shutil.which("node"))
    p.add_argument("--bios", type=Path, default=ROOT / "firmware/seabios/bios.bin")
    p.add_argument("--vga-bios", type=Path, default=ROOT / "firmware/seabios/vgabios-stdvga.bin")
    p.add_argument("--cpu", type=int, default=3)
    p.add_argument("--runs", type=int, default=2)
    p.add_argument("--timeout", type=int, default=600)
    p.add_argument("--frame-ms", type=int, default=100)
    p.add_argument("--jit-cache-mib", type=int, default=128)
    a = p.parse_args()
    if min(a.runs, a.timeout, a.frame_ms) < 1 or not a.node:
        p.error("invalid run count, timeout, frame cadence or Node executable")
    paths = {key: getattr(a, key).resolve(strict=True) for key in ("native", "wasm", "iso", "bios", "vga_bios")}
    paths.update(cli=ROOT / "script/wasm_cli.cjs", js_host=ROOT / "web/wasm.mjs", jit_host=ROOT / "web/jit.mjs",
                 signature=Path(__file__).with_name("langsel.rgb"))
    directory = output_directory(a.output)
    # V8 retains its normal lazy/tiered policy. The separate code ceiling is
    # fatal if reached, not a substitute for the core's eviction accounting.
    flags = ["--wasm-num-compilation-tasks=1", "--wasm-max-committed-code-mb=96"]
    report = new_report("reactos_livecd_boot_to_langsel", "arm64-jit-vs-node-wasm-jit", paths,
                        cpu=a.cpu, warmups=0, runs=a.runs, candidate="wasm", baseline="native",
                        guest_clocks={"native": "realtime", "wasm": "realtime"})
    report.update(node_versions=json.loads(subprocess.check_output([a.node, "-p", "JSON.stringify(process.versions)"], text=True)),
                  node_flags=flags, mem_mib=128, frame_ms=a.frame_ms, jit_cache_mib=a.jit_cache_mib)
    save_report(report, directory)
    for index in range(a.runs):
        for name in (("native", "wasm") if index % 2 == 0 else ("wasm", "native")):
            sample = directory / f"{index:02d}-{name}"
            frame = str(sample / "matched.ppm")
            if name == "native":
                command = [str(paths["native"]), str(paths["iso"]), str(paths["bios"]), str(paths["vga_bios"]),
                           "jit", frame, str(a.timeout), str(a.frame_ms), "realtime"]
            else:
                command = [a.node, *flags, str(paths["cli"]), "--cpu-engine", "jit", "--wasm", str(paths["wasm"]),
                           "--iso", str(paths["iso"]), "--bios", str(paths["bios"]), "--vga-bios", str(paths["vga_bios"]),
                           "--firmware-display", "vga", "--clock", "realtime", "--mem-mib", "128", "--steps", "100000000000",
                           "--timeout", str(a.timeout), "--frame-ms", str(a.frame_ms), "--jit-cache-mib", str(a.jit_cache_mib),
                           "--expect-frame", str(paths["signature"]), "--frame", frame, "--report", str(sample / "node.json"), "--quiet"]
            result = run(command, a.cpu, sample, a.timeout + 10)
            result.update(engine=name, round=index, warmup=False)
            report["runs"].append(result); save_report(report, directory)
            print(json.dumps({key: result[key] for key in ("engine", "round", "elapsed_seconds", "cpu_seconds", "frame_sha256")}), flush=True)
    report["frames_equal"] = len({run["frame_sha256"] for run in report["runs"]}) == 1
    if not report["frames_equal"]:
        save_report(report, directory); raise RuntimeError("full endpoint frames differ")
    finish_report(report, directory)
    print(json.dumps(report["ratios"]), flush=True)


if __name__ == "__main__":
    main()
