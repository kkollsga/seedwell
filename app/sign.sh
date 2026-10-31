#!/usr/bin/env bash
# Signs Seedwell.app inside-out: the CLI, Sparkle's XPC services, updater and framework, then the app.
#   sign.sh <Seedwell.app>
# Identity: SEEDWELL_SIGN_IDENTITY ("Developer ID Application: Name (TEAMID)", or its SHA-1); unset = ad-hoc ("-").
# Ad-hoc is for local testing only: Gatekeeper rejects it on every other Mac.
set -euo pipefail
APP="${1:?usage: sign.sh <Seedwell.app>}"
APP_DIR="$(cd "$(dirname "$0")" && pwd)"
ID="${SEEDWELL_SIGN_IDENTITY:--}"
# codesign is handed the identity's SHA-1, never its name: a name with a non-ASCII letter (the "å" in the
# developer's) does not match the keychain's spelling of it. A name is looked up by its team in parentheses.
if [ "$ID" != "-" ] && ! [[ "$ID" =~ ^[0-9A-F]{40}$ ]]; then
    TEAM="${ID##*(}"; TEAM="${TEAM%)}"
    HASH="$(security find-identity -v -p codesigning | awk -v t="($TEAM)\"" 'index($0, "\"Developer ID Application:") && index($0, t) { print $2; exit }')"
    [ -n "$HASH" ] || { echo "no valid Developer ID Application identity of team $TEAM in the keychain" >&2; exit 1; }
    ID="$HASH"
fi
BUNDLE_ID=io.github.kkollsga.Seedwell
ENT="$APP_DIR/Seedwell.entitlements"
# The app group the app shares with its File Provider extension (Seedwell.entitlements has it too).
GROUP=6GND8Z2C7Z.io.github.kkollsga.Seedwell

if [ "$ID" = "-" ]; then
    echo "    Signing AD-HOC: NOT distributable (no Developer ID; Gatekeeper will refuse it elsewhere)."
    TS=(--timestamp=none)
    # No team id, so library validation would reject Sparkle.framework; only the ad-hoc build gets this.
    ENT_TMP="$(mktemp -t seedwell-ent).plist"
    printf '<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0"><dict><key>com.apple.security.cs.disable-library-validation</key><true/><key>com.apple.security.application-groups</key><array><string>%s</string></array></dict></plist>\n' "$GROUP" > "$ENT_TMP"
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
# The Finder extension: sandboxed (app extensions must be), reading only the folder the app writes its status to.
EXT="$APP/Contents/PlugIns/SeedwellFinderSync.appex"
if [ -d "$EXT" ]; then
    DIR="$(/usr/libexec/PlistBuddy -c 'Print :SeedwellStatusDir' "$EXT/Contents/Info.plist")"
    case "$DIR" in
        "~/"*) KEY=com.apple.security.temporary-exception.files.home-relative-path.read-only; DIR="${DIR#\~}" ;;
        /*) KEY=com.apple.security.temporary-exception.files.absolute-path.read-only ;;
        *) echo "SeedwellStatusDir must start with ~/ or /: $DIR" >&2; exit 1 ;;
    esac
    EXT_ENT="$(mktemp -t seedwell-ext-ent).plist"
    cat > "$EXT_ENT" <<ENT
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>com.apple.security.app-sandbox</key><true/>
    <key>$KEY</key><array><string>${DIR%/}/</string></array>
</dict></plist>
ENT
    codesign "${COMMON[@]}" --identifier "$BUNDLE_ID.FinderSync" --entitlements "$EXT_ENT" "$EXT"
    rm -f "$EXT_ENT"
fi
# The Share extension: sandboxed, with nothing beyond (it only hands a path to the app).
SHARE="$APP/Contents/PlugIns/SeedwellShare.appex"
if [ -d "$SHARE" ]; then
    SHARE_ENT="$(mktemp -t seedwell-share-ent).plist"
    printf '<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0"><dict><key>com.apple.security.app-sandbox</key><true/></dict></plist>\n' > "$SHARE_ENT"
    codesign "${COMMON[@]}" --identifier "$BUNDLE_ID.Share" --entitlements "$SHARE_ENT" "$SHARE"
    rm -f "$SHARE_ENT"
fi
# The File Provider extension (on-demand folders, SPEC 7G.11): sandboxed, with the app group, whose container holds
# the socket it asks the app through. No provisioning profile: the group is Team-prefixed.
FP="$APP/Contents/PlugIns/SeedwellFileProvider.appex"
if [ -d "$FP" ]; then
    FP_ENT="$(mktemp -t seedwell-fp-ent).plist"
    printf '<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0"><dict><key>com.apple.security.app-sandbox</key><true/><key>com.apple.security.application-groups</key><array><string>%s</string></array></dict></plist>\n' "$GROUP" > "$FP_ENT"
    codesign "${COMMON[@]}" --identifier "$BUNDLE_ID.FileProvider" --entitlements "$FP_ENT" "$FP"
    rm -f "$FP_ENT"
fi
# The CLI runs on its own (agents and terminals launch `seedwell`), so it carries its own identifier.
codesign "${COMMON[@]}" --identifier "$BUNDLE_ID.cli" "$APP/Contents/MacOS/seedwell"
codesign "${COMMON[@]}" --identifier "$BUNDLE_ID" --entitlements "$ENT" "$APP"
codesign --verify --deep --strict --verbose=1 "$APP"
echo "    codesign --verify --deep --strict: ok"
