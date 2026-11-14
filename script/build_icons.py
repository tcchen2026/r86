#!/usr/bin/env python3
"""Render the r86 logo for the web console, Windows and the Mac.

The source is ports/cardputerzero/packaging/icon.svg, the handheld's pixel-art
CRT: 80x80 cells of 6x6 units, one <rect> per run. Requires Pillow. Writes:

  web/r86.png                  80x80, scaled up pixelated by the page
  src/frontend/win32/r86.ico   16-256 px, with 80 and 160 px sharp
  apple/r86/r86.icns           16-1024 px, artwork in the 824 px icon grid
"""
from pathlib import Path
import re

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
SVG = ROOT / 'ports/cardputerzero/packaging/icon.svg'
CELL = 6
RECT = re.compile(r'<rect x="(\d+)" y="(\d+)" width="(\d+)" height="(\d+)" fill="#([0-9a-f]{6})"/>')


def cells():
    svg = SVG.read_text()
    size = int(re.search(r'viewBox="0 0 (\d+) \1"', svg).group(1)) // CELL
    image = Image.new('RGBA', (size, size))
    rects = RECT.findall(svg)
    if len(rects) != svg.count('<rect'):
        raise SystemExit(f'{SVG}: expected only plain filled rects')
    for *box, fill in rects:
        x, y, w, h = (int(v) // CELL for v in box)
        image.paste((*bytes.fromhex(fill), 255), (x, y, x + w, y + h))
    return image


def framed(image, canvas, art):
    """`image` scaled whole-pixel to `art` px, centred on a `canvas` px square."""
    out = Image.new('RGBA', (canvas, canvas))
    scaled = image.resize((art, art), Image.NEAREST)
    out.paste(scaled, ((canvas - art) // 2,) * 2)
    return out


def main():
    image = cells()
    assert image.size == (80, 80), image.size
    image.save(ROOT / 'web/r86.png', optimize=True)
    # Integer scales stay sharp; the small Windows sizes are filtered down.
    # 80 and 160 px are the VM window's "Powered off" logo at 1x and 2x.
    small = [image.resize((n, n), Image.LANCZOS) for n in (16, 24, 32, 48, 64)]
    sharp = [image, image.resize((160, 160), Image.NEAREST)]
    framed(image, 256, 240).save(
        ROOT / 'src/frontend/win32/r86.ico', append_images=small + sharp,
        sizes=[(n, n) for n in (16, 24, 32, 48, 64, 80, 160, 256)])
    framed(image, 1024, 800).save(ROOT / 'apple/r86/r86.icns')


if __name__ == '__main__':
    main()
