#!/usr/bin/env python3
"""Add the Thumb-2 JIT section to design/VALIDATION.md.

usage: docs.py REPO_ROOT

Inserts cortex-m/stage/VALIDATION_SECTION.md before the "## Handheld frontend"
heading. Fails, changing nothing, if that heading is not present exactly once
or the section already exists.
"""
import pathlib
import sys


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    root = pathlib.Path(sys.argv[1])
    path = root / "design/VALIDATION.md"
    section = (pathlib.Path(__file__).parent / "VALIDATION_SECTION.md").read_text()
    text = path.read_text()
    anchor = "## Handheld frontend\n"
    if "## Optional Thumb-2 (Cortex-M) JIT checks" in text:
        sys.exit("design/VALIDATION.md already has the Thumb-2 section; nothing changed")
    if text.count(anchor) != 1:
        sys.exit(f"design/VALIDATION.md: expected exactly one {anchor!r}, found {text.count(anchor)}")
    path.write_text(text.replace(anchor, section + "\n" + anchor))
    print("design/VALIDATION.md: Thumb-2 section added before 'Handheld frontend'")


if __name__ == "__main__":
    main()
