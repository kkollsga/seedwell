#!/usr/bin/env bash
# The menu bar menu's downloading rows with a real speed and time left ("42% · 3.1 MB/s · 4 min left", SPEC 8.2):
# two big downloads from a rate-limited local seeder (a headless seedwell daemon on 127.0.0.1), one torrent with
# no source (it says why: "Waiting for peers"). The menu is opened by QAControl and captured by its own window
# id (QA_PID limits winid to this QA app); a second capture a few seconds later must differ (the rows refresh
# once a second while the menu is open). Capture: app/build/qa/menu-downloading-speed.png.
# Own QA root (seedwell-qa-menu); only processes carrying that root are signalled.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-menu"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
FAIL=0
[ -x "$OUT/winid" ] || swiftc -O -o "$OUT/winid" "$REPO/app/qa/winid.swift" 2>/dev/null
TRS="${TRS_DAEMON:-$REPO/target/release/seedwell}"   # a seedwell binary with the engine (the app bundle's CLI is client-only)
SEEDER="$QA_ROOT/seeder"

pkill -f "SeedwellApp --state-dir $STATE" 2>/dev/null; pkill -f "seedwell daemon --state-dir $QA_ROOT" 2>/dev/null; sleep 1
rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT" "$SEEDER"
qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --count 2 --start 1000 --scale 60 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/orphan" --count 1 --start 3000 --scale 30 >/dev/null
printf 'version = 1\n[transfers]\nupload_limit = 1500000\n' > "$SEEDER/config.toml"
TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/seeder-home" detach "$TRS" daemon --state-dir "$SEEDER" --download-dir "$QA_ROOT/seed/data" --listen-port 7201 --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$SEEDER/control.sock" >"$QA_ROOT/seeder.log" 2>&1 &
for i in $(seq 1 40); do [ -S "$SEEDER/control.sock" ] && break; sleep 0.25; done
for t in "$QA_ROOT/seed/torrents"/*.torrent; do "$TRS" --socket "$SEEDER/control.sock" add "$t" --save-path "$QA_ROOT/seed/data" >/dev/null; done
qa_app_start --window-size 1180x820 --show-window
for t in "$QA_ROOT/seed/torrents"/*.torrent; do "$CLI" --socket "$SOCK" add "$t" --peer 127.0.0.1:7201 >/dev/null; done
"$CLI" --socket "$SOCK" add "$QA_ROOT/orphan/torrents"/*.torrent >/dev/null
sleep 25
QA_PID=$(pgrep -f "SeedwellApp --state-dir $STATE" | head -1); export QA_PID
qa_cmd "appearance dark" "menu open" >/dev/null; sleep 1.5
menu_id() { for i in 1 2 3 4 5 6 7 8; do id=$("$OUT/winid" Seedwell | awk '$2>0 {print $1}' | head -1); [ -n "$id" ] && break; sleep 0.4; done; echo "$id"; }
id=$(menu_id)
if [ -n "$id" ]; then
  screencapture -x -o -l "$id" "$OUT/menu-downloading-speed.png" && echo "  menu-downloading-speed (menu window $id)"
  sleep 4
  screencapture -x -o -l "$id" "$OUT/menu-downloading-speed-later.png"
  if cmp -s "$OUT/menu-downloading-speed.png" "$OUT/menu-downloading-speed-later.png"; then echo "   FAIL: the open menu did not change in 4 s"; FAIL=1; else echo "   ok: the open menu refreshed live"; fi
else
  echo "   FAIL: no menu window"; FAIL=1
fi
qa_cmd "menu close" >/dev/null
"$CLI" --socket "$SOCK" list 2>&1 | head -6
[ $FAIL = 0 ] && echo "menu speed: PASS" || echo "menu speed: FAIL"
qa_cmd "quit" >/dev/null 2>&1; sleep 2
for pat in "SeedwellApp --state-dir $STATE" "seedwell daemon --state-dir $QA_ROOT"; do for pid in $(pgrep -f "$pat" 2>/dev/null); do kill "$pid" 2>/dev/null; done; done
exit $FAIL
