#!/usr/bin/env bash
# The menu bar menu's inline file list (SPEC 8.2): every multi-file torrent row (Downloading, Queued, Recent) has a
# disclosure chevron at its left edge; it opens a scrolling checklist right under the row, the menu stays open, only
# one list is open at a time, and ticking a file reaches the engine. Scene (own QA root seedwell-qa-menufiles, a
# rate-limited local seeder = headless seedwell daemon on 127.0.0.1): Night Harbor S01 (13 files) and a 2,303-file photo
# archive downloading, Big Buck Bunny (3 files) finished into Recent.
# The REAL status menu is opened (`menu open`); QAControl clicks the chevron and the list's rows exactly as their mouse
# handlers would (`menu-files expand|press|fold|state|submenu`; a real pointer cannot be driven inside a tracking menu).
# Asserts: no "Files" submenu item, expand adds rows and the menu keeps tracking, ticking changes the engine's wanted
# count (file, folder open + file inside), expanding a second torrent closes the first, Recent works.
# Captures (the menu window by id, QA_PID limits winid to this QA app): app/build/qa/menufiles-inline-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-menufiles"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
FAIL=0
[ -x "$OUT/winid" ] || swiftc -O -o "$OUT/winid" "$REPO/app/qa/winid.swift" 2>/dev/null
TRS="${TRS_DAEMON:-$REPO/target/release/seedwell}"
SEEDER="$QA_ROOT/seeder"
REPORT="$CTL.bench"
check() { case "$2" in *"$3"*) echo "  ok: $1 ($2)";; *) echo "  FAIL: $1: wanted '$3' in: $2"; FAIL=1;; esac; }
say() { rm -f "$REPORT"; qa_cmd "$1" >/dev/null; sleep "${2:-0.3}"; tail -1 "$REPORT" 2>/dev/null; }
state() { say "menu-files state $1" "${2:-0.3}"; }
menu_win() { for i in 1 2 3 4 5 6 7 8; do id=$("$OUT/winid" Seedwell | awk '$2>0 {split($4,a,"x"); print a[2], $1}' | sort -rn | head -1 | cut -d' ' -f2); [ -n "$id" ] && break; sleep 0.4; done; echo "$id"; }
shot() { local id; id=$(menu_win); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/menufiles-inline-$1.png" && echo "  menufiles-inline-$1 (menu window $id)" || { echo "  FAIL: no menu window for $1"; FAIL=1; }; }
rows() { echo "$1" | sed -n 's/.* rows \([0-9]*\) .*/\1/p'; }

if [ "${1:-}" != "reuse" ]; then
  pkill -f "SeedwellApp --state-dir $STATE" 2>/dev/null; pkill -f "seedwell daemon --state-dir $QA_ROOT" 2>/dev/null; sleep 1
  rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT" "$SEEDER"
  QA_START_DELAY=0 qa_write_config
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special season --start 1 --scale 6 >/dev/null
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special archive --start 2 --scale 4 >/dev/null
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special bunny --start 3 >/dev/null
  printf 'version = 1\n[transfers]\nupload_limit = 150000\n' > "$SEEDER/config.toml"
  TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/seeder-home" detach "$TRS" daemon --state-dir "$SEEDER" --download-dir "$QA_ROOT/seed/data" --listen-port 7201 --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$SEEDER/control.sock" >"$QA_ROOT/seeder.log" 2>&1 &
  for i in $(seq 1 40); do [ -S "$SEEDER/control.sock" ] && break; sleep 0.25; done
  for t in "$QA_ROOT"/seed/torrents/*.torrent; do "$TRS" --socket "$SEEDER/control.sock" add "$t" --save-path "$QA_ROOT/seed/data" >/dev/null; done
  qa_app_start --window-size 1180x820 --show-window
  sleep 2
  qa_cmd "appearance dark" >/dev/null
  echo "== scene: Big Buck Bunny finishes into Recent, then the season pack and the archive download"
  "$CLI" --socket "$SOCK" add "$QA_ROOT"/seed/torrents/*Bunny*.torrent --peer 127.0.0.1:7201 >/dev/null
  for i in $(seq 1 90); do "$CLI" --socket "$SOCK" list recent 2>/dev/null | grep -q "Bunny" && break; sleep 1; done
  "$CLI" --socket "$SOCK" add "$QA_ROOT"/seed/torrents/*Night*.torrent --peer 127.0.0.1:7201 >/dev/null
  "$CLI" --socket "$SOCK" add "$QA_ROOT"/seed/torrents/*Archive*.torrent --peer 127.0.0.1:7201 >/dev/null
  sleep 6
fi
QA_PID=$(pgrep -f "SeedwellApp --state-dir $STATE" | head -1); export QA_PID
[ -n "$QA_PID" ] || { echo "QA app not running"; exit 1; }
"$CLI" --socket "$SOCK" list 2>&1 | head -4

echo "== 1. collapsed by default, chevron on the rows, no Files submenu item"
qa_cmd "menu open" >/dev/null; sleep 1.3
s=$(state Night); check "nothing open" "$s" "expanded none"
sub=$(say 'menu-files submenu Night'); case "$sub" in *"submenu Files|"*) echo "  FAIL: a Files submenu item is still there: $sub"; FAIL=1;; *) echo "  ok: no Files item in the row submenu";; esac
R0=$(rows "$s")
shot "collapsed"

echo "== 2. the chevron opens the list in place; the menu stays open"
check "expand Night Harbor" "$(say 'menu-files expand Night' 0.8)" "expand true"
s=$(state Night 0.5)
N0=$(echo "$s" | sed -n 's/.*engine \([0-9]*\)\/13.*/\1/p')
check "this torrent's list is open and has rows" "$s" "this true"
[ "$(rows "$s")" -gt 0 ] && echo "  ok: $(rows "$s") rows (was ${R0:-0})" || { echo "  FAIL: no rows"; FAIL=1; }
check "menu still tracking" "$s" "open true"
check "footer shows the engine's wanted files" "$s" "footer $N0 of 13 files"
say "menu-files frames" >/dev/null; echo "  $(tail -1 $REPORT)"
shot "expanded"

echo "== 3. a toggle reaches the engine, the menu stays open"
check "press an episode" "$(say 'menu-files press S01E03.1080p')" "press true"
sleep 1.3
s=$(state Night); check "one file less in the engine" "$s" "engine $((N0 - 1))/13"; check "menu still tracking" "$s" "open true"
check "footer follows" "$s" "footer $((N0 - 1)) of 13 files"
[ -n "$(menu_win)" ] && echo "  ok: the menu window is on screen" || { echo "  FAIL: the menu window is gone"; FAIL=1; }
shot "after-toggle"
before=$(rows "$s")
say "menu-files fold Subs" >/dev/null; s=$(state Night); [ "$(rows "$s")" -gt "$before" ] && echo "  ok: the Subs folder opened inside the list ($before -> $(rows "$s") rows)" || { echo "  FAIL: fold did not add rows: $s"; FAIL=1; }
say "menu-files press 01.en" >/dev/null; sleep 1.3
check "a file inside the folder" "$(state Night)" "engine $((N0 - 2))/13"
shot "folder-open"
check "menu still tracking" "$(state Night)" "open true"

echo "== 4. one list at a time"
check "expand the archive" "$(say 'menu-files expand Archive' 0.8)" "expand true"
s=$(state Archive 0.5); check "the archive's list is open" "$s" "this true"
check "the season's list closed" "$(state Night)" "this false"
say "menu-files fold scans" >/dev/null; s=$(state Archive); echo "  archive rows after opening scans: $(rows "$s")"
[ "$(rows "$s")" -gt 20 ] && echo "  ok: a long list (scrolls)" || { echo "  FAIL: scans did not open: $s"; FAIL=1; }
shot "archive"
say "menu-files expand Archive" >/dev/null; check "expanding it again closes it" "$(state Archive 0.5)" "expanded none"

echo "== 5. Recent"
check "expand the finished torrent" "$(say 'menu-files expand Bunny' 0.8)" "expand true"
s=$(state Bunny 0.5); check "its list is open" "$s" "this true"; check "footer" "$s" "footer 3 of 3 files"
shot "recent"

echo "== 6. closing the menu frees the list"
qa_cmd "menu close" >/dev/null; sleep 1.5
qa_cmd "menu open" >/dev/null; sleep 1.2
check "collapsed again on reopen" "$(state Bunny)" "expanded none"
qa_cmd "menu close" >/dev/null

"$CLI" --socket "$SOCK" list 2>&1 | head -4
[ $FAIL = 0 ] && echo "menu files: PASS" || echo "menu files: FAIL"
qa_cmd "quit" >/dev/null 2>&1; sleep 2
for pat in "SeedwellApp --state-dir $STATE" "seedwell daemon --state-dir $QA_ROOT"; do for pid in $(pgrep -f "$pat" 2>/dev/null); do kill "$pid" 2>/dev/null; done; done
exit $FAIL
