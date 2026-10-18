#!/usr/bin/env bash
# Builds the Sparkle appcast for a folder of release disk images.
#   app/make-appcast.sh [releases-folder]      (default: app/build/release)
# Put every Seedwell-<version>.dmg you have shipped in the folder (keep old ones: deltas are made from them),
# run this, then upload the folder's appcast.xml (and the dmgs, plus any deltas) to where SUFeedURL points.
# generate_appcast signs each archive with the private EdDSA key in your login keychain (the one
# `generate_keys` created, docs/RELEASING.md). Set SEEDWELL_DOWNLOAD_URL_PREFIX (e.g. https://kkollsga.github.io/seedwell/releases/) when the dmgs are not next to the appcast.
set -euo pipefail
APP_DIR="$(cd "$(dirname "$0")" && pwd)"
DIR="${1:-$APP_DIR/build/release}"
"$APP_DIR/fetch-sparkle.sh"
TOOL="$APP_DIR/.sparkle/bin/generate_appcast"
[ -d "$DIR" ] || { echo "no such folder: $DIR" >&2; exit 1; }
ls "$DIR"/*.dmg >/dev/null 2>&1 || { echo "no .dmg in $DIR" >&2; exit 1; }
echo "==> generate_appcast over $DIR"
"$TOOL" "$DIR" ${SEEDWELL_DOWNLOAD_URL_PREFIX:+--download-url-prefix "$SEEDWELL_DOWNLOAD_URL_PREFIX"}
echo "Wrote $DIR/appcast.xml"
