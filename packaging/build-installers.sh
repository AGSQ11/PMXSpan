#!/usr/bin/env bash
# Builds every installer this machine can produce into dist/:
#   - Linux: pmxspan_<ver>_<arch>.deb and a portable .tar.gz
#   - Windows: PMXSpan-<ver>-Setup.exe and a portable .zip
#     (cross-compiled; needs the x86_64-pc-windows-gnu target, mingw-w64 and makensis)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
mkdir -p dist

echo "== Linux"
cargo build --release
packaging/linux/build-deb.sh target/release/pmxspan dist
tar -C target/release -czf "dist/pmxspan-${VERSION}-linux-x86_64.tar.gz" pmxspan

if rustup target list --installed 2>/dev/null | grep -q x86_64-pc-windows-gnu && command -v makensis >/dev/null; then
  echo "== Windows"
  cargo build --release --target x86_64-pc-windows-gnu
  EXE="$ROOT/target/x86_64-pc-windows-gnu/release/pmxspan.exe"
  (cd packaging/windows && makensis -V2 -DVERSION="$VERSION" -DEXE="$EXE" -DOUTFILE="$ROOT/dist/PMXSpan-${VERSION}-Setup.exe" installer.nsi)
  (cd "$(dirname "$EXE")" && rm -f "$ROOT/dist/pmxspan-${VERSION}-windows-x86_64-portable.zip" && \
    python3 -c "import zipfile,sys; z=zipfile.ZipFile(sys.argv[1],'w',zipfile.ZIP_DEFLATED); z.write('pmxspan.exe'); z.close()" \
    "$ROOT/dist/pmxspan-${VERSION}-windows-x86_64-portable.zip")
else
  echo "== Windows skipped (needs: rustup target add x86_64-pc-windows-gnu; mingw-w64; nsis)"
fi
ls -la dist
