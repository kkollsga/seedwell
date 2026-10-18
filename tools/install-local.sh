#!/usr/bin/env bash
# Update the Seedwell installed on this Mac from the repo: build, swap into /Applications, relaunch.
#
#   tools/install-local.sh              build the committed HEAD (refuses a dirty tree), install, relaunch
#   tools/install-local.sh --allow-dirty   include uncommitted changes (a dev build, labelled as such)
#   tools/install-local.sh --no-build      install the app already built in app/build
#   tools/install-local.sh --rollback      put the previous install back
#
# Quitting: SIGTERM, which Seedwell handles as a proper quit (engine shutdown, trackers told
# `stopped`), waiting up to QUIT_WAIT seconds (default 120, the app's own quit linger cap); only then
# a force kill. The previous app is kept at $BACKUP for --rollback.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
DEST=/Applications/Seedwell.app
BACKUP="${SEEDWELL_BACKUP:-$HOME/Library/Caches/Seedwell-dev/Seedwell.previous.app}"
BUILT="$REPO/app/build/Seedwell.app"
EXE_NAME=SeedwellApp
QUIT_WAIT="${QUIT_WAIT:-120}"
HEAVY="$REPO/tools/sysguard/heavy.sh"

build=1; dirty=""; rollback=0
for a in "$@"; do
  case "$a" in
    --no-build) build=0 ;;
    --allow-dirty) dirty="--allow-dirty" ;;
    --rollback) rollback=1 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done

running_pid() { pgrep -f "^$DEST/Contents/MacOS/$EXE_NAME" | head -1 || true; }
version_of() { /usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" -c "Print :CFBundleVersion" "$1/Contents/Info.plist" 2>/dev/null | paste -sd' ' -; }

quit_running() {
  local pid; pid=$(running_pid)
  [ -z "$pid" ] && { echo "==> Seedwell is not running"; return; }
  echo "==> Quitting Seedwell (pid $pid), up to ${QUIT_WAIT}s for trackers to hear 'stopped'"
  kill -TERM "$pid" 2>/dev/null || true
  for _ in $(seq 1 "$QUIT_WAIT"); do kill -0 "$pid" 2>/dev/null || { echo "    quit cleanly"; return; }; sleep 1; done
  echo "    still running after ${QUIT_WAIT}s: force-closing"
  kill -KILL "$pid" 2>/dev/null || true
  sleep 1
}

launch_and_check() {
  # --background: an update relaunch stays in the menu bar (a launch from the Dock opens the window).
  open "$DEST" --args --background
  for _ in $(seq 1 30); do [ -n "$(running_pid)" ] && break; sleep 0.5; done
  if [ -n "$(running_pid)" ]; then echo "==> Running: Seedwell $(version_of "$DEST") (pid $(running_pid))"
  else echo "Seedwell did not start; see Console.app (subsystem io.github.kkollsga.Seedwell)" >&2; exit 1; fi
}

if [ "$rollback" = 1 ]; then
  [ -d "$BACKUP" ] || { echo "no backup at $BACKUP" >&2; exit 1; }
  echo "==> Rolling back to $(version_of "$BACKUP")"
  quit_running
  rm -rf "$DEST.new"; ditto "$BACKUP" "$DEST.new"
  rm -rf "$DEST"; mv "$DEST.new" "$DEST"
  launch_and_check; exit 0
fi

if [ "$build" = 1 ]; then
  echo "==> Building (release)"
  log="$(mktemp -t seedwell-build)"
  if ! "$HEAVY" "$REPO/app/release.sh" $dirty > "$log" 2>&1; then
    tail -20 "$log" >&2; echo "build failed (full log: $log); nothing installed" >&2; exit 1
  fi
  grep -E "^Seedwell|AD-HOC" "$log" || true
fi
[ -d "$BUILT" ] || { echo "no build at $BUILT" >&2; exit 1; }
codesign --verify --deep --strict "$BUILT"
echo "==> New: Seedwell $(version_of "$BUILT")   installed: $( [ -d "$DEST" ] && version_of "$DEST" || echo none)"

# Stage next to the destination first, so the swap itself is a quick rename.
rm -rf "$DEST.new"; ditto "$BUILT" "$DEST.new"
quit_running
if [ -d "$DEST" ]; then mkdir -p "$(dirname "$BACKUP")"; rm -rf "$BACKUP"; mv "$DEST" "$BACKUP"; fi
mv "$DEST.new" "$DEST"
launch_and_check
echo "    previous version kept for 'tools/install-local.sh --rollback'"
