#!/usr/bin/env python3
"""Generate tiny386 INI files for the TinyCore and ReactOS benchmarks.

Ordinary values remain unquoted for the pinned d47748e parser. Values requiring
quotes use the syntax added by a569517/e99ae8f and require that newer parser.
The default tiny386 build limits each input line to a 200-byte buffer.
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys


def ini_value(value: str) -> str:
    if "\0" in value:
        raise ValueError("tiny386 INI values cannot contain NUL")
    escapes = {
        "\\": "\\\\",
        '"': '\\"',
        "\a": "\\a",
        "\b": "\\b",
        "\f": "\\f",
        "\n": "\\n",
        "\r": "\\r",
        "\t": "\\t",
        "\v": "\\v",
    }
    if (
        value == value.strip()
        and not value.startswith('"')
        and not re.search(r"(^|\s);|[\a\b\f\n\r\t\v]", value)
    ):
        return value
    return '"' + "".join(escapes.get(ch, ch) for ch in value) + '"'


def benchmark_config(
    iso: pathlib.Path, bios: pathlib.Path, vga_bios: pathlib.Path, *, serial: bool
) -> str:
    lines = [
        "[pc]",
        f"bios = {ini_value(str(bios))}",
        f"vga_bios = {ini_value(str(vga_bios))}",
        "mem_size = 128M",
        "vga_mem_size = 16M",
        f"cdb = {ini_value(str(iso))}",
        "fill_cmos = 0",
        f"enable_serial = {int(serial)}",
        "",
        "[display]",
        f"width = {720 if serial else 800}",
        f"height = {400 if serial else 600}",
        "",
        "[cpu]",
        "gen = 6",
        "fpu = 1",
        "",
    ]
    for number, line in enumerate(lines, 1):
        if len(line.encode("utf-8")) > 198:
            raise ValueError(
                f"tiny386 INI line {number} exceeds the default 200-byte buffer; "
                "use a shorter asset path"
            )
    return "\n".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--iso", type=pathlib.Path, required=True)
    parser.add_argument("--bios", type=pathlib.Path, required=True)
    parser.add_argument("--vga-bios", type=pathlib.Path, required=True)
    parser.add_argument("--serial", action="store_true")
    args = parser.parse_args()
    try:
        config = benchmark_config(
            args.iso.resolve(),
            args.bios.resolve(),
            args.vga_bios.resolve(),
            serial=args.serial,
        )
    except ValueError as err:
        parser.error(str(err))
    sys.stdout.write(config)


if __name__ == "__main__":
    main()
