#!/usr/bin/env bash
# Builds PMXSpan.app as a universal binary (Apple silicon + Intel) and packs
# it as a .dmg and a .zip. Run on macOS with Xcode command line tools:
#
#   rustup target add aarch64-apple-darwin x86_64-apple-darwin
#   packaging/macos/build-app.sh [out-dir]
#
# The app is ad-hoc signed, which is enough to run it after confirming the
# first launch (right-click > Open). Set MACOS_SIGN_IDENTITY to sign with a
# Developer ID instead.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="${1:-$ROOT/dist}"
cd "$ROOT"
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"

cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
APP="$STAGE/PMXSpan.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create -output "$APP/Contents/MacOS/pmxspan" \
  target/aarch64-apple-darwin/release/pmxspan target/x86_64-apple-darwin/release/pmxspan

ICONSET="$STAGE/pmxspan.iconset"
mkdir -p "$ICONSET"
for s in 16 32 128 256; do
  sips -z $s $s packaging/assets/pmxspan.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s * 2)) $((s * 2)) packaging/assets/pmxspan.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/pmxspan.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>PMXSpan</string>
  <key>CFBundleDisplayName</key><string>PMXSpan</string>
  <key>CFBundleIdentifier</key><string>com.pmxspan.PMXSpan</string>
  <key>CFBundleExecutable</key><string>pmxspan</string>
  <key>CFBundleIconFile</key><string>pmxspan</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

if [ -n "${MACOS_SIGN_IDENTITY:-}" ]; then
  codesign --force --deep --options runtime --timestamp -s "$MACOS_SIGN_IDENTITY" "$APP"
else
  codesign --force --deep -s - "$APP"
fi

mkdir -p "$OUT"
ditto -c -k --keepParent "$APP" "$OUT/PMXSpan-${VERSION}-macos-universal.zip"
DMG="$STAGE/dmg"
mkdir -p "$DMG"
cp -R "$APP" "$DMG/"
ln -s /Applications "$DMG/Applications"
hdiutil create -volname "PMXSpan ${VERSION}" -srcfolder "$DMG" -ov -format UDZO "$OUT/PMXSpan-${VERSION}-macos-universal.dmg"
