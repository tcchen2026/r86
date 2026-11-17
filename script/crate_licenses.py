#!/usr/bin/env python3
"""Write the licence texts of every crates.io package r86 may link.

The set is a superset over all features and targets: cargo metadata with
--all-features, following normal dependency edges from r86 and skipping
build and dev dependencies and proc-macro crates (they run at compile time).
Each package's LICENSE*, COPYING* and NOTICE* files are copied from its
source; identical texts are printed once. Path dependencies such as the
vendored fatfs are covered by THIRD_PARTY_NOTICES.md instead.
ports/licenses.sh runs this, so every package that ships r86 carries it.
"""
import argparse
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
NOTICE = re.compile(r"^(un)?licen[cs]e|^copying|^notice", re.IGNORECASE)


def linked_packages(meta):
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    seen, stack = set(), [meta["resolve"]["root"]]
    while stack:
        for dep in nodes[stack.pop()]["deps"]:
            package = packages[dep["pkg"]]
            if (dep["pkg"] in seen
                    or all(kind["kind"] is not None for kind in dep["dep_kinds"])
                    or any("proc-macro" in target["kind"] for target in package["targets"])):
                continue
            seen.add(dep["pkg"])
            stack.append(dep["pkg"])
    registry = [packages[id] for id in seen if (packages[id]["source"] or "").startswith("registry+")]
    return sorted(registry, key=lambda p: (p["name"], p["version"]))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--all-features"],
        cwd=ROOT, check=True, capture_output=True, text=True).stdout)
    out = ["Licences of the Rust crates r86 may link (all features and targets),",
           "generated from Cargo.lock by r86's script/crate_licenses.py.",
           "Identical texts appear once.", ""]
    first = {}
    for package in linked_packages(meta):
        name = f"{package['name']} {package['version']}"
        out += [f"== {name}: {package['license'] or 'no licence declared'}"]
        if package.get("repository"):
            out += [f"   {package['repository']}"]
        files = sorted(f for f in Path(package["manifest_path"]).parent.iterdir()
                       if f.is_file() and NOTICE.match(f.name))
        if not files:
            out += ["   (the published crate has no licence file)", ""]
            print(f"crate_licenses: {name} ships no licence file", file=sys.stderr)
            continue
        for path in files:
            text = path.read_text(encoding="utf-8", errors="replace").strip()
            if text in first:
                out += [f"--- {path.name}: same text as {first[text]}"]
            else:
                first[text] = f"{name} {path.name}"
                out += [f"--- {path.name}", "", text, ""]
        out += [""]
    args.output.write_text("\n".join(out), encoding="utf-8")


if __name__ == "__main__":
    main()
