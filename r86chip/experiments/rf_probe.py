#!/usr/bin/env python3
"""Test and map a standalone 2R1W RF to Gowin GW5A primitives.

Requires Yosys with synth_gowin -family gw5a, Icarus Verilog and vvp. No timing,
whole-core area or board-fit claim. Outputs stay in the ignored experiments/out.
"""
from collections import Counter
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

HERE = Path(__file__).resolve().parent
OUT = HERE / "out/rf-probe"
SOURCE = HERE / "rf_probe.v"
TEST = HERE / "rf_probe_tb.v"


def run(command, cwd):
    result = subprocess.run(command, cwd=cwd, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT)
    if result.returncode:
        raise RuntimeError(f"{command!r} failed in {cwd}:\n{result.stdout[-6000:]}")
    return result.stdout


def prepare_library(datadir, output=OUT):
    original_path = datadir / "gowin/cells_xtra_gw5a.v"
    original = original_path.read_text()
    patched = original
    repairs = []
    # Installed Yosys 0.52 has two commas where this unused MIPI blackbox needs
    # declaration-ending semicolons. Keep every primitive, especially the GW5A
    # SDPX9B RAMs. Never edit the installation or substitute a different family.
    for end in ("HSTXEN_LNCK,\n", "HSRX_ODTEN_CK, \n"):
        if end in patched:
            assert patched.count(end) == 1
            patched = patched.replace(end, end.split(",")[0] + ";\n")
            repairs.append(end.strip())
    local = output / "cells_xtra_gw5a.v"
    local.write_text(patched)
    # The installed SDP mapper uses RESETA/RESETB, but its GW5A SDPB/SDPX9B
    # declarations have one RESET port. Repair only that module in a local copy.
    # The RF does not request RAM reset; this removes an invalid constant-zero
    # port and connects the existing read-reset signal to the declared port.
    mapper_path = datadir / "gowin/brams_map.v"
    mapper_original = mapper_path.read_text()
    mapper = mapper_original
    prefix, sdp = mapper.split("module $__GOWIN_SDP_", 1)
    mapper_repairs = 0
    if "SDPX9B #(" in sdp and ".RESETA(1'b0)" in sdp:
        assert sdp.count(".RESETA(1'b0)") == 2
        assert sdp.count(".RESETB(RST)") == 2
        sdp = sdp.replace("\t\t.RESETA(1'b0),\n", "")
        sdp = sdp.replace(".RESETB(RST)", ".RESET(RST)")
        mapper = prefix + "module $__GOWIN_SDP_" + sdp
        mapper_repairs = 2
    mapper_local = output / "brams_map.v"
    mapper_local.write_text(mapper)
    return local, mapper_local, {
        "original_path": str(original_path),
        "original_sha256": hashlib.sha256(original.encode()).hexdigest(),
        "local_sha256": hashlib.sha256(patched.encode()).hexdigest(),
        "MIPI_DPHY_comma_to_semicolon_repairs": repairs,
        "mapper_original_sha256": hashlib.sha256(mapper_original.encode()).hexdigest(),
        "mapper_local_sha256": hashlib.sha256(mapper.encode()).hexdigest(),
        "SDP_reset_port_repairs": mapper_repairs,
        "scope": "Local copies only; primitive definitions unchanged except MIPI syntax",
    }


def measure(depth, synchronous, block, allow_wide_luts, datadir, library, mapper):
    name = f"{'sync' if synchronous else 'async'}-{'bram' if block else 'lutram'}-{depth}"
    name += "-wide" if allow_wide_luts else "-lut4"
    out = OUT / name
    out.mkdir(parents=True, exist_ok=True)
    parameters = {"ADDR_BITS": depth.bit_length() - 1,
                  "SYNC_READ": int(synchronous), "BLOCK_RAM": int(block)}
    run(["iverilog", "-g2012", "-s", "rf_probe_tb", "-o", "test.vvp",
         *[f"-Prf_probe_tb.{key}={value}" for key, value in parameters.items()],
         str(SOURCE), str(TEST)], out)
    simulation = run(["vvp", "test.vvp"], out)
    (out / "simulation.log").write_text(simulation)
    changes = " ".join(f"-set {key} {value}" for key, value in parameters.items())
    synthesis = "synth_gowin" + ("" if allow_wide_luts else " -nowidelut")
    script = (f"read_verilog -sv {json.dumps(str(SOURCE))}\n"
              f"chparam {changes} r86_rf\n"
              f"read_verilog -specify -lib {json.dumps(str(datadir / 'gowin/cells_sim.v'))}\n"
              f"read_verilog -specify -lib {json.dumps(str(library))}\n"
              "hierarchy -check -top r86_rf\n"
              f"{synthesis} -run flatten:map_ram -family gw5a -top r86_rf -noiopads\n"
              "memory_libmap -lib +/gowin/lutrams.txt -lib +/gowin/brams.txt\n"
              f"techmap -map +/gowin/lutrams_map.v -map {json.dumps(str(mapper))}\n"
              f"{synthesis} -run map_ffram: -family gw5a -top r86_rf -noiopads -json mapped.json\n"
              "check -assert\nstat\nwrite_verilog -noattr mapped.v\n")
    (out / "synth.ys").write_text(script)
    run(["yosys", "-Q", "-T", "-l", "synthesis.log", "synth.ys"], out)
    module = json.loads((out / "mapped.json").read_text())["modules"]["r86_rf"]
    cells = Counter(cell["type"] for cell in module["cells"].values())
    mapped_simulation = "Not run: GW5A BSRAM library declarations are blackboxes"
    if not block:
        run(["iverilog", "-g2012", "-DPOST_MAP", "-s", "rf_probe_tb", "-o", "mapped.vvp",
             *[f"-Prf_probe_tb.{key}={value}" for key, value in parameters.items()],
             "mapped.v", str(TEST), str(datadir / "gowin/cells_sim.v")], out)
        mapped_simulation = run(["vvp", "mapped.vvp"], out)
        (out / "mapped-simulation.log").write_text(mapped_simulation)
    # Keep dedicated wide muxes and distributed RAM separate. A cell histogram
    # is not a place-and-route physical utilization report.
    result = {
        "name": name, "depth": depth, "read_latency_clocks": int(synchronous),
        "ram_style": "block" if block else "distributed",
        "allow_wide_luts": allow_wide_luts,
        "logic_lut1_to_lut4": sum(cells[f"LUT{i}"] for i in range(1, 5)),
        "wide_mux_cells": sum(n for cell, n in cells.items() if cell.startswith("MUX")),
        "flip_flops": sum(n for cell, n in cells.items() if cell.startswith("DFF")),
        "block_ram_cells": sum(cells[cell] for cell in
                               ("SP", "SPX9", "SDP", "SDPX9", "DP", "DPX9",
                                "SDPB", "SDPX9B", "DPB", "DPX9B")),
        "distributed_ram_cells": {cell: n for cell, n in sorted(cells.items())
                                  if cell.startswith("RAM")},
        "all_cells": dict(sorted(cells.items())),
        "simulation": simulation.splitlines()[0],
        "mapped_simulation": mapped_simulation.splitlines()[0],
    }
    if block:
        assert result["block_ram_cells"] > 0, result
    else:
        assert result["distributed_ram_cells"], result
        assert result["block_ram_cells"] == 0, result
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--yosys-data", type=Path,
                        default=Path(shutil.which("yosys") or "/usr/bin/yosys")
                        .resolve().parent.parent / "share/yosys")
    args = parser.parse_args()
    OUT.mkdir(parents=True, exist_ok=True)
    library, mapper, library_report = prepare_library(args.yosys_data.resolve())
    # The two depths in each pair differ only in the address/context bit.
    results = [measure(depth, sync, block, wide, args.yosys_data.resolve(), library, mapper)
               for wide in (False, True)
               for sync, block in ((False, False), (True, False), (True, True))
               for depth in (32, 64)]
    report = {
        "scope": "Standalone RF synthesis only; no CPU, pipeline switch, routing or Fmax",
        "library": library_report,
        "yosys": run(["yosys", "-V"], HERE).strip(),
        "iverilog": run(["iverilog", "-V"], HERE).splitlines()[0],
        "source_sha256": hashlib.sha256(SOURCE.read_bytes()).hexdigest(),
        "test_sha256": hashlib.sha256(TEST.read_bytes()).hexdigest(),
        "results": results,
    }
    (OUT / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    lines = ["# Standalone GW5A register-file mapping", "", report["scope"], "",
             "GW5A primitive library loaded from a local copy; report.json records any parser repairs.", "",
             "Logic LUTs exclude RAM primitives and dedicated wide muxes.", "",
             "| Variant | Logic LUT1–4 | Wide mux | FF | BSRAM | Distributed RAM |",
             "|---|---:|---:|---:|---:|---|"]
    for row in results:
        ram = ", ".join(f"{cell} × {n}" for cell, n in row["distributed_ram_cells"].items()) or "—"
        lines.append(f"| {row['name']} | {row['logic_lut1_to_lut4']} | "
                     f"{row['wide_mux_cells']} | {row['flip_flops']} | "
                     f"{row['block_ram_cells']} | {ram} |")
    lines.extend(["", report["yosys"], "", report["iverilog"], ""])
    markdown = "\n".join(lines)
    (OUT / "report.md").write_text(markdown)
    print(markdown)


if __name__ == "__main__":
    main()
