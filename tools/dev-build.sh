#!/usr/bin/env bash
# A development build of the Mac app (SPEC 12A): the release build of this tree (release peer id, it checks and
# installs as a release does), numbered X.Y.Z.N: Cargo.toml's version and the next development number, counted in
# app/build/dev/counter from 1 for each version. The zip lands in app/build/dev/.
#
#   tools/sysguard/heavy.sh tools/dev-build.sh               build Seedwell-X.Y.Z.N.zip, print its path and SHA-256
#   tools/sysguard/heavy.sh tools/dev-build.sh --to <folder>  …and put it in a folder the app watches
#                                                             (`seedwell update watch <folder>`)
#   … --full                                                  full LTO, as a release (slower; default: thin)
#
# Installing it needs "Allow development builds" on in the app's Settings › General › Updates (the user's switch):
#   seedwell update install app/build/dev/Seedwell-X.Y.Z.N.zip
# A folder gets the zip first and latest.json last (written aside and renamed), so a half-copied build is never seen.
# One folder per system: the Windows builds (app-windows/dev-build.ps1) need a folder of their own, or each
# system's latest.json would replace the other's.
# Uncommitted changes are allowed; the commit is noted as <sha>-dirty. Over a Developer ID release, set
# SEEDWELL_SIGN_IDENTITY to that identity (see the note it prints).
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
to=""; full=0
while [ $# -gt 0 ]; do
  case "$1" in
    --to) to="${2:?--to needs a folder}"; shift ;;
    --full) full=1 ;;
    -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done
if [ -n "$to" ]; then [ -d "$to" ] || { echo "no such folder: $to" >&2; exit 1; }; to="$(cd "$to" && pwd)"; fi

VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' "$REPO/Cargo.toml" | head -1)"
OUT="$REPO/app/build/dev"
mkdir -p "$OUT"
read -r last n < "$OUT/counter" 2>/dev/null || { last=""; n=0; }
[ "$last" = "$VERSION" ] || n=0
n=$((n + 1))
DEV="$VERSION.$n"
COMMIT="$(git -C "$REPO" rev-parse --short HEAD)"
[ -z "$(git -C "$REPO" status --porcelain)" ] || COMMIT="$COMMIT-dirty"
echo "==> Seedwell $DEV (development build of $COMMIT)"

# Signed as the installed app is: with SEEDWELL_SIGN_IDENTITY (the release's Developer ID) the build reads that app's
# Keychain (Folder Sync and chat keys); ad hoc it can replace only an ad-hoc app (install.sh refuses it over a signed
# one, as it would start as a new device). Not notarized: a --zip install does not need it.
if [ -z "${SEEDWELL_SIGN_IDENTITY:-}" ]; then
  echo "note: no SEEDWELL_SIGN_IDENTITY: signed ad hoc, so it installs only over an ad-hoc Seedwell (not over a Developer ID release)"
fi
export SEEDWELL_RELEASE=1 SEEDWELL_DEV_VERSION="$DEV"
if [ "$full" = 1 ]; then export SEEDWELL_FAST=0; else export SEEDWELL_FAST=1; fi
"$REPO/app/build.sh"
APP="$REPO/app/build/Seedwell.app"
codesign --verify --deep --strict "$APP"
[ "$(/usr/libexec/PlistBuddy -c 'Print :SeedwellDevVersion' "$APP/Contents/Info.plist")" = "$DEV" ] || { echo "the app does not say $DEV" >&2; exit 1; }
# Counted only once it is built: a failed build does not use up a number.
echo "$VERSION $n" > "$OUT/counter"

ZIP="Seedwell-$DEV.zip"
rm -f "$OUT"/Seedwell-*.zip
ditto -c -k --sequesterRsrc --keepParent "$APP" "$OUT/$ZIP"
SHA="$(shasum -a 256 "$OUT/$ZIP" | cut -d' ' -f1)"
MIN_MACOS="$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' "$APP/Contents/Info.plist")"

if [ -n "$to" ]; then
  cp "$OUT/$ZIP" "$to/.$ZIP.part" && mv "$to/.$ZIP.part" "$to/$ZIP"
  printf '{"version":"%s","commit":"%s","minimum_macos":"%s","zip":"%s","sha256":"%s"}\n' "$DEV" "$COMMIT" "$MIN_MACOS" "$ZIP" "$SHA" > "$to/.latest.json.part"
  mv "$to/.latest.json.part" "$to/latest.json"
  # Older builds go: the newest is the one latest.json names.
  find "$to" -maxdepth 1 -name 'Seedwell-*.zip' ! -name "$ZIP" ! -name '*-windows-x64.zip' -delete
  echo "==> In $to (an app watching it installs it now)"
fi
echo "$OUT/$ZIP"
echo "sha256 $SHA"
echo "install: seedwell update install \"$OUT/$ZIP\" --sha256 $SHA"
