#!/usr/bin/env bash
# The menu bar menu's Sharing line with the combined upload rate at its right edge ("–" while idle), the
# Folder Sync line only once a folder is synced, and Settings › Folder Sync with the editable sync port and
# its router line, before and after the port moves (SPEC 8.2, 7G.5). Captures into
# app/build/qa/sync-port-*.png. Own QA root (seedwell-qa-syncport); only processes carrying it are signalled.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-syncport"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
FAIL=0
[ -x "$OUT/winid" ] || swiftc -O -o "$OUT/winid" "$REPO/app/qa/winid.swift" 2>/dev/null
main_id() { "$OUT/winid" Seedwell | awk '$2==0 && $4 ~ /^1[0-9][0-9][0-9]x/ {print $1; exit}'; }
menu_id() { for i in 1 2 3 4 5 6 7 8; do id=$("$OUT/winid" Seedwell | awk '$2>0 {print $1}' | head -1); [ -n "$id" ] && break; sleep 0.4; done; echo "$id"; }
titles() { : > "$CTL.bench"; qa_cmd "menu-titles" >/dev/null; grep '^menu:' "$CTL.bench" | tail -1; }
shot_menu() {
  local id=""
  # The first open after the app starts can be swallowed: try again.
  for try in 1 2 3; do
    qa_cmd "activate" "menu open" >/dev/null; sleep 1.2
    id=$(menu_id); [ -n "$id" ] && break
    qa_cmd "menu close" >/dev/null
  done
  if [ -n "$id" ]; then screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"; else echo "   FAIL: no menu window"; FAIL=1; fi
}

qa_app_stop; sleep 1
rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT"
qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --count 2 --start 1000 >/dev/null
qa_app_start --window-size 1180x820
QA_PID=$(pgrep -f "SeedwellApp --state-dir $STATE" | head -1); export QA_PID
qa_add_all "$QA_ROOT/seed/torrents" "$QA_ROOT/seed/data"
for i in $(seq 1 40); do "$CLI" --socket "$SOCK" sync list >/dev/null 2>&1 && break; sleep 0.5; done
sleep 4
qa_cmd "appearance dark" >/dev/null

# No synced folder: no Folder Sync line; the Sharing line carries the idle rate.
shot_menu sync-port-menu-no-folders
t=$(titles); echo "   $t"
case "$t" in *"Folder Sync"*) echo "   FAIL: Folder Sync shown with no folders"; FAIL=1;; *) echo "   ok: no Folder Sync line";; esac
case "$t" in *"Sharing 2 torrents –"*) echo "   ok: Sharing line with the idle rate";; *) echo "   FAIL: no Sharing line with its rate"; FAIL=1;; esac
qa_cmd "menu close" >/dev/null

# One synced folder: the line is there.
mkdir -p "$HOMEROOT/Docs"; echo hi > "$HOMEROOT/Docs/a.txt"
"$CLI" --socket "$SOCK" sync add "$HOMEROOT/Docs" >/dev/null
sleep 1.5
shot_menu sync-port-menu-one-folder
t=$(titles)
case "$t" in *"Folder Sync"*) echo "   ok: Folder Sync line with a folder";; *) echo "   FAIL: no Folder Sync line with a folder: $t"; FAIL=1;; esac
qa_cmd "menu close" >/dev/null

# Settings › Folder Sync: the port field, then the port moved.
qa_cmd "window open" "window size 1180x820" "settings folderSync" >/dev/null; sleep 1.5
qa_cmd "activate main" >/dev/null; sleep 0.5
id=$(main_id); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/sync-port-settings.png" && echo "  sync-port-settings"
before=$("$CLI" --socket "$SOCK" sync list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["port"])')
want=$(python3 -c 'import socket; s=socket.socket(); s.bind(("0.0.0.0",0)); print(s.getsockname()[1])')
"$CLI" --socket "$SOCK" sync settings --port "$want" | grep -i port
sleep 3
now=$("$CLI" --socket "$SOCK" sync list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["port"])')
[ "$now" = "$want" ] && echo "   ok: port $before -> $now" || { echo "   FAIL: port is $now, wanted $want"; FAIL=1; }
qa_cmd "activate main" >/dev/null; sleep 0.5
id=$(main_id); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/sync-port-settings-moved.png" && echo "  sync-port-settings-moved"

[ $FAIL = 0 ] && echo "sync port: PASS" || echo "sync port: FAIL"
qa_cmd "quit" >/dev/null 2>&1; sleep 2
for pid in $(pgrep -f "SeedwellApp --state-dir $STATE" 2>/dev/null); do kill "$pid" 2>/dev/null; done
exit $FAIL
