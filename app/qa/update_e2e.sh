#!/usr/bin/env bash
# Updater end to end (SPEC §12A), fully isolated: a copy of app/build/Seedwell.app with its own bundle id
# (io.github.kkollsga.Seedwell.e2e, so it shares no UserDefaults with an installed Seedwell), marked as a
# release build, with automatic install on, its own --state-dir/--home-root, installed in a temp folder.
# A fake 0.1.99 release is served on 127.0.0.1; the app must find it, run the curl installer, get quit
# by it and end up replaced by 0.1.99. The served install.sh has relaunch off (a relaunch would start
# the copy without --state-dir). Needs a built app (bash app/build.sh).
set -euo pipefail
export QA_ROOT=/private/tmp/claude-501/seedwell-qa-update
source "$(dirname "$0")/env.sh"
qa_guard
PORT=7199
BASE="http://127.0.0.1:$PORT"
NEW=0.1.99
INST="$QA_ROOT/inst"
REL="$QA_ROOT/rel/releases"
E2E_ID=io.github.kkollsga.Seedwell.e2e
INST_APP="$INST/Seedwell.app"

cleanup() {
  pkill -f "^$INST_APP/Contents/MacOS/SeedwellApp" 2>/dev/null || true
  pkill -f "http.server $PORT --bind 127.0.0.1" 2>/dev/null || true
}
trap cleanup EXIT
cleanup; sleep 1
defaults delete "$E2E_ID" 2>/dev/null || true   # the copy's own preferences domain, never the real app's
rm -rf "$QA_ROOT"; mkdir -p "$INST" "$REL/latest/download" "$REL/download/v$NEW"
qa_write_config
printf '\n[updates]\ncheck = true\nauto_install = true\n' >> "$STATE/config.toml"

# copy <src> <dest> <version>: our bundle id, release build, ad-hoc signed again.
copy_app() {
  ditto "$1" "$2"
  local pl="$2/Contents/Info.plist"
  /usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $E2E_ID" "$pl"
  /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $3" "$pl"
  /usr/libexec/PlistBuddy -c "Delete :SeedwellReleaseBuild" "$pl" 2>/dev/null || true
  /usr/libexec/PlistBuddy -c "Add :SeedwellReleaseBuild bool true" "$pl"
  codesign --force --deep -s - "$2" 2>/dev/null
  codesign --verify --deep --strict "$2"
}
OLD=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$APP/Contents/Info.plist")
copy_app "$APP" "$INST_APP" "$OLD"
copy_app "$APP" "$QA_ROOT/new/Seedwell.app" "$NEW"

ZIP="Seedwell-$NEW.zip"
ditto -c -k --sequesterRsrc --keepParent "$QA_ROOT/new/Seedwell.app" "$REL/download/v$NEW/$ZIP"
sed 's/^version=""; cli=0; launch=1; uninstall=0$/version=""; cli=0; launch=0; uninstall=0/' "$REPO/install.sh" > "$REL/download/v$NEW/install.sh"
grep -q 'launch=0; uninstall=0' "$REL/download/v$NEW/install.sh" || { echo "could not turn off relaunch in install.sh"; exit 1; }
(cd "$REL/download/v$NEW" && shasum -a 256 "$ZIP" install.sh > SHA256SUMS)
SHA=$(shasum -a 256 "$REL/download/v$NEW/$ZIP" | cut -d' ' -f1)
printf '{"version":"%s","tag":"v%s","notes_url":"%s/releases/tag/v%s","minimum_macos":"15.0","zip":"%s","sha256":"%s"}\n' \
  "$NEW" "$NEW" "$BASE" "$NEW" "$ZIP" "$SHA" > "$REL/latest/download/latest.json"

detach python3 -I -m http.server "$PORT" --bind 127.0.0.1 --directory "$QA_ROOT/rel" > "$QA_ROOT/http.log" 2>&1 &
sleep 1
curl -fsS "$BASE/releases/latest/download/latest.json" >/dev/null

LOG="$HOMEROOT/Library/Logs/Seedwell/update.log"
# run_case <label>: launch the installed copy and wait until it has been replaced by $NEW and has quit.
run_case() {
  echo "==> $1"
  : > "$QA_ROOT/http.log.mark"; local from; from=$(wc -l < "$QA_ROOT/http.log")
  SEEDWELL_UPDATE_REPO="$BASE" detach "$INST_APP/Contents/MacOS/SeedwellApp" \
    --state-dir "$STATE" --home-root "$HOMEROOT" --qa-control "$CTL" --background > "$QA_ROOT/app.log" 2>&1 &
  sleep 3
  pgrep -f "^$INST_APP/Contents/MacOS/SeedwellApp" >/dev/null || { echo "FAIL: the app did not start"; cat "$QA_ROOT/app.log"; exit 1; }
  local v=""
  for _ in $(seq 1 240); do
    v=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$INST_APP/Contents/Info.plist" 2>/dev/null || true)
    if [ "$v" = "$NEW" ] && ! pgrep -f "^$INST_APP/Contents/MacOS/SeedwellApp" >/dev/null; then break; fi
    sleep 1
  done
  echo "--- update.log"; cat "$LOG" 2>/dev/null || echo "(no log at $LOG)"
  echo "--- requests by the app"; tail -n +"$((from + 1))" "$QA_ROOT/http.log" | grep -o '"GET [^"]*" [0-9]*' || true
  [ "$v" = "$NEW" ] || { echo "FAIL ($1): installed version is '$v', expected $NEW"; exit 1; }
  pgrep -f "^$INST_APP/Contents/MacOS/SeedwellApp" >/dev/null && { echo "FAIL ($1): the old app is still running"; exit 1; }
  codesign --verify --deep --strict "$INST_APP"
  echo "PASS ($1)"
}

run_case "Fresh launch of $OLD: the daily check is due (about 60 s), finds $NEW and installs it"

# An update found earlier, the daily check not due: the launch must still install it.
rm -rf "$INST_APP" "$LOG"; copy_app "$APP" "$INST_APP" "$OLD"
defaults write "$E2E_ID" update.lastCheck -date "$(date -u '+%Y-%m-%d %H:%M:%S +0000')"
defaults write "$E2E_ID" update.lastVersion "$NEW"
defaults delete "$E2E_ID" update.pending 2>/dev/null || true
run_case "Launch of $OLD with $NEW already known and the daily check not due"

[ ! -e "$HOME/Library/Logs/Seedwell/update.log" ] || [ "$HOME/Library/Logs/Seedwell/update.log" -ot "$QA_ROOT/http.log.mark" ] \
  || { echo "FAIL: wrote to the real ~/Library/Logs"; exit 1; }
defaults delete "$E2E_ID" 2>/dev/null || true
echo "PASS: both cases"
