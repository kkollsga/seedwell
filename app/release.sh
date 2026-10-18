#!/usr/bin/env bash
# Scripted release: full-LTO build, sign (hardened runtime), .dmg, notarize + staple, verify.
#   app/release.sh [--allow-dirty]
# Environment (docs/RELEASING.md):
#   SEEDWELL_SIGN_IDENTITY   "Developer ID Application: Your Name (TEAMID)". Unset = ad-hoc, NOT distributable.
#   SEEDWELL_NOTARY_PROFILE  keychain profile from `xcrun notarytool store-credentials`. Unset = no notarization.
#   SEEDWELL_SPARKLE_PUBLIC_KEY / app/sparkle_public_key   EdDSA public key for Sparkle (build.sh).
# Wrap it in tools/sysguard/heavy.sh: the build is a full LTO build.
set -euo pipefail

APP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$APP_DIR/.." && pwd)"
ALLOW_DIRTY=0
for a in "$@"; do
    case "$a" in
        --allow-dirty) ALLOW_DIRTY=1 ;;
        -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
        *) echo "unknown argument: $a" >&2; exit 2 ;;
    esac
done

if [ -n "$(git -C "$REPO" status --porcelain)" ]; then
    if [ "$ALLOW_DIRTY" = 1 ]; then echo "WARNING: dirty git tree (--allow-dirty); this build is not reproducible from a commit."
    else echo "Refusing to release a dirty git tree. Commit first, or pass --allow-dirty for a test build." >&2; exit 1; fi
fi

VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' "$REPO/Cargo.toml" | head -1)"
BUILD_NUMBER="$(git -C "$REPO" rev-list --count HEAD)"
# Every crate inherits the workspace version; warn if one ever gets its own literal again.
for f in "$REPO"/crates/*/Cargo.toml; do
    v="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$f" | head -1)"
    [ -z "$v" ] || [ "$v" = "$VERSION" ] || echo "WARNING: $f has its own version $v (workspace: $VERSION)"
done
echo "==> Seedwell $VERSION (build $BUILD_NUMBER), commit $(git -C "$REPO" rev-parse --short HEAD)"

REAL_ID=0
if [ -n "${SEEDWELL_SIGN_IDENTITY:-}" ] && [ "$SEEDWELL_SIGN_IDENTITY" != "-" ]; then
    REAL_ID=1
    security find-identity -v -p codesigning | grep -F "$SEEDWELL_SIGN_IDENTITY" >/dev/null \
        || { echo "Identity not found in the keychain: $SEEDWELL_SIGN_IDENTITY (security find-identity -v -p codesigning)" >&2; exit 1; }
else
    unset SEEDWELL_SIGN_IDENTITY
    echo "WARNING: SEEDWELL_SIGN_IDENTITY is not set: signing AD-HOC. The result is NOT distributable."
fi

# Full LTO, never the fast profile; signing happens inside build.sh via sign.sh.
export SEEDWELL_FAST=0
"$APP_DIR/build.sh"

BUILD="$APP_DIR/build"
APP="$BUILD/Seedwell.app"
OUT="$BUILD/release"
DMG="$OUT/Seedwell-$VERSION.dmg"
mkdir -p "$OUT"; rm -f "$DMG"

echo "==> Disk image"
STAGE="$(mktemp -d "$BUILD/dmg-stage.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT
ditto "$APP" "$STAGE/Seedwell.app"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "Seedwell $VERSION" -srcfolder "$STAGE" -fs HFS+ -format UDZO -imagekey zlib-level=9 -ov "$DMG" >/dev/null
if [ "$REAL_ID" = 1 ]; then codesign --force --sign "$SEEDWELL_SIGN_IDENTITY" --timestamp "$DMG"; fi

if [ "$REAL_ID" = 1 ] && [ -n "${SEEDWELL_NOTARY_PROFILE:-}" ]; then
    echo "==> Notarizing (waits for Apple)"
    xcrun notarytool submit "$DMG" --keychain-profile "$SEEDWELL_NOTARY_PROFILE" --wait
    xcrun stapler staple "$DMG"
    xcrun stapler validate "$DMG"
    echo "==> Gatekeeper"
    spctl -a -vv -t install "$DMG"
elif [ "$REAL_ID" = 1 ]; then
    echo "SKIPPED notarization: SEEDWELL_NOTARY_PROFILE is not set (xcrun notarytool store-credentials, docs/RELEASING.md)."
    echo "         Without it, Gatekeeper warns on every other Mac."
else
    echo "SKIPPED notarization: needs a real Developer ID identity."
fi

echo "==> Verify"
codesign --verify --deep --strict --verbose=1 "$APP"
SIZE_APP="$(du -sk "$APP" | cut -f1)"; SIZE_DMG="$(du -k "$DMG" | cut -f1)"
echo
echo "Seedwell $VERSION (build $BUILD_NUMBER)"
echo "  app: $APP (${SIZE_APP} KB)"
echo "  dmg: $DMG (${SIZE_DMG} KB)"
[ "$REAL_ID" = 1 ] || echo "  AD-HOC SIGNED: NOT distributable."
