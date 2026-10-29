#!/usr/bin/env bash
# Builds a Debian/Ubuntu package from an already-built release binary.
#
#   packaging/linux/build-deb.sh [path/to/proxmoxpanel] [out-dir]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${1:-$ROOT/target/release/proxmoxpanel}"
OUT="${2:-$ROOT/dist}"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
ARCH="$(dpkg --print-architecture 2>/dev/null || echo amd64)"

[ -x "$BIN" ] || { echo "binary not found: $BIN (run cargo build --release first)" >&2; exit 1; }

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
PKG="$STAGE/proxmoxpanel_${VERSION}_${ARCH}"

install -Dm755 "$BIN" "$PKG/usr/bin/proxmoxpanel"
install -Dm644 "$ROOT/packaging/assets/proxmoxpanel.png" "$PKG/usr/share/icons/hicolor/256x256/apps/proxmoxpanel.png"
install -Dm644 "$ROOT/packaging/linux/proxmoxpanel.desktop" "$PKG/usr/share/applications/proxmoxpanel.desktop"
install -Dm644 "$ROOT/README.md" "$PKG/usr/share/doc/proxmoxpanel/README.md"
for f in "$ROOT"/assets/fonts/*-LICENSE.txt; do
  install -Dm644 "$f" "$PKG/usr/share/doc/proxmoxpanel/licenses/$(basename "$f")"
done

mkdir -p "$PKG/DEBIAN"
cat > "$PKG/DEBIAN/control" <<EOF
Package: proxmoxpanel
Version: $VERSION
Section: admin
Priority: optional
Architecture: $ARCH
Depends: libxkbcommon-x11-0, libgl1 | libvulkan1, xdg-utils
Recommends: xdg-desktop-portal, virt-viewer
Maintainer: ProxmoxPanel <noreply@localhost>
Description: Standalone desktop manager for Proxmox VE clusters
 Manage Proxmox VE nodes, VMs and containers: provisioning with
 auto-placement and IPAM, rebuilds, snapshots, backups, firewall,
 migration, HA, users/pools and more.
EOF
cat > "$PKG/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q /usr/share/applications || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true
EOF
chmod 755 "$PKG/DEBIAN/postinst"

mkdir -p "$OUT"
dpkg-deb --build --root-owner-group "$PKG" "$OUT/proxmoxpanel_${VERSION}_${ARCH}.deb"
