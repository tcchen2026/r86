#!/usr/bin/env python3
"""Build the shared interpreter and optional guest JIT for Node and the browser.

Default output needs only Rust's wasm32-unknown-unknown target. --optimize applies
Binaryen to the core at build time. --asyncify also produces r86-asyncify.wasm
using Binaryen (tested with 120); only browser.disk_io
may suspend. Do not use asyncify-ignore-indirect: Rust device calls can be
indirect. The raw build is the CPU baseline, the transformed build measures the
cost of enabling asynchronous external I/O. Neither stores guest translations.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--asyncify", action="store_true")
    parser.add_argument("--optimize", action="store_true", help="Post-optimize the core with Binaryen")
    parser.add_argument("--jit", action="store_true", help="Include the bounded x86-to-Wasm region compiler")
    parser.add_argument("--profile", action="store_true", help="Diagnostic build; not for timing acceptance")
    parser.add_argument("--wasm-opt", default="wasm-opt")
    parser.add_argument("--output", type=Path, default=ROOT / "web/dist")
    args = parser.parse_args()
    features = "web-frontend,bus-instruction-context" + (",wasm-jit" if args.jit else "")
    if args.profile:
        features += ",jit-profile"
    command = ["cargo", "rustc", "--release", "--no-default-features",
               "--features", features, "--bin", "r86-web",
               "--target", "wasm32-unknown-unknown", "--",
               "-C", "link-arg=-zstack-size=4194304", "-C", "link-arg=--max-memory=1073741824"]
    if args.jit:
        command += ["-C", "link-arg=--export-table", "-C", "link-arg=--growable-table"]
    subprocess.run(command, cwd=ROOT, check=True)
    target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
    if not target.is_absolute():
        target = ROOT / target
    args.output.mkdir(parents=True, exist_ok=True)
    raw = args.output / ("r86-jit.wasm" if args.jit else "r86.wasm")
    shutil.copyfile(target / "wasm32-unknown-unknown/release/r86-web.wasm", raw)
    files = [raw]
    manifest = {"command": command, "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
                "features": features, "guest_engine": "wasm-jit" if args.jit else "interpreter"}
    if args.optimize:
        # Preserve deliberate cold outlining. Binaryen's default single-caller
        # inlining has no size limit and can undo it in the hot dispatcher.
        opt = [args.wasm_opt, str(raw), "-O3", "--all-features", "--debuginfo",
               "--one-caller-inline-max-function-size=0", "-o", str(raw)]
        subprocess.run(opt, check=True)
        manifest["optimizer_command"] = opt
        manifest["binaryen"] = subprocess.check_output([args.wasm_opt, "--version"], text=True).strip()
    if args.asyncify:
        transformed = args.output / ("r86-jit-asyncify.wasm" if args.jit else "r86-asyncify.wasm")
        opt = [args.wasm_opt, str(raw), "-O2", "--asyncify", "--all-features",
               "--pass-arg=asyncify-imports@browser.disk_io", "-o", str(transformed)]
        subprocess.run(opt, check=True)
        manifest["asyncify_command"] = opt
        manifest["binaryen"] = subprocess.check_output([args.wasm_opt, "--version"], text=True).strip()
        files.append(transformed)
    manifest["artifacts"] = {f.name: {"bytes": f.stat().st_size, "sha256": hashlib.sha256(f.read_bytes()).hexdigest()} for f in files}
    (args.output / "build.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps(manifest["artifacts"], indent=2))


if __name__ == "__main__":
    main()
