#!/usr/bin/env bash
# Builds every installer this machine can produce into dist/:
#   - Linux: pmxspan_<ver>_<arch>.deb, PMXSpan-<ver>-x86_64.AppImage and a portable .tar.gz
#   - Windows: PMXSpan-<ver>-Setup.exe and a portable .zip
#     (cross-compiled; needs the x86_64-pc-windows-gnu target, mingw-w64 and makensis)
#
# Set SIGN_PFX (and SIGN_PASS_FILE) to Authenticode-sign pmxspan.exe, the
# installer and the uninstaller with osslsigncode; see packaging/windows/sign.sh.
# macOS packages are built on a Mac with packaging/macos/build-app.sh.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
mkdir -p dist

echo "== Linux"
if command -v cargo-zigbuild >/dev/null && python3 -m ziglang version >/dev/null 2>&1; then
  # Link against glibc 2.31 so the builds run on Debian 11+, Ubuntu 20.04+
  # and other distributions of that age, not just the build machine's.
  cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.31
  LINBIN="$ROOT/target/x86_64-unknown-linux-gnu/release/pmxspan"
else
  echo "   (linking against this system's glibc; install zig + cargo-zigbuild for an older baseline)"
  cargo build --release
  LINBIN="$ROOT/target/release/pmxspan"
fi
packaging/linux/build-deb.sh "$LINBIN" dist
packaging/linux/build-appimage.sh "$LINBIN" dist
tar -C "$(dirname "$LINBIN")" -czf "dist/pmxspan-${VERSION}-linux-x86_64.tar.gz" pmxspan

if rustup target list --installed 2>/dev/null | grep -q x86_64-pc-windows-gnu && command -v makensis >/dev/null; then
  echo "== Windows"
  cargo build --release --target x86_64-pc-windows-gnu
  EXE="$ROOT/target/x86_64-pc-windows-gnu/release/pmxspan.exe"
  SIGN=()
  if [ -n "${SIGN_PFX:-}" ]; then
    packaging/windows/sign.sh "$EXE"
    SIGN=(-DSIGN_CMD="$ROOT/packaging/windows/sign.sh")
  else
    echo "   (unsigned: set SIGN_PFX to sign)"
  fi
  (cd packaging/windows && makensis -V2 -DVERSION="$VERSION" -DEXE="$EXE" -DOUTFILE="$ROOT/dist/PMXSpan-${VERSION}-Setup.exe" "${SIGN[@]}" installer.nsi)
  (cd "$(dirname "$EXE")" && rm -f "$ROOT/dist/pmxspan-${VERSION}-windows-x86_64-portable.zip" && \
    python3 -c "import zipfile,sys; z=zipfile.ZipFile(sys.argv[1],'w',zipfile.ZIP_DEFLATED); z.write('pmxspan.exe'); z.close()" \
    "$ROOT/dist/pmxspan-${VERSION}-windows-x86_64-portable.zip")
else
  echo "== Windows skipped (needs: rustup target add x86_64-pc-windows-gnu; mingw-w64; nsis)"
fi
ls -la dist
