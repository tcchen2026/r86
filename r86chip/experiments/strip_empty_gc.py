#!/usr/bin/env python3
"""Specialize Verilator 5.032's host wrapper for this fixed, class-free RTL core.

The generated r86_core model never queues SV objects for deletion. Its eval_step
still takes an uncontended collector mutex on every evaluation. After checking
all generated C++ sources, remove only that empty collection call. RTL evaluation,
initialization, finalization, clocks and model data remain unchanged. This is an
optional host simulation experiment, not an RTL optimization or a boot shortcut.

Run after Verilation and before compiling C++; regenerate any GCC profile data.
Unsupported/generated models fail the checks before any source is changed.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re

CALL = "    vlSymsp->__Vm_deleter.deleteAll();"
REPLACEMENT = "    // Model has no SV dynamic objects: guarded removal of empty GC housekeeping."
RUNTIME_HASHES = {
    "verilated.cpp": "ccb2ed63ef9e66fe27f35da19985326e237eaa65bcec4b5f6eb8d491e5a1bc08",
    "verilated_types.h": "6791c1970b5b89342422be6aa8d3df51167d22d6c872b5e4e253553fb03a9275",
}


def specialize(directory):
    directory = Path(directory)
    makefile = (directory / "Vr86_core.mk").read_text()
    match = re.search(r"^VERILATOR_ROOT = (.+)$", makefile, re.MULTILINE)
    if not match:
        raise ValueError("cannot identify the generated model's Verilator runtime")
    include = Path(match[1].strip()) / "include"
    for name, expected in RUNTIME_HASHES.items():
        if hashlib.sha256((include / name).read_bytes()).hexdigest() != expected:
            raise ValueError(f"unreviewed Verilator runtime implementation: {include / name}")
    sources = {str(p.relative_to(directory)): p for p in directory.rglob("*")
               if p.suffix in (".cpp", ".h")}
    contents = {name: p.read_text() for name, p in sources.items()}
    if any(token in text for text in contents.values()
           for token in ("VlClassRef", "VlDeletable", "VlClass ")):
        raise ValueError("generated model uses dynamic SV objects")
    uses = sorted((name, line.strip()) for name, text in contents.items()
                  for line in text.splitlines() if "__Vm_deleter" in line)
    if uses != [("Vr86_core.cpp", CALL.strip()),
                ("Vr86_core__Syms.h", "VlDeleter __Vm_deleter;")]:
        raise ValueError(f"unexpected model collector ownership/use: {uses}")
    path = directory / "Vr86_core.cpp"
    original = contents[path.name]
    if (original.count(CALL) != 1
            or "unsigned Vr86_core::threads() const { return 1; }" not in original):
        raise ValueError("expected the fixed single-threaded r86_core model")
    backup = path.with_suffix(".cpp.before-empty-gc")
    if backup.exists():
        raise ValueError("original wrapper backup already exists")
    changed = original.replace(CALL, REPLACEMENT)
    record = dict(scope="host wrapper empty SV collector only",
                  checked_runtime_root=str(include.parent), runtime_hashes=RUNTIME_HASHES,
                  original_sha256=hashlib.sha256(original.encode()).hexdigest(),
                  specialized_sha256=hashlib.sha256(changed.encode()).hexdigest(),
                  generated_sources_before={name: hashlib.sha256(text.encode()).hexdigest()
                                            for name, text in contents.items()},
                  checked_uses=uses)
    backup.write_text(original)
    path.write_text(changed)
    (directory / "empty-gc-specialization.json").write_text(json.dumps(record, indent=2) + "\n")
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("object_directory", type=Path)
    args = parser.parse_args()
    result = specialize(args.object_directory)
    print(json.dumps({k: v for k, v in result.items() if k != "generated_sources_before"}, indent=2))


if __name__ == "__main__":
    main()
