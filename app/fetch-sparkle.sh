#!/usr/bin/env bash
# Fetches Sparkle 2.10.1 (framework + generate_keys / generate_appcast tools) into app/.sparkle, once.
# Pinned by version AND SHA-256, so it is as exact as a SwiftPM pin. Package.swift links
# .sparkle/Sparkle.xcframework as a binaryTarget; build.sh embeds the framework in the app.
#
# Why not `.package(url: ".../Sparkle", exact: "2.10.1")`? On the macOS 27 / Command Line Tools SwiftPM used
# here, "Downloading binary artifact" for that package hangs forever. This is the same release zip.
set -euo pipefail
VERSION=2.10.1
SHA256=4c2e48a3e50ebcc2c88a58a4c7264a1a99c3c31857ce754357afb9cdb323022a
DEST="$(cd "$(dirname "$0")" && pwd)/.sparkle"
if [ -f "$DEST/VERSION" ] && [ "$(cat "$DEST/VERSION")" = "$VERSION" ] && [ -d "$DEST/Sparkle.xcframework" ]; then exit 0; fi
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
echo "==> Fetching Sparkle $VERSION"
curl -fsSL -o "$TMP/sparkle.zip" "https://github.com/sparkle-project/Sparkle/releases/download/$VERSION/Sparkle-for-Swift-Package-Manager.zip"
[ "$(shasum -a 256 "$TMP/sparkle.zip" | cut -d' ' -f1)" = "$SHA256" ] || { echo "Sparkle checksum mismatch" >&2; exit 1; }
rm -rf "$DEST"; mkdir -p "$DEST"
ditto -x -k "$TMP/sparkle.zip" "$DEST"
echo "$VERSION" > "$DEST/VERSION"
