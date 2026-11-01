#!/usr/bin/env python3
"""Renders the PMXSpan app icon to pmxspan.png and pmxspan.ico.

The mark is three stacked plates with rounded corners on a dark tile, the
top one in the accent orange (vector source: pmxspan.svg). The app embeds
the PNG as its window icon and in-app logo; the ICO goes into the Windows
executable and installer.

    pip install pillow
    python3 packaging/assets/make_icon.py
"""
import os

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
TILE = (0x15, 0x18, 0x1E, 255)
# Bottom to top: (corners in a 100x100 box, colour).
PLATES = [
    ([(50, 50), (78, 64), (50, 78), (22, 64)], (0x7E, 0x40, 0x11, 255)),
    ([(50, 35), (78, 49), (50, 63), (22, 49)], (0xB8, 0x5D, 0x17, 255)),
    ([(50, 20), (78, 34), (50, 48), (22, 34)], (0xF0, 0x7B, 0x1F, 255)),
]
STROKE = 7  # rounds the corners
GAP = 14  # tile-coloured outline that separates a plate from the one below


def rounded_polygon(d: ImageDraw.ImageDraw, pts, color, width: float, k: float) -> None:
    """A filled polygon outlined with round joins (Pillow has no round line joins)."""
    p = [(x * k, y * k) for x, y in pts]
    d.polygon(p, fill=color)
    r = width * k / 2
    for a, b in zip(p, p[1:] + p[:1]):
        d.line([a, b], fill=color, width=round(width * k))
    for x, y in p:
        d.ellipse((x - r, y - r, x + r, y + r), fill=color)


def render(n: int) -> Image.Image:
    # Draw at 8x and downsample for smooth edges.
    k = n * 8 / 100
    img = Image.new("RGBA", (n * 8, n * 8), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle((0, 0, n * 8 - 1, n * 8 - 1), radius=round(23 * k), fill=TILE)
    for i, (pts, color) in enumerate(PLATES):
        if i > 0:
            rounded_polygon(d, pts, TILE, GAP, k)
        rounded_polygon(d, pts, color, STROKE, k)
    return img.resize((n, n), Image.LANCZOS)


def main() -> None:
    png = os.path.join(HERE, "pmxspan.png")
    ico = os.path.join(HERE, "pmxspan.ico")
    big = render(256)
    big.save(png, optimize=True)
    sizes = [256, 128, 64, 48, 32, 24, 16]
    # Each size rendered on its own so small icons stay crisp.
    big.save(ico, sizes=[(s, s) for s in sizes], append_images=[render(s) for s in sizes[1:]])
    print("wrote", png, ico)


if __name__ == "__main__":
    main()
