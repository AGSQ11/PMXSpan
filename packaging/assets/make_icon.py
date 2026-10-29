#!/usr/bin/env python3
"""Renders the app icon (same design as src/ui/icon.rs) to a PPM+alpha pair,
then ImageMagick turns it into pmxspan.png and pmxspan.ico.

    python3 packaging/assets/make_icon.py
"""
import math
import os
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))


def render(n):
    rgba = bytearray(n * n * 4)
    c = (n - 1) / 2
    stroke = 5.5 * n / 64
    for y in range(n):
        for x in range(n):
            fx, fy = x - c, y - c
            edge = max(abs(fx), abs(fy)) / c
            if edge > 0.97:
                continue
            r, g, b = 0x1D, 0x23, 0x2B
            d1 = abs(fx - fy) / math.sqrt(2)
            d2 = abs(fx + fy) / math.sqrt(2)
            if (d1 < stroke or d2 < stroke) and edge < 0.72:
                r, g, b = 0xE5, 0x70, 0x00
            i = (y * n + x) * 4
            rgba[i:i + 4] = bytes((r, g, b, 255))
    return bytes(rgba)


def main():
    n = 256
    raw = os.path.join(HERE, "icon.rgba")
    with open(raw, "wb") as f:
        f.write(render(n))
    png = os.path.join(HERE, "pmxspan.png")
    ico = os.path.join(HERE, "pmxspan.ico")
    subprocess.run(["convert", "-size", f"{n}x{n}", "-depth", "8", f"rgba:{raw}", png], check=True)
    subprocess.run(["convert", png, "-define", "icon:auto-resize=256,128,64,48,32,16", ico], check=True)
    os.remove(raw)
    print("wrote", png, ico)


if __name__ == "__main__":
    main()
