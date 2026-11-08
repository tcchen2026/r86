"""Complete ReactOS language-dialog predicate shared by the Python/SDL drivers.

boot.rs embeds langsel.rgb for the same headless predicate. Pixel signatures
locate the endpoint; retain and compare the entire frame to the recorded golden
frame hash before accepting a performance result.
"""

import argparse
from pathlib import Path
import re


def points(profile="r86"):
    name = "langsel-tiny386.rgb" if profile == "tiny386" else "langsel.rgb"
    return [tuple(map(int, line.split()))
            for line in Path(__file__).with_name(name).read_text().splitlines()
            if line.strip() and not line.startswith("#")]


def matching_frame(path: Path) -> bool:
    raw = path.read_bytes()
    header = re.match(rb"P6\s+(\d+)\s+(\d+)\s+255\n", raw)
    if not header:
        raise RuntimeError(f"invalid PPM: {path}")
    width, height = map(int, header.groups())
    rgb = raw[header.end():]
    if len(rgb) != width * height * 3:
        raise RuntimeError(f"incomplete PPM: {path}")
    return (width, height) == (800, 600) and all(
        tuple(rgb[(y * width + x) * 3: (y * width + x) * 3 + 3]) == (r, g, b)
        for x, y, r, g, b in points())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("profile", choices=["r86", "tiny386"])
    args = parser.parse_args()
    signature = points(args.profile)
    print("|".join(f"%[pixel:p{{{x},{y}}}]" for x, y, *_ in signature))
    print("|".join(f"srgb({r},{g},{b})" for _, _, r, g, b in signature))


if __name__ == "__main__":
    main()
