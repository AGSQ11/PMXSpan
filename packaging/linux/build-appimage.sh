#!/usr/bin/env bash
# Builds PMXSpan-<ver>-x86_64.AppImage from an already-built release binary.
#
#   packaging/linux/build-appimage.sh [path/to/pmxspan] [out-dir]
#
# Uses appimagetool from $APPIMAGETOOL or PATH, and otherwise downloads it
# once into ~/.cache/pmxspan.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${1:-$ROOT/target/release/pmxspan}"
OUT="${2:-$ROOT/dist}"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"

[ -x "$BIN" ] || { echo "binary not found: $BIN (run cargo build --release first)" >&2; exit 1; }

TOOL="${APPIMAGETOOL:-$(command -v appimagetool || true)}"
if [ -z "$TOOL" ]; then
  TOOL="${XDG_CACHE_HOME:-$HOME/.cache}/pmxspan/appimagetool-x86_64.AppImage"
  if [ ! -x "$TOOL" ]; then
    mkdir -p "$(dirname "$TOOL")"
    curl -fsSL -o "$TOOL.part" https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$TOOL.part" && mv "$TOOL.part" "$TOOL"
  fi
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
APPDIR="$STAGE/PMXSpan.AppDir"

install -Dm755 "$BIN" "$APPDIR/usr/bin/pmxspan"
install -Dm644 "$ROOT/packaging/assets/pmxspan.png" "$APPDIR/usr/share/icons/hicolor/256x256/apps/pmxspan.png"
install -Dm644 "$ROOT/packaging/linux/pmxspan.desktop" "$APPDIR/usr/share/applications/pmxspan.desktop"
for f in "$ROOT"/assets/fonts/*-LICENSE.txt; do
  install -Dm644 "$f" "$APPDIR/usr/share/doc/pmxspan/licenses/$(basename "$f")"
done
cp "$ROOT/packaging/linux/pmxspan.desktop" "$APPDIR/pmxspan.desktop"
cp "$ROOT/packaging/assets/pmxspan.png" "$APPDIR/pmxspan.png"
ln -s pmxspan.png "$APPDIR/.DirIcon"
ln -s usr/bin/pmxspan "$APPDIR/AppRun"

mkdir -p "$OUT"
# Extract-and-run: works without FUSE (containers, CI).
ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$TOOL" --no-appstream "$APPDIR" "$OUT/PMXSpan-${VERSION}-x86_64.AppImage"
