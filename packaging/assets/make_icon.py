#!/usr/bin/env python3
"""Renders the PMXSpan app icon to pmxspan.png and pmxspan.ico.

The mark matches the in-app logo: the "hard drives" glyph from the bundled
icon font on a rounded square in the accent orange. The app embeds the PNG
as its window icon; the ICO goes into the Windows executable and installer.

    pip install pillow
    python3 packaging/assets/make_icon.py
"""
import os

from PIL import Image, ImageDraw, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
FONT = os.path.join(HERE, "..", "..", "assets", "fonts", "PhosphorSubset.ttf")
GLYPH = ""  # theme::icon::HARD_DRIVES
ACCENT = (0xF0, 0x7B, 0x1F, 255)
INK = (0x14, 0x17, 0x1D, 255)


def render(n: int) -> Image.Image:
    # Draw at 4x and downsample for smooth edges.
    s = n * 4
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    pad = round(s * 0.04)
    d.rounded_rectangle((pad, pad, s - pad, s - pad), radius=round(s * 0.22), fill=ACCENT)
    font = ImageFont.truetype(FONT, round(s * 0.62))
    # Centre the glyph's ink, not its advance box.
    left, top, right, bottom = d.textbbox((0, 0), GLYPH, font=font)
    d.text(((s - (right - left)) / 2 - left, (s - (bottom - top)) / 2 - top), GLYPH, font=font, fill=INK)
    return img.resize((n, n), Image.LANCZOS)


def main() -> None:
    png = os.path.join(HERE, "pmxspan.png")
    ico = os.path.join(HERE, "pmxspan.ico")
    big = render(256)
    big.save(png, optimize=True)
    big.save(ico, sizes=[(256, 256), (128, 128), (64, 64), (48, 48), (32, 32), (16, 16)])
    print("wrote", png, ico)


if __name__ == "__main__":
    main()
