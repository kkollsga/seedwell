#!/usr/bin/env bash
# A finished torrent that gets more files is a download again (SPEC 8.2, 8.3, 7B.2): the user first picks
# S01 of a private two-season pack, it finishes and seeds (Recent / Sharing); then S02 is ticked in the
# file tree. The row must move to Downloads (progress, speed, time left), the menu must list it under
# Downloading, the window must switch to Downloads (the user re-selected files), and the local mock
# tracker must see the true `left`, no second `started` and `completed` exactly once.
# Captures app/build/qa/redl-*.png. A local rate-limited seeder (headless daemon on 127.0.0.1) and a local
# mock tracker (qa_tracker.py on 7105) serve it. Own QA root (seedwell-qa-redl); only processes carrying
# that root are signalled.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-redl"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
FAIL=0
[ -x "$OUT/winid" ] || swiftc -O -o "$OUT/winid" "$REPO/app/qa/winid.swift" 2>/dev/null
SEEDER="$QA_ROOT/seeder"
TRS="${TRS_DAEMON:-$REPO/target/release/seedwell}"
REPORT="$CTL.bench"
TLOG="$QA_ROOT/trk/7105.log"
where() { rm -f "$REPORT"; qa_cmd "where" >/dev/null; tail -1 "$REPORT" 2>/dev/null; }
menu_titles() { rm -f "$REPORT"; qa_cmd "menu-titles" >/dev/null; tail -1 "$REPORT" 2>/dev/null; }
check() { case "$2" in *"$3"*) echo "  ok: $1 ($2)";; *) echo "  FAIL: $1: wanted '$3' in: $2"; FAIL=1;; esac; }
count() { grep -c "$1" "$TLOG" 2>/dev/null || true; }
menu_id() { for i in 1 2 3 4 5 6 7 8; do id=$("$OUT/winid" Seedwell | awk '$2>0 && $4 !~ /^3[0-9][0-9]x/ {print $1}' | head -1); [ -n "$id" ] && break; sleep 0.4; done; echo "$id"; }
shot_menu() { # name
  qa_cmd "menu open" >/dev/null; sleep 1.5
  local id; id=$(menu_id)
  [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/redl-$1.png" && echo "  redl-$1 (menu window $id)" || { echo "  FAIL: no menu window for $1"; FAIL=1; }
  MT=$(menu_titles)
  qa_cmd "menu close" >/dev/null
}
shot_main() { qa_shot "redl-$1" >/dev/null; echo "  redl-$1"; }
row_line() { "$CLI" --socket "$SOCK" list 2>/dev/null | grep -i harbor | head -1; }

pkill -f "SeedwellApp --state-dir $STATE" 2>/dev/null; pkill -f "seedwell daemon --state-dir $QA_ROOT" 2>/dev/null; pkill -f "qa_tracker.py --port 7105" 2>/dev/null; sleep 1
rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT/trk" "$SEEDER"
QA_START_DELAY=0 qa_write_config
sed -i '' 's/^ask_before_adding = .*/ask_before_adding = "multi_file"/' "$STATE/config.toml"
detach python3 -I "$REPO/app/qa/qa_tracker.py" --port 7105 --mode ok --interval 1800 --log "$TLOG" &
sleep 0.5
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special twoseasons --start 1 >/dev/null
T=$(ls "$QA_ROOT"/seed/torrents/*Harbor*.torrent | head -1)
printf 'version = 1\n[transfers]\nupload_limit = 2500000\n' > "$SEEDER/config.toml"
TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/seeder-home" detach "$TRS" daemon --state-dir "$SEEDER" --download-dir "$QA_ROOT/seed/data" --listen-port 7201 --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$SEEDER/control.sock" >"$QA_ROOT/seeder.log" 2>&1 &
for i in $(seq 1 40); do [ -S "$SEEDER/control.sock" ] && break; sleep 0.25; done
"$TRS" --socket "$SEEDER/control.sock" add "$T" --save-path "$QA_ROOT/seed/data" >/dev/null
qa_app_start --window-size 1180x820 --show-window
sleep 2
qa_cmd "appearance dark" >/dev/null

echo "== 1. S01 only: add (held for the choice), choose S01, it finishes and seeds"
"$CLI" --socket "$SOCK" add "$T" --peer 127.0.0.1:7201 >/dev/null 2>&1 || echo "add failed"
sleep 2
ID=$("$CLI" --socket "$SOCK" list 2>/dev/null | grep -io '[0-9a-f]\{40\}' | head -1)
[ -z "$ID" ] && ID=$(python3 -c "import json;print(json.load(open('$QA_ROOT/seed/manifest.json'))[0]['infohash'])")
"$CLI" --socket "$SOCK" choose "$ID" --skip 6-13 >/dev/null 2>&1 || echo "choose failed"
for i in $(seq 1 60); do "$CLI" --socket "$SOCK" list recent 2>/dev/null | grep -q "Harbor" && break; sleep 1; done
"$CLI" --socket "$SOCK" list recent 2>&1 | head -3
check "S01 finished and is Recent" "$("$CLI" --socket "$SOCK" list recent 2>&1)" "Harbor"
echo "  tracker: completed=$(count completed) started=$(count started)"
[ "$(count completed)" = 0 ] || { echo "  FAIL: completed for a partial selection"; FAIL=1; }
sleep 4
qa_cmd "window open" "tab sharing" "files collapse-all" >/dev/null; sleep 1
shot_main "before-window"
shot_menu "before-menu"
check "menu has Recent" "$MT" "Recent"

echo "== 2. tick S02 in the file tree"
qa_cmd "files expand Harbor" >/dev/null; sleep 2
qa_cmd "files folders Harbor" >/dev/null; sleep 1
qa_cmd "files skip Harbor S02" >/dev/null
sleep 6
w=$(where); check "the window follows to Downloads" "$w" "section downloads"
echo "  row: $(row_line)"
check "engine: downloading again" "$(row_line)" "ownload"
sleep 6
shot_main "window"
shot_menu "menu"
check "menu: Downloading lists it" "$MT" "Downloading|"
case "$MT" in *Recent*) echo "  FAIL: menu still has Recent: $MT"; FAIL=1;; *) echo "  ok: menu: not under Recent";; esac
echo "  tracker so far: $(grep -v '^$' "$TLOG" | sed 's/^[0-9]* 7105 [^ ]* //')"
check "left is true while downloading again" "$(tail -1 "$TLOG")" "left="
[ "$(count started)" = 1 ] || { echo "  FAIL: started count $(count started)"; FAIL=1; }
[ "$(count completed)" = 0 ] || { echo "  FAIL: completed before it finished"; FAIL=1; }

echo "== 3. it finishes again: one completed, back in Recent"
for i in $(seq 1 120); do "$CLI" --socket "$SOCK" list recent 2>/dev/null | grep -q "Harbor" && break; sleep 1; done
sleep 4
echo "  tracker: $(sed 's/^[0-9]* 7105 [^ ]* //' "$TLOG" | tr '\n' ' ')"
check "completed once" "$(count completed)" "1"
[ "$(count started)" = 1 ] || { echo "  FAIL: started count $(count started)"; FAIL=1; }
shot_menu "finished-menu"
check "menu: Recent again" "$MT" "Recent"

[ $FAIL = 0 ] && echo "redownload: PASS" || echo "redownload: FAIL"
qa_cmd "quit" >/dev/null 2>&1; sleep 2
for pat in "SeedwellApp --state-dir $STATE" "seedwell daemon --state-dir $QA_ROOT" "qa_tracker.py --port 7105"; do for pid in $(pgrep -f "$pat" 2>/dev/null); do kill "$pid" 2>/dev/null; done; done
exit $FAIL
