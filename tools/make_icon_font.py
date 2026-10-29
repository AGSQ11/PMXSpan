#!/usr/bin/env python3
"""Builds assets/fonts/PhosphorSubset.ttf and prints the matching Rust constants.

The app mixes Phosphor icons into ordinary text. Phosphor's own codepoints
(U+E000..) overlap the private-use glyphs Inter ships, so the icons we use are
subset out and moved to U+F700.., a range none of the bundled fonts use.

    pip install fonttools
    npm pack @phosphor-icons/web@2.1.1 && tar xzf phosphor-icons-web-2.1.1.tgz
    python3 tools/make_icon_font.py package/src/regular > icons.rs

Add an icon: append (CONST_NAME, phosphor-name) to ICONS (append only, so
existing codepoints stay stable), rerun, and paste the output into
`theme::icon`.
"""

import os
import re
import sys

from fontTools import subset
from fontTools.ttLib import TTFont

BASE = 0xF700
ICONS = [
    ("ARCHIVE", "archive"), ("REFRESH", "arrow-clockwise"), ("EXTERNAL", "arrow-square-out"), ("CAMERA", "camera"),
    ("CARET_DOWN", "caret-down"), ("CARET_RIGHT", "caret-right"), ("CHECK", "check"), ("CLOCK", "clock"),
    ("DOWNLOAD", "cloud-arrow-down"), ("SERVER", "computer-tower"), ("COPY", "copy"), ("CPU", "cpu"), ("CUBE", "cube"),
    ("DATABASE", "database"), ("DESKTOP", "desktop"), ("DISC", "disc"), ("DOTS", "dots-three"), ("SAVE", "floppy-disk"),
    ("FUNNEL", "funnel"), ("GEAR", "gear-six"), ("GLOBE", "globe"), ("HARD_DRIVE", "hard-drive"),
    ("HARD_DRIVES", "hard-drives"), ("HOUSE", "house"), ("INFO", "info"), ("KEY", "key"), ("LIST", "list"),
    ("TASKS", "list-checks"), ("SEARCH", "magnifying-glass"), ("MOON", "moon"), ("NETWORK", "network"),
    ("PACKAGE", "package"), ("PAUSE", "pause"), ("PLAY", "play"), ("PLUGS", "plugs-connected"), ("PLUS", "plus"),
    ("POWER", "power"), ("SHIELD", "shield-check"), ("SIGN_OUT", "sign-out"), ("SLIDERS", "sliders-horizontal"),
    ("DASHBOARD", "squares-four"), ("STACK", "stack"), ("STOP", "stop"), ("SUN", "sun"), ("SWAP", "swap"),
    ("TERMINAL", "terminal-window"), ("TRASH", "trash"), ("TREE", "tree-structure"), ("USERS", "users-three"),
    ("WARNING", "warning"), ("WRENCH", "wrench"), ("CLOSE", "x"), ("LOCK", "lock"), ("ARROW_RIGHT", "arrow-right"),
    ("PENCIL", "pencil-simple"), ("EYE", "eye"), ("CHART", "chart-line"), ("UPLOAD", "upload-simple"),
    ("CLOUD", "cloud"), ("CIRCLE_NOTCH", "circle-notch"), ("LIGHTNING", "lightning"), ("ROCKET", "rocket-launch"),
    ("IMAGE", "image"),
]


def main(src: str) -> None:
    css = open(os.path.join(src, "style.css")).read()
    codepoints = {m.group(1): int(m.group(2), 16)
                  for m in re.finditer(r'\.ph\.ph-([a-z0-9-]+):before\s*\{\s*content:\s*"\\([0-9a-f]+)"', css)}
    font = TTFont(os.path.join(src, "Phosphor.ttf"))
    cmap = font.getBestCmap()
    glyphs = [cmap[codepoints[name]] for _, name in ICONS]
    opts = subset.Options()
    opts.layout_features = []
    opts.name_IDs = ["*"]
    opts.notdef_outline = True
    opts.glyph_names = True
    sub = subset.Subsetter(opts)
    sub.populate(glyphs=glyphs)
    sub.subset(font)
    remapped = {BASE + i: cmap[codepoints[name]] for i, (_, name) in enumerate(ICONS)}
    for table in font["cmap"].tables:
        table.cmap = dict(remapped)
    font["cmap"].tables = [t for t in font["cmap"].tables if t.format == 4]
    out = os.path.join(os.path.dirname(__file__), "..", "assets", "fonts", "PhosphorSubset.ttf")
    font.save(out)
    for i, (const, _) in enumerate(ICONS):
        print(f'    pub const {const}: &str = "\\u{{{BASE + i:x}}}";')


if __name__ == "__main__":
    main(sys.argv[1])
