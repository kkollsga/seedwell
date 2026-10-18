#!/usr/bin/env bash
# A new download takes the window and the menu (SPEC 8.2, 8.3): after a multi-file torrent finished and
# the window sits in Sharing (or a tracker tab), every way of adding a torrent that downloads must
#   - switch the main window to Downloads, select the new row (Get Info stays closed),
#   - show it at the top of the menu bar menu's Downloading section, at once, reopened and live.
# Paths: control-socket add (= open a .torrent / magnet link), watch folder, and the add popover
# (Start Now, Download/OK for a multi-file torrent, countdown expiry). Also: window closed while the
# torrent is added -> it opens on Downloads with the row selected; and Settings is not taken over
# (only a badge on Downloads).
# Captures: app/build/qa/newdl-*.png (before, after, menu). A local rate-limited seeder (headless seedwell
# daemon on 127.0.0.1) serves everything. Own QA root (seedwell-qa-newdl); only processes carrying that root
# are signalled.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-newdl"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
FAIL=0
[ -x "$OUT/winid" ] || swiftc -O -o "$OUT/winid" "$REPO/app/qa/winid.swift" 2>/dev/null
TRS="${TRS_DAEMON:-$REPO/target/release/seedwell}"
SEEDER="$QA_ROOT/seeder"
REPORT="$CTL.bench"
WATCH="$HOMEROOT/Watch"
qa_pid() { pgrep -f "SeedwellApp --state-dir $STATE" | head -1; }
add() { "$CLI" --socket "$SOCK" add "$@" >/dev/null 2>&1 || echo "add failed: $*"; }
big() { ls "$QA_ROOT"/big/torrents/*.torrent | sed -n "$1p"; }
where() { rm -f "$REPORT"; qa_cmd "where" >/dev/null; tail -1 "$REPORT" 2>/dev/null; }
menu_titles() { rm -f "$REPORT"; qa_cmd "menu-titles" >/dev/null; tail -1 "$REPORT" 2>/dev/null; }
check() { # label, output, expected substring
  case "$2" in *"$3"*) echo "  ok: $1 ($2)";; *) echo "  FAIL: $1: wanted '$3' in: $2"; FAIL=1;; esac
}
set_cfg() { python3 - "$STATE/config.toml" "$1" "$2" <<'PY'
import re, sys
p, k, v = sys.argv[1:4]
s = open(p).read()
s = re.sub(rf'(?m)^{k} = .*$', f'{k} = {v}', s)
open(p, 'w').write(s)
PY
sleep 5; }
menu_id() { for i in 1 2 3 4 5 6 7 8; do id=$("$OUT/winid" Seedwell | awk '$2>0 && $4 !~ /^3[0-9][0-9]x/ {print $1}' | head -1); [ -n "$id" ] && break; sleep 0.4; done; echo "$id"; }
shot_menu() { # name
  qa_cmd "menu open" >/dev/null; sleep 1.5
  local id; id=$(menu_id)
  [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/newdl-$1.png" && echo "  newdl-$1 (menu window $id)" || { echo "  FAIL: no menu window for $1"; FAIL=1; }
  MT=$(menu_titles)
  qa_cmd "menu close" >/dev/null
}
shot_main() { qa_shot "newdl-$1" >/dev/null; echo "  newdl-$1"; }
# Back to the start: the finished multi-file torrent selected in Sharing, no new download yet.
park() { qa_cmd "window open" "tab sharing" "select Night Harbor" >/dev/null; sleep 1; }
wait_for() { # secs, pattern in `where`
  for i in $(seq 1 "$1"); do w=$(where); case "$w" in *"$2"*) break;; esac; sleep 1; done; echo "$w"; }

if [ "${1:-}" != "reuse" ]; then
  pkill -f "SeedwellApp --state-dir $STATE" 2>/dev/null; pkill -f "seedwell daemon --state-dir $QA_ROOT" 2>/dev/null; sleep 1
  rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT" "$SEEDER" "$WATCH"
  QA_START_DELAY=0 qa_write_config
  cat >> "$STATE/config.toml" <<TOML
[[folders.watch]]
path = "~/Watch"
mode = "auto"
after_add = "leave"
TOML
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special season --start 1 >/dev/null
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/arch" --special season --start 2 --seed 9 >/dev/null
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/big" --count 8 --start 1000 --scale 80 >/dev/null
  cp "$QA_ROOT"/seed/torrents/*.torrent "$QA_ROOT/seed/data" 2>/dev/null
  printf 'version = 1\n[transfers]\nupload_limit = 2500000\n' > "$SEEDER/config.toml"
  TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/seeder-home" detach "$TRS" daemon --state-dir "$SEEDER" --download-dir "$QA_ROOT/seed/data" --listen-port 7201 --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$SEEDER/control.sock" >"$QA_ROOT/seeder.log" 2>&1 &
  for i in $(seq 1 40); do [ -S "$SEEDER/control.sock" ] && break; sleep 0.25; done
  for t in "$QA_ROOT"/seed/torrents/*.torrent; do "$TRS" --socket "$SEEDER/control.sock" add "$t" --save-path "$QA_ROOT/seed/data" >/dev/null; done
  for t in "$QA_ROOT"/arch/torrents/*.torrent; do "$TRS" --socket "$SEEDER/control.sock" add "$t" --save-path "$QA_ROOT/arch/data" >/dev/null; done
  for t in "$QA_ROOT"/big/torrents/*.torrent; do "$TRS" --socket "$SEEDER/control.sock" add "$t" --save-path "$QA_ROOT/big/data" >/dev/null; done
  qa_app_start --window-size 1180x820 --show-window
  sleep 2
  qa_cmd "appearance dark" >/dev/null
  echo "== a multi-file torrent downloads from the local seeder and finishes"
  add "$QA_ROOT"/seed/torrents/*Night*.torrent --peer 127.0.0.1:7201
  for i in $(seq 1 60); do "$CLI" --socket "$SOCK" list recent 2>/dev/null | grep -q "Night" && break; sleep 1; done
  "$CLI" --socket "$SOCK" list 2>&1 | head -4
fi

echo "== 1. window in Sharing, finished multi-file selected -> add a magnet-less torrent file (open .torrent / socket add)"
park; sleep 1
shot_main "before"
add "$(big 1)" --peer 127.0.0.1:7201
sleep 2
w=$(where); check "window follows the new download" "$w" "section downloads"
check "new row selected" "$w" "selection 1"
check "Get Info closed" "$w" "info none"
shot_main "after"
shot_menu "menu-new"
check "menu: Downloading section first" "$MT" "menu: Downloading|"

echo "== 2. window on a tracker tab -> watch folder"
qa_cmd "tab tracker Public" >/dev/null; sleep 1
cp "$(big 2)" "$WATCH/"
w=$(wait_for 12 "section downloads"); check "watch folder add switches" "$w" "section downloads"

echo "== 3. menu open (live) while a torrent is added"
park
qa_cmd "menu open" >/dev/null; sleep 1
add "$(big 3)" --peer 127.0.0.1:7201
sleep 3
t=$(menu_titles); echo "  live menu: $t"
check "live menu shows Downloading first" "$t" "menu: Downloading|"
id=$(menu_id); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/newdl-menu-live.png" && echo "  newdl-menu-live (menu window $id)"
qa_cmd "menu close" >/dev/null

echo "== 4. window closed -> opens on Downloads with the row selected"
park; qa_cmd "window close" >/dev/null; sleep 1
add "$(big 4)" --peer 127.0.0.1:7201
sleep 4
qa_cmd "window open" >/dev/null; sleep 1
w=$(where); check "opens on Downloads" "$w" "section downloads"; check "row selected" "$w" "selection 1"

echo "== 5. Settings is not taken over: only a badge on Downloads"
qa_cmd "settings general" >/dev/null; sleep 1
add "$(big 5)" --peer 127.0.0.1:7201
sleep 3
w=$(where); check "stays in settings" "$w" "section settings"; check "badge on Downloads" "$w" "badge 1"
shot_main "badge"
qa_cmd "tab downloads" >/dev/null; sleep 1; w=$(where); check "badge clears on arrival" "$w" "badge 0"

echo "== 6. add popover: countdown expiry, Start Now and OK"
set_cfg start_delay_secs 5
park
add "$(big 6)" --peer 127.0.0.1:7201
sleep 1; w=$(where); check "held torrent does not switch" "$w" "section sharing"
w=$(wait_for 15 "section downloads"); check "countdown expiry switches" "$w" "section downloads"
set_cfg start_delay_secs 120
park
add "$(big 7)" --peer 127.0.0.1:7201
sleep 2; qa_cmd "starting now" >/dev/null
w=$(wait_for 8 "section downloads"); check "Start Now switches" "$w" "section downloads"
set_cfg start_delay_secs 0
set_cfg ask_before_adding '"multi_file"'
park
add "$QA_ROOT"/arch/torrents/*.torrent --peer 127.0.0.1:7201
sleep 3; w=$(where); check "a torrent waiting for its file choice does not switch" "$w" "section sharing"
qa_cmd "starting all Night" "starting ok Night" >/dev/null
w=$(wait_for 10 "section downloads"); check "OK in the popover switches" "$w" "section downloads"
set_cfg ask_before_adding '"never"'

echo "== 7. a restart with downloads running does not count as new"
park
qa_cmd "quit" >/dev/null 2>&1; sleep 4
qa_app_start --window-size 1180x820 --show-window; sleep 3
qa_cmd "tab sharing" >/dev/null; sleep 6
w=$(where); check "window stays in Sharing after a launch" "$w" "section sharing"

"$CLI" --socket "$SOCK" list 2>&1 | head -8
[ $FAIL = 0 ] && echo "new download: PASS" || echo "new download: FAIL"
qa_cmd "quit" >/dev/null 2>&1; sleep 2
for pat in "SeedwellApp --state-dir $STATE" "seedwell daemon --state-dir $QA_ROOT"; do for pid in $(pgrep -f "$pat" 2>/dev/null); do kill "$pid" 2>/dev/null; done; done
exit $FAIL
