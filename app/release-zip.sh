#!/usr/bin/env bash
# The Mac's release assets: full-LTO build, signed app, zip, disk image, SHA256SUMS, install.sh, latest.json and the
# release notes. Everything lands in app/build/release/ ; nothing is published.
#
#   app/release-zip.sh [--allow-dirty]            a local check: signed ad hoc, not notarized (not distributable)
#   app/release-zip.sh --signed [--allow-dirty]   the release (release.yml): Developer ID signed and notarized
#
# --signed needs (docs/RELEASING.md, "One-time setup"):
#   SEEDWELL_SIGN_IDENTITY    "Developer ID Application: <name> (<team>)", of install.sh's TEAM_ID
#   SEEDWELL_NOTARY_PROFILE   a notarytool keychain profile (xcrun notarytool store-credentials)
#   SEEDWELL_NOTARY_KEYCHAIN  the keychain holding that profile, when it is not the default one (CI's temporary one,
#                             which is also in the search list, so codesign finds the identity there)
# It signs the app (hardened runtime, secure timestamp), notarizes and staples it, zips it, makes the disk image from
# it, signs, notarizes and staples that too, and checks every result with Gatekeeper (spctl).
#
# It never creates a release: a release is the Mac's and the Windows build together, made only by the release
# workflow (.github/workflows/release.yml) when both succeeded. A Mac-only release would leave every Windows copy
# without an update (docs/RELEASING.md).
#
# Wrap it in tools/sysguard/heavy.sh: the build is a full LTO build.
set -euo pipefail

APP_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$APP_DIR/.." && pwd)"
ALLOW_DIRTY=0; SIGNED=0
for a in "$@"; do
    case "$a" in
        --allow-dirty) ALLOW_DIRTY=1 ;;
        --signed) SIGNED=1 ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "unknown argument: $a" >&2; exit 2 ;;
    esac
done

if [ -n "$(git -C "$REPO" status --porcelain)" ]; then
    if [ "$ALLOW_DIRTY" = 1 ]; then echo "WARNING: dirty git tree (--allow-dirty): a test build, not a release."
    else echo "Refusing to release a dirty git tree. Commit first, or pass --allow-dirty for a test build." >&2; exit 1; fi
fi

# The one Team ID a release is signed by: install.sh's (it refuses any other on an update).
TEAM_ID="$(sed -n 's/^TEAM_ID=\([A-Z0-9]*\)$/\1/p' "$REPO/install.sh")"
[ -n "$TEAM_ID" ] || { echo "install.sh has no TEAM_ID" >&2; exit 1; }
if [ "$SIGNED" = 1 ]; then
    : "${SEEDWELL_SIGN_IDENTITY:?--signed needs SEEDWELL_SIGN_IDENTITY (docs/RELEASING.md)}"
    : "${SEEDWELL_NOTARY_PROFILE:?--signed needs SEEDWELL_NOTARY_PROFILE (docs/RELEASING.md)}"
    case "$SEEDWELL_SIGN_IDENTITY" in
        "Developer ID Application: "*"($TEAM_ID)") ;;
        *) echo "SEEDWELL_SIGN_IDENTITY must be a Developer ID Application identity of team $TEAM_ID: $SEEDWELL_SIGN_IDENTITY" >&2; exit 1 ;;
    esac
    security find-identity -v -p codesigning | grep -qF "\"$SEEDWELL_SIGN_IDENTITY\"" \
        || { echo "the identity is not in the keychain: $SEEDWELL_SIGN_IDENTITY" >&2; exit 1; }
else
    # A local check: ad hoc, whatever the environment says.
    unset SEEDWELL_SIGN_IDENTITY
fi

VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' "$REPO/Cargo.toml" | head -1)"
TAG="v$VERSION"
echo "==> Seedwell $VERSION ($TAG), commit $(git -C "$REPO" rev-parse --short HEAD)"

# Full LTO; build.sh signs with SEEDWELL_SIGN_IDENTITY (sign.sh: hardened runtime, secure timestamp), else ad hoc.
export SEEDWELL_FAST=0
# Release peer id `-SW<ver>-` instead of the dev `-SX0001-` (trs-metainfo PEER_ID_PREFIX).
export SEEDWELL_RELEASE=1
"$APP_DIR/build.sh"

APP="$APP_DIR/build/Seedwell.app"
OUT="$APP_DIR/build/release"
ZIP="Seedwell-$VERSION.zip"
DMG="Seedwell-$VERSION.dmg"
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
rm -rf "$OUT"; mkdir -p "$OUT"

echo "==> Verify the app"
codesign --verify --deep --strict "$APP"
for f in "$APP/Contents/MacOS/"*; do
    lipo -archs "$f" | grep -qx arm64 || { echo "unexpected architectures in $f: $(lipo -archs "$f")" >&2; exit 1; }
done

team_of() { codesign -dv --verbose=2 "$1" 2>&1 | sed -n 's/^TeamIdentifier=//p'; }

# notarize <file>: submits it and waits for Apple's answer; anything but Accepted fails with Apple's log.
notarize() {
    local answer status id
    answer="$(xcrun notarytool submit "$1" --keychain-profile "$SEEDWELL_NOTARY_PROFILE" \
        ${SEEDWELL_NOTARY_KEYCHAIN:+--keychain "$SEEDWELL_NOTARY_KEYCHAIN"} --wait --timeout 45m --output-format json)" || true
    status="$(printf '%s' "$answer" | python3 -I -c 'import json,sys; print(json.load(sys.stdin).get("status",""))' 2>/dev/null || true)"
    id="$(printf '%s' "$answer" | python3 -I -c 'import json,sys; print(json.load(sys.stdin).get("id",""))' 2>/dev/null || true)"
    echo "    notarization of $(basename "$1"): ${status:-no answer} ${id:+(submission $id)}"
    if [ "$status" != Accepted ]; then
        [ -z "$id" ] || xcrun notarytool log "$id" --keychain-profile "$SEEDWELL_NOTARY_PROFILE" \
            ${SEEDWELL_NOTARY_KEYCHAIN:+--keychain "$SEEDWELL_NOTARY_KEYCHAIN"} >&2 || true
        echo "notarization failed: ${answer:-no answer from notarytool}" >&2
        exit 1
    fi
}

if [ "$SIGNED" = 1 ]; then
    echo "==> Developer ID: team $(team_of "$APP")"
    [ "$(team_of "$APP")" = "$TEAM_ID" ] || { echo "the app is not signed by team $TEAM_ID" >&2; exit 1; }
    echo "==> Notarize and staple the app (waits for Apple)"
    ditto -c -k --sequesterRsrc --keepParent "$APP" "$WORK/notarize.zip"
    notarize "$WORK/notarize.zip"
    xcrun stapler staple "$APP"
    xcrun stapler validate "$APP"
    spctl --assess --type execute --verbose=2 "$APP"
fi

echo "==> Zip"
# ditto keeps the bundle's symlinks, permissions, signature and stapled ticket intact (zip(1) can break them).
ditto -c -k --sequesterRsrc --keepParent "$APP" "$OUT/$ZIP"

echo "==> Disk image"
# dmgbuild lays out the window (the app, a link to Applications, the background with its arrow, the volume icon)
# without scripting the Finder; pinned with its dependencies by SHA-256.
python3 -m venv "$WORK/dmgbuild"
"$WORK/dmgbuild/bin/pip" install --quiet --disable-pip-version-check --require-hashes --no-deps --only-binary :all: \
    -r "$APP_DIR/dmg/requirements.txt"
swiftc -O "$APP_DIR/dmg/make_background.swift" -o "$WORK/make_background" 2>&1 | tail -3
"$WORK/make_background" "$WORK/background.png" "$WORK/background@2x.png"
tiffutil -cathidpicheck "$WORK/background.png" "$WORK/background@2x.png" -out "$WORK/background.tiff" >/dev/null
"$WORK/dmgbuild/bin/dmgbuild" -s "$APP_DIR/dmg/settings.py" -D app="$APP" -D background="$WORK/background.tiff" \
    -D icon="$APP/Contents/Resources/AppIcon.icns" "Seedwell $VERSION" "$OUT/$DMG" >/dev/null
if [ "$SIGNED" = 1 ]; then
    codesign --force --sign "$SEEDWELL_SIGN_IDENTITY" --timestamp "$OUT/$DMG"
    echo "==> Notarize and staple the disk image (waits for Apple)"
    notarize "$OUT/$DMG"
    xcrun stapler staple "$OUT/$DMG"
    xcrun stapler validate "$OUT/$DMG"
    spctl --assess --type open --context context:primary-signature --verbose=2 "$OUT/$DMG"
fi
# The same disk image without a version in its name: <repo>/releases/latest/download/Seedwell.dmg is a stable link.
cp "$OUT/$DMG" "$OUT/Seedwell.dmg"

cp "$REPO/install.sh" "$OUT/install.sh"
(cd "$OUT" && shasum -a 256 "$ZIP" install.sh "$DMG" Seedwell.dmg > SHA256SUMS)
# latest.json: what the app's updater reads (<repo>/releases/latest/download/latest.json). Not in SHA256SUMS;
# install.sh verifies the zip itself. Every value comes from this script.
REPO_URL="${SEEDWELL_REPO_URL:-https://github.com/kkollsga/seedwell}"
ZIP_SHA="$(cd "$OUT" && shasum -a 256 "$ZIP" | cut -d' ' -f1)"
MIN_MACOS="$(sed -n 's/.*<key>LSMinimumSystemVersion<\/key><string>\(.*\)<\/string>.*/\1/p' "$APP/Contents/Info.plist" | head -1)"
printf '{"version":"%s","tag":"%s","notes_url":"%s/releases/tag/%s","minimum_macos":"%s","zip":"%s","sha256":"%s"}\n' \
    "$VERSION" "$TAG" "$REPO_URL" "$TAG" "$MIN_MACOS" "$ZIP" "$ZIP_SHA" > "$OUT/latest.json"

echo "==> Check the installer against the zip and the disk image (local, no network)"
CHECK="$WORK/check"; mkdir -p "$CHECK"
(cd "$OUT" && shasum -a 256 -c SHA256SUMS >/dev/null)
ditto -x -k "$OUT/$ZIP" "$CHECK"
codesign --verify --deep --strict "$CHECK/Seedwell.app"
if [ "$SIGNED" = 1 ]; then
    [ "$(team_of "$CHECK/Seedwell.app")" = "$TEAM_ID" ] || { echo "the zipped app lost its signature" >&2; exit 1; }
    xcrun stapler validate "$CHECK/Seedwell.app"
    spctl --assess --type execute "$CHECK/Seedwell.app"
fi
MNT="$WORK/mnt"; mkdir -p "$MNT"
hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$MNT" "$OUT/$DMG" >/dev/null
codesign --verify --deep --strict "$MNT/Seedwell.app" && [ "$(readlink "$MNT/Applications")" = /Applications ] \
    || { hdiutil detach "$MNT" -quiet || true; echo "the disk image does not hold the app and the Applications link" >&2; exit 1; }
hdiutil detach "$MNT" -quiet
bash -n "$OUT/install.sh"
[ -n "$MIN_MACOS" ] || { echo "no LSMinimumSystemVersion in the app" >&2; exit 1; }
plutil -lint "$OUT/latest.json" >/dev/null 2>&1 || python3 -I -c 'import json,sys; json.load(open(sys.argv[1]))' "$OUT/latest.json"

# The release notes' list: each public commit since `prev`, newest first. A release commit ("Seedwell X.Y.Z: …")
# gives the bullets of its message (wrapped lines joined), or its subject when it has none; only the newest one per
# version counts (a release cut again). Other commits (Dependabot's) give their subject.
changes() {
    local repo="$1" prev="$2" end="${3:-HEAD}" seen="" c subject key body
    for c in $(git -C "$repo" rev-list "$prev..$end"); do
        subject="$(git -C "$repo" log -1 --format=%s "$c")"
        key="$(printf '%s' "$subject" | sed -nE 's/^(Seedwell [0-9]+\.[0-9]+\.[0-9]+).*/\1/p')"
        if [ -z "$key" ]; then
            echo "- $subject" | cut -c1-200
            continue
        fi
        case "$seen" in *"|$key|"*) continue ;; esac
        seen="$seen|$key|"
        body="$(git -C "$repo" log -1 --format=%b "$c" | awk '
            /^- / { if (b != "") print b; b = $0; next }
            /^[ \t]+[^ \t]/ && b != "" { sub(/^[ \t]+/, ""); b = b " " $0; next }
            { if (b != "") print b; b = "" }
            END { if (b != "") print b }')"
        if [ -n "$body" ]; then printf '%s\n' "$body"; else echo "- $subject" | cut -c1-200; fi
    done
}

# The tag before this commit (on a tagged commit, describe of HEAD would name the commit's own tag).
PREV="$(git -C "$REPO" describe --tags --abbrev=0 "HEAD^" 2>/dev/null || true)"
{
    echo "## Install"
    echo
    echo "**Mac** (macOS 15 or later, Apple silicon): download [$DMG]($REPO_URL/releases/download/$TAG/$DMG), open it and"
    echo "drag Seedwell to Applications. Or in Terminal:"
    echo
    echo '```sh'
    echo "curl -fsSL https://github.com/kkollsga/seedwell/releases/latest/download/install.sh | bash"
    echo '```'
    echo
    echo "Seedwell updates itself (Settings › General › Updates); the Terminal command also updates it."
    echo
    echo "## Changes"
    echo
    if [ -n "$PREV" ]; then changes "$REPO" "$PREV"; else echo "- First release."; fi
    echo
    echo "## Checksums"
    echo
    echo '```'
    cat "$OUT/SHA256SUMS"
    echo '```'
} > "$OUT/NOTES.md"

echo "==> Assets in $OUT"
ls -l "$OUT" | tail -n +2 | awk '{printf "    %-28s %s\n", $NF, $5}'

echo "    A release is made only by the release workflow (tag vX.Y.Z), with the Windows build: docs/RELEASING.md."
