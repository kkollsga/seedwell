#!/usr/bin/env bash
# Release for the curl installer (no Developer ID yet): full-LTO build, ad-hoc signed app, zip,
# SHA256SUMS, install.sh, release notes. Everything lands in app/build/release/ ; nothing is published.
#
#   app/release-zip.sh [--allow-dirty]          build the assets
#   app/release-zip.sh --draft [--allow-dirty]  …and create a DRAFT GitHub release with them (gh CLI);
#                                               publishing the draft is a manual click.
#
# Wrap it in tools/sysguard/heavy.sh: the build is a full LTO build. The signed .dmg route is app/release.sh.
set -euo pipefail

APP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$APP_DIR/.." && pwd)"
ALLOW_DIRTY=0; DRAFT=0
for a in "$@"; do
    case "$a" in
        --allow-dirty) ALLOW_DIRTY=1 ;;
        --draft) DRAFT=1 ;;
        -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
        *) echo "unknown argument: $a" >&2; exit 2 ;;
    esac
done

if [ -n "$(git -C "$REPO" status --porcelain)" ]; then
    if [ "$ALLOW_DIRTY" = 1 ]; then echo "WARNING: dirty git tree (--allow-dirty): a test build, not a release."
    else echo "Refusing to release a dirty git tree. Commit first, or pass --allow-dirty for a test build." >&2; exit 1; fi
    [ "$DRAFT" = 0 ] || { echo "--draft needs a clean tree." >&2; exit 1; }
fi

VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' "$REPO/Cargo.toml" | head -1)"
TAG="v$VERSION"
echo "==> Seedwell $VERSION ($TAG), commit $(git -C "$REPO" rev-parse --short HEAD)"

# Full LTO; build.sh signs ad hoc (sign.sh) when no Developer ID is set.
unset SEEDWELL_SIGN_IDENTITY
export SEEDWELL_FAST=0
# Release peer id `-SW<ver>-` instead of the dev `-SX0001-` (trs-metainfo PEER_ID_PREFIX).
export SEEDWELL_RELEASE=1
"$APP_DIR/build.sh"

APP="$APP_DIR/build/Seedwell.app"
OUT="$APP_DIR/build/release"
ZIP="Seedwell-$VERSION.zip"
rm -rf "$OUT"; mkdir -p "$OUT"

echo "==> Verify the app"
codesign --verify --deep --strict "$APP"
for f in "$APP/Contents/MacOS/"*; do
    lipo -archs "$f" | grep -qx arm64 || { echo "unexpected architectures in $f: $(lipo -archs "$f")" >&2; exit 1; }
done

echo "==> Zip"
# ditto keeps the bundle's symlinks, permissions and signature intact (zip(1) can break them).
ditto -c -k --sequesterRsrc --keepParent "$APP" "$OUT/$ZIP"
cp "$REPO/install.sh" "$OUT/install.sh"
(cd "$OUT" && shasum -a 256 "$ZIP" install.sh > SHA256SUMS)
# latest.json: what the app's updater reads (<repo>/releases/latest/download/latest.json). Not in SHA256SUMS;
# install.sh verifies the zip itself. Every value comes from this script.
REPO_URL="${SEEDWELL_REPO_URL:-https://github.com/kkollsga/seedwell}"
ZIP_SHA="$(cd "$OUT" && shasum -a 256 "$ZIP" | cut -d' ' -f1)"
MIN_MACOS="$(sed -n 's/.*<key>LSMinimumSystemVersion<\/key><string>\(.*\)<\/string>.*/\1/p' "$APP/Contents/Info.plist" | head -1)"
printf '{"version":"%s","tag":"%s","notes_url":"%s/releases/tag/%s","minimum_macos":"%s","zip":"%s","sha256":"%s"}\n' \
    "$VERSION" "$TAG" "$REPO_URL" "$TAG" "$MIN_MACOS" "$ZIP" "$ZIP_SHA" > "$OUT/latest.json"

echo "==> Check the installer against the zip (local, no network)"
CHECK="$(mktemp -d)"; trap 'rm -rf "$CHECK"' EXIT
(cd "$OUT" && grep " $ZIP\$" SHA256SUMS | shasum -a 256 -c - >/dev/null)
ditto -x -k "$OUT/$ZIP" "$CHECK"
codesign --verify --deep --strict "$CHECK/Seedwell.app"
bash -n "$OUT/install.sh"
[ -n "$MIN_MACOS" ] || { echo "no LSMinimumSystemVersion in the app" >&2; exit 1; }
plutil -lint "$OUT/latest.json" >/dev/null 2>&1 || python3 -I -c 'import json,sys; json.load(open(sys.argv[1]))' "$OUT/latest.json"

# The tag before this commit (on a tagged commit, describe of HEAD would name the commit's own tag).
PREV="$(git -C "$REPO" describe --tags --abbrev=0 "HEAD^" 2>/dev/null || true)"
{
    echo "## Install"
    echo
    echo '```sh'
    echo "curl -fsSL https://github.com/kkollsga/seedwell/releases/latest/download/install.sh | bash"
    echo '```'
    echo
    echo "Requires macOS 15 or later on Apple silicon. Run the same command again to update."
    echo
    echo "## Changes"
    echo
    if [ -n "$PREV" ]; then git -C "$REPO" log --format='- %s' "$PREV"..HEAD | cut -c1-200; else echo "- First release."; fi
    echo
    echo "## Checksums"
    echo
    echo '```'
    cat "$OUT/SHA256SUMS"
    echo '```'
} > "$OUT/NOTES.md"

echo "==> Assets in $OUT"
ls -l "$OUT" | tail -n +2 | awk '{printf "    %-28s %s\n", $NF, $5}'

if [ "$DRAFT" = 1 ]; then
    command -v gh >/dev/null || { echo "gh (GitHub CLI) is not installed." >&2; exit 1; }
    echo "==> Draft release $TAG (not published)"
    gh release create "$TAG" --draft --title "Seedwell $VERSION" --notes-file "$OUT/NOTES.md" \
        "$OUT/$ZIP" "$OUT/install.sh" "$OUT/SHA256SUMS" "$OUT/latest.json"
    echo "    Review it on GitHub and press Publish. The install URL works once it is published."
else
    echo "    Next: app/release-zip.sh --draft (creates a draft GitHub release), then publish it on GitHub."
fi
