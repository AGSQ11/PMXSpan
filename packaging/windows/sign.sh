#!/usr/bin/env bash
# Authenticode-signs a Windows binary in place with osslsigncode.
#
#   SIGN_PFX=cert.pfx [SIGN_PASS_FILE=pass.txt] packaging/windows/sign.sh file.exe
#
# The password is read from a file so it never shows up in the process list.
set -euo pipefail
: "${SIGN_PFX:?set SIGN_PFX to a PKCS#12 code-signing certificate}"
command -v osslsigncode >/dev/null || { echo "osslsigncode is required for signing" >&2; exit 1; }
f="$1"
args=(sign -pkcs12 "$SIGN_PFX" -n PMXSpan -h sha256 -ts "${SIGN_TS_URL:-http://timestamp.digicert.com}" -in "$f" -out "$f.signed")
[ -n "${SIGN_PASS_FILE:-}" ] && args+=(-readpass "$SIGN_PASS_FILE")
osslsigncode "${args[@]}"
mv "$f.signed" "$f"
