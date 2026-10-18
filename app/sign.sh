#!/usr/bin/env bash
# Signs Seedwell.app inside-out: the CLI, Sparkle's XPC services, updater and framework, then the app.
#   sign.sh <Seedwell.app>
# Identity: SEEDWELL_SIGN_IDENTITY ("Developer ID Application: Name (TEAMID)"); unset = ad-hoc ("-").
# Ad-hoc is for local testing only: Gatekeeper rejects it on every other Mac.
set -euo pipefail
APP="${1:?usage: sign.sh <Seedwell.app>}"
APP_DIR="$(cd "$(dirname "$0")" && pwd)"
ID="${SEEDWELL_SIGN_IDENTITY:--}"
BUNDLE_ID=io.github.kkollsga.Seedwell
ENT="$APP_DIR/Seedwell.entitlements"

if [ "$ID" = "-" ]; then
    echo "    Signing AD-HOC: NOT distributable (no Developer ID; Gatekeeper will refuse it elsewhere)."
    TS=(--timestamp=none)
    # No team id, so library validation would reject Sparkle.framework; only the ad-hoc build gets this.
    ENT_TMP="$(mktemp -t seedwell-ent).plist"
    printf '<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0"><dict><key>com.apple.security.cs.disable-library-validation</key><true/></dict></plist>\n' > "$ENT_TMP"
    ENT="$ENT_TMP"
    trap 'rm -f "$ENT_TMP"' EXIT
else
    echo "    Signing with: $ID"
    TS=(--timestamp)
fi
COMMON=(--force --sign "$ID" --options runtime "${TS[@]}")

FW="$APP/Contents/Frameworks/Sparkle.framework"
if [ -d "$FW" ]; then
    V="$FW/Versions/B"
    for x in "$V"/XPCServices/*.xpc; do [ -e "$x" ] && codesign "${COMMON[@]}" --preserve-metadata=entitlements "$x"; done
    [ -e "$V/Autoupdate" ] && codesign "${COMMON[@]}" "$V/Autoupdate"
    [ -e "$V/Updater.app" ] && codesign "${COMMON[@]}" "$V/Updater.app"
    codesign "${COMMON[@]}" "$FW"
fi
# The CLI runs on its own (agents and terminals launch `seedwell`), so it carries its own identifier.
codesign "${COMMON[@]}" --identifier "$BUNDLE_ID.cli" "$APP/Contents/MacOS/seedwell"
codesign "${COMMON[@]}" --identifier "$BUNDLE_ID" --entitlements "$ENT" "$APP"
codesign --verify --deep --strict --verbose=1 "$APP"
echo "    codesign --verify --deep --strict: ok"
