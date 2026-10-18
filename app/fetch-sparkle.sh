#!/usr/bin/env bash
# Fetches Sparkle 2.9.6 (framework + generate_keys / generate_appcast tools) into app/.sparkle, once.
# Pinned by version AND SHA-256, so it is as exact as a SwiftPM pin. Package.swift links
# .sparkle/Sparkle.xcframework as a binaryTarget; build.sh embeds the framework in the app.
#
# Why not `.package(url: ".../Sparkle", exact: "2.9.6")`? On the macOS 27 / Command Line Tools SwiftPM used
# here, "Downloading binary artifact" for that package hangs forever. This is the same release zip.
set -euo pipefail
VERSION=2.9.6
SHA256=8d5fb41d960b43f4a68aa14126bf62b098544ec8d191cdcc73eb14e63a8e7606
DEST="$(cd "$(dirname "$0")" && pwd)/.sparkle"
if [ -f "$DEST/VERSION" ] && [ "$(cat "$DEST/VERSION")" = "$VERSION" ] && [ -d "$DEST/Sparkle.xcframework" ]; then exit 0; fi
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
echo "==> Fetching Sparkle $VERSION"
curl -fsSL -o "$TMP/sparkle.zip" "https://github.com/sparkle-project/Sparkle/releases/download/$VERSION/Sparkle-for-Swift-Package-Manager.zip"
[ "$(shasum -a 256 "$TMP/sparkle.zip" | cut -d' ' -f1)" = "$SHA256" ] || { echo "Sparkle checksum mismatch" >&2; exit 1; }
rm -rf "$DEST"; mkdir -p "$DEST"
ditto -x -k "$TMP/sparkle.zip" "$DEST"
echo "$VERSION" > "$DEST/VERSION"
