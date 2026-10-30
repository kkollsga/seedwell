#!/usr/bin/env bash
# Updater end to end (SPEC §12A), fully isolated: a copy of app/build/Seedwell.app with its own bundle id
# (io.github.kkollsga.Seedwell.e2e, so it shares no UserDefaults with an installed Seedwell), marked as a
# release build, with automatic install on, its own --state-dir/--home-root, installed in a temp folder.
# A fake 0.1.99 release is served on 127.0.0.1; the app must find it, run the curl installer, get quit
# by it and end up replaced by 0.1.99. The served install.sh has relaunch off in these two cases.
#
# Then development builds (SPEC 12A), on the same isolated copy with "Allow development builds" on: a build handed
# over with `seedwell update install` (kept app, watchdog, reopened with its own --state-dir); a broken build (its
# executable exits at once) put back by the watchdog and remembered as failed; a build that appears in a watched
# folder; and the switch turned off, which installs the (loopback) release although it is older than the build.
# Needs a built app (bash app/build.sh).
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

# copy <src> <dest> <version> [<dev version>]: our bundle id, release build (a development build X.Y.Z.N with the
# fourth argument), ad-hoc signed again.
copy_app() {
  rm -rf "$2"; ditto "$1" "$2"
  local pl="$2/Contents/Info.plist"
  /usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $E2E_ID" "$pl"
  /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $3" "$pl"
  /usr/libexec/PlistBuddy -c "Delete :SeedwellReleaseBuild" "$pl" 2>/dev/null || true
  /usr/libexec/PlistBuddy -c "Add :SeedwellReleaseBuild bool true" "$pl"
  /usr/libexec/PlistBuddy -c "Delete :SeedwellDevVersion" "$pl" 2>/dev/null || true
  [ -z "${4:-}" ] || /usr/libexec/PlistBuddy -c "Add :SeedwellDevVersion string $4" "$pl"
  codesign --force --deep -s - "$2" 2>/dev/null
  codesign --verify --deep --strict "$2"
}
OLD=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$APP/Contents/Info.plist")
copy_app "$APP" "$INST_APP" "$OLD"
copy_app "$APP" "$QA_ROOT/new/Seedwell.app" "$NEW"

ZIP="Seedwell-$NEW.zip"
ditto -c -k --sequesterRsrc --keepParent "$QA_ROOT/new/Seedwell.app" "$REL/download/v$NEW/$ZIP"
sed 's/^\(version=""; cli=0; \)launch=1; /\1launch=0; /' "$REPO/install.sh" > "$REL/download/v$NEW/install.sh"
grep -q '^version=""; cli=0; launch=0; ' "$REL/download/v$NEW/install.sh" || { echo "could not turn off relaunch in install.sh"; exit 1; }
(cd "$REL/download/v$NEW" && shasum -a 256 "$ZIP" install.sh > SHA256SUMS)
SHA=$(shasum -a 256 "$REL/download/v$NEW/$ZIP" | cut -d' ' -f1)
printf '{"version":"%s","tag":"v%s","notes_url":"%s/releases/tag/v%s","minimum_macos":"15.0","zip":"%s","sha256":"%s"}\n' \
  "$NEW" "$NEW" "$BASE" "$NEW" "$ZIP" "$SHA" > "$REL/latest/download/latest.json"

detach python3 -I -m http.server "$PORT" --bind 127.0.0.1 --directory "$QA_ROOT/rel" > "$QA_ROOT/http.log" 2>&1 &
sleep 1
curl -fsS "$BASE/releases/latest/download/latest.json" >/dev/null

LOG="$STATE/logs/update.log"
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
# The core's own record (updates.json in the state folder): checked a minute ago, $NEW on offer.
printf '{"last_check_ms":%s,"offered":"%s","notes":null,"pending":null}\n' "$(( $(date +%s) * 1000 - 60000 ))" "$NEW" > "$STATE/updates.json"
run_case "Launch of $OLD with $NEW already known and the daily check not due"

[ ! -e "$HOME/Library/Application Support/Seedwell/logs/update.log" ] \
  || [ "$HOME/Library/Application Support/Seedwell/logs/update.log" -ot "$QA_ROOT/http.log.mark" ] \
  || { echo "FAIL: wrote to the real app's update log"; exit 1; }
# ---------------------------------------------------------------- development builds (SPEC 12A)
CUR=$OLD   # the version the build is: development builds are $CUR.N
# The app only: the loopback release server stays up (the last case goes back to a release it serves).
pkill -f "^$INST_APP/Contents/MacOS/SeedwellApp" 2>/dev/null || true; sleep 1
rm -rf "$INST_APP" "$STATE" "$QA_ROOT/builds"; mkdir -p "$QA_ROOT/builds"
qa_write_config
printf '\n[updates]\ncheck = false\ndev_builds = true\n' >> "$STATE/config.toml"
LOG="$STATE/logs/update.log"

# What runs: the installed copy's version (a development build's X.Y.Z.N), whether it runs, and what it marked.
installed_version() {
  /usr/libexec/PlistBuddy -c "Print :SeedwellDevVersion" "$INST_APP/Contents/Info.plist" 2>/dev/null \
    || /usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$INST_APP/Contents/Info.plist"
}
running() { pgrep -f "^$INST_APP/Contents/MacOS/SeedwellApp" >/dev/null; }
started_as() { [ "$(installed_version)" = "$1" ] && running && [ "$(head -1 "$STATE/launch-ok" 2>/dev/null)" = "$1" ]; }
# wait_started <version> <secs> <label>
wait_started() {
  if ! qa_until "$2" started_as "$1"; then
    echo "--- update.log"; cat "$LOG" 2>/dev/null; echo "--- app.log"; tail -20 "$QA_ROOT/app.log"
    echo "FAIL ($3): installed $(installed_version), running $(running && echo yes || echo no), launch-ok $(cat "$STATE/launch-ok" 2>/dev/null)"
    exit 1
  fi
}
# build_zip <dev version> [broken]: a development build of the copy in $QA_ROOT/builds; broken: its executable exits.
build_zip() {
  local d="$QA_ROOT/b-$1"; rm -rf "$d"; mkdir -p "$d"
  copy_app "$APP" "$d/Seedwell.app" "$CUR" "$1"
  if [ "${2:-}" = broken ]; then
    printf '#!/bin/sh\nexit 1\n' > "$d/Seedwell.app/Contents/MacOS/SeedwellApp"
    chmod +x "$d/Seedwell.app/Contents/MacOS/SeedwellApp"
    codesign --force --deep -s - "$d/Seedwell.app" 2>/dev/null
  fi
  ditto -c -k --sequesterRsrc --keepParent "$d/Seedwell.app" "$QA_ROOT/builds/Seedwell-$1.zip"
  echo "$QA_ROOT/builds/Seedwell-$1.zip"
}
cli() { "$CLI" --socket "$STATE/control.sock" "$@"; }

echo "==> Development build $CUR.2 handed over with seedwell update install"
copy_app "$APP" "$INST_APP" "$CUR" "$CUR.1"
SEEDWELL_UPDATE_REPO="$BASE" SEEDWELL_WATCHDOG_SECS=75 detach "$INST_APP/Contents/MacOS/SeedwellApp" \
  --state-dir "$STATE" --home-root "$HOMEROOT" --qa-control "$CTL" --background > "$QA_ROOT/app.log" 2>&1 &
wait_started "$CUR.1" 60 "the first build starts and marks itself"
cli update | grep -q "Development builds: on" || { echo "FAIL: update status"; cli update; exit 1; }
cli update install "$(build_zip "$CUR.2")"
wait_started "$CUR.2" 150 "the handed-over build"
grep -q "\"version\":\"$CUR.1\"" "$STATE/previous/previous.json" || { echo "FAIL: $CUR.1 was not kept"; exit 1; }
echo "PASS (a handed-over build is installed, the one before kept)"

echo "==> A broken build $CUR.3: the watchdog puts $CUR.2 back"
cli update install "$(build_zip "$CUR.3" broken)"
qa_until 150 grep -q "did not start; the previous app was put back" "$LOG" || { echo "FAIL: no rollback"; cat "$LOG"; exit 1; }
wait_started "$CUR.2" 60 "the kept build is back"
cli update | grep -q "$CUR.3 didn't start; back on $CUR.2" || { echo "FAIL: it does not say so"; cli update; exit 1; }
grep -q "\"$CUR.3\"" "$STATE/updates.json" || { echo "FAIL: $CUR.3 not remembered as failed"; cat "$STATE/updates.json"; exit 1; }
echo "PASS (a build that does not start is replaced by the one before and not retried)"

echo "==> A watched folder: $CUR.4 appears and is installed"
mkdir -p "$QA_ROOT/watch"
cli update watch "$QA_ROOT/watch"
zip4=$(build_zip "$CUR.4")
SHA4=$(shasum -a 256 "$zip4" | cut -d' ' -f1)
cp "$zip4" "$QA_ROOT/watch/"
printf '{"version":"%s","minimum_macos":"15.0","zip":"Seedwell-%s.zip","sha256":"%s"}\n' "$CUR.4" "$CUR.4" "$SHA4" > "$QA_ROOT/watch/.latest.json.part"
mv "$QA_ROOT/watch/.latest.json.part" "$QA_ROOT/watch/latest.json"
wait_started "$CUR.4" 150 "the watched folder's build"
echo "PASS (a build put in the watched folder is installed)"

echo "==> Allow development builds off: back to the release $CUR (older than $CUR.4)"
copy_app "$APP" "$QA_ROOT/rel-app/Seedwell.app" "$CUR"
mkdir -p "$REL/download/v$CUR"
ZIP="Seedwell-$CUR.zip"
ditto -c -k --sequesterRsrc --keepParent "$QA_ROOT/rel-app/Seedwell.app" "$REL/download/v$CUR/$ZIP"
cp "$REPO/install.sh" "$REL/download/v$CUR/install.sh"
(cd "$REL/download/v$CUR" && shasum -a 256 "$ZIP" install.sh > SHA256SUMS)
SHA=$(shasum -a 256 "$REL/download/v$CUR/$ZIP" | cut -d' ' -f1)
printf '{"version":"%s","tag":"v%s","notes_url":"%s/releases/tag/v%s","minimum_macos":"15.0","zip":"%s","sha256":"%s"}\n' \
  "$CUR" "$CUR" "$BASE" "$CUR" "$ZIP" "$SHA" > "$REL/latest/download/latest.json"
sed -i '' 's/^dev_builds = true$/dev_builds = false/' "$STATE/config.toml"
wait_started "$CUR" 180 "back to the release"
/usr/libexec/PlistBuddy -c "Print :SeedwellDevVersion" "$INST_APP/Contents/Info.plist" >/dev/null 2>&1 && { echo "FAIL: still a development build"; exit 1; }
echo "PASS (turning the switch off installs the release)"

[ ! -e "$HOME/Library/Application Support/Seedwell/previous" ] || [ "$HOME/Library/Application Support/Seedwell/previous" -ot "$QA_ROOT/http.log.mark" ] \
  || { echo "FAIL: kept an app in the real app's state"; exit 1; }
defaults delete "$E2E_ID" 2>/dev/null || true
echo "PASS: all cases"
