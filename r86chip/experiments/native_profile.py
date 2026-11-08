#!/usr/bin/env python3
"""Attribute actual retired native words to linked firmware functions/source lines.

Run the ISS machine harness with --profile, supplying firmware with debug info.
Profiling is host-only: no native instructions are inserted or discounted. The
CSV must sum to the run's retired count; startup/devices/services stay included.
This is cost diagnosis, not the OS-corpus performance acceptance endpoint.
"""
import argparse
from collections import Counter
import csv
import hashlib
import json
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--firmware", required=True, type=Path)
    parser.add_argument("--directory", required=True, type=Path)
    args = parser.parse_args()
    directory = args.directory.resolve()
    firmware = args.firmware.resolve()
    rows = list(csv.DictReader((directory / "iss-profile.csv").open()))
    result = json.loads((directory / "iss-run.json").read_text())
    total = sum(int(r["retired"]) for r in rows)
    assert total == result["retired_native"], "profile does not account for every native word"
    output = subprocess.run(["llvm-symbolizer", "--output-style=JSON", "--inlines",
                             "--no-debuginfod", f"--obj={firmware}"],
                            input="".join(r["pc"] + "\n" for r in rows),
                            text=True, capture_output=True, check=True)
    symbols = [json.loads(line) for line in output.stdout.splitlines()]
    assert len(symbols) == len(rows)
    functions, lines, pcs = Counter(), Counter(), []
    for row, entry in zip(rows, symbols):
        stack = entry.get("Symbol", [])
        symbol = stack[0] if stack else {}
        function = symbol.get("FunctionName", "??")
        path = symbol.get("FileName", "??")
        line = symbol.get("Line", 0)
        count = int(row["retired"])
        functions[function] += count
        lines[f"{path}:{line}"] += count
        pcs.append(dict(pc=row["pc"], retired=count, stack=stack))
    report = dict(scope=__doc__, firmware_sha256=hashlib.sha256(firmware.read_bytes()).hexdigest(),
                  retired_native=total, x86_completed=result["x86_completed"],
                  functions=functions.most_common(), lines=lines.most_common(),
                  pcs=sorted(pcs, key=lambda x: x["retired"], reverse=True))
    (directory / "profile.json").write_text(json.dumps(report, indent=2) + "\n")
    for title, counts in (("Functions", functions), ("Source lines", lines)):
        print(title)
        for name, count in counts.most_common(30):
            print(f"{count:12d} {count / total:7.2%} {name}")


if __name__ == "__main__":
    main()
