#!/usr/bin/env bash
# The menu bar menu's inline file list (SPEC 8.2): every multi-file torrent row (Downloading, Queued, Recent) has a
# disclosure chevron at its left edge; it opens a scrolling checklist right under the row, the menu stays open, only
# one list is open at a time, and ticking a file reaches the engine. Scene (own QA root seedwell-qa-menufiles, a
# rate-limited local seeder = headless seedwell daemon on 127.0.0.1): Night Harbor S01 (13 files) and a 2,303-file photo
# archive downloading, Big Buck Bunny (3 files) finished into Recent.
# The status menu is opened (`menu open`: built and refreshed as on a click, not shown in a quiet QA run); QAControl
# clicks the chevron and the list's rows exactly as their mouse handlers would (`menu-files expand|press|fold|state|context`).
# Asserts: no "Files" item in the right-click menu, expand adds rows and the menu keeps tracking, ticking changes the engine's wanted
# count (file, folder open + file inside), expanding a second torrent closes the first, Recent works.
# Shots (the open menu's real rows on a stand-in panel): app/build/qa/menufiles-inline-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-menufiles"
source "$(dirname "$0")/env.sh"
say() { qa_ask "$1" | tail -1; }
state() { say "menu-files state $1"; }
shot() { qa_shot "menufiles-inline-$1" menu; }
rows() { echo "$1" | sed -n 's/.* rows \([0-9]*\) .*/\1/p'; }

qa_fresh
QA_START_DELAY=0 qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special season --start 1 --scale 6 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special archive --start 2 --scale 4 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special bunny --start 3 >/dev/null
qa_seeder 7201 "$QA_ROOT/seed/data" 150000 || qa_end menu_files
qa_seed "$QA_ROOT/seed/data" "$QA_ROOT"/seed/torrents/*.torrent
qa_app_start --window-size 1180x820 --show-window || qa_end menu_files
qa_cmd "appearance dark"
echo "== scene: Big Buck Bunny finishes into Recent, then the season pack and the archive download"
"$CLI" --socket "$SOCK" add "$QA_ROOT"/seed/torrents/*Bunny*.torrent --peer 127.0.0.1:7201 >/dev/null
qa_until 90 qa_listed recent Bunny || { echo "  FAIL: Big Buck Bunny did not finish"; FAIL=1; }
"$CLI" --socket "$SOCK" add "$QA_ROOT"/seed/torrents/*Night*.torrent --peer 127.0.0.1:7201 >/dev/null
"$CLI" --socket "$SOCK" add "$QA_ROOT"/seed/torrents/*Archive*.torrent --peer 127.0.0.1:7201 >/dev/null
qa_until 20 eval 'qa_listed downloading Night && qa_listed downloading Archive' || echo "  (the pack and the archive are not downloading yet)"
"$CLI" --socket "$SOCK" list 2>&1 | head -4

echo "== 1. collapsed by default, chevron on the rows, no Files item in the right-click menu"
qa_cmd "menu open"
s=$(state Night); check "nothing open" "$s" "expanded none"
lacks "no Files item in the row's right-click menu" "$(say 'menu-files context Night')" "context Files|"
R0=$(rows "$s")
shot "collapsed"

echo "== 2. the chevron opens the list in place; the menu stays open"
check "expand Night Harbor" "$(say 'menu-files expand Night')" "expand true"
s=$(qa_wait "menu-files state Night ~ this true" 5)
N0=$(echo "$s" | sed -n 's/.*engine \([0-9]*\)\/13.*/\1/p')
check "this torrent's list is open and has rows" "$s" "this true"
[ "$(rows "$s")" -gt 0 ] && echo "  ok: $(rows "$s") rows (was ${R0:-0})" || { echo "  FAIL: no rows"; FAIL=1; }
check "menu still open" "$s" "open true"
check "footer shows the engine's wanted files" "$s" "footer $N0 of 13 files"
echo "  $(say "menu-files frames")"
shot "expanded"

echo "== 3. a toggle reaches the engine, the menu stays open"
check "press an episode" "$(say 'menu-files press S01E03.1080p')" "press true"
s=$(qa_wait "menu-files state Night ~ engine $((N0 - 1))/13" 10)
check "one file less in the engine" "$s" "engine $((N0 - 1))/13"; check "menu still open" "$s" "open true"
check "footer follows" "$(qa_wait "menu-files state Night ~ footer $((N0 - 1)) of 13 files" 5)" "footer $((N0 - 1)) of 13 files"
shot "after-toggle"
before=$(rows "$s")
say "menu-files fold Subs" >/dev/null; s=$(state Night); [ "$(rows "$s")" -gt "$before" ] && echo "  ok: the Subs folder opened inside the list ($before -> $(rows "$s") rows)" || { echo "  FAIL: fold did not add rows: $s"; FAIL=1; }
say "menu-files press 01.en" >/dev/null
check "a file inside the folder" "$(qa_wait "menu-files state Night ~ engine $((N0 - 2))/13" 10)" "engine $((N0 - 2))/13"
shot "folder-open"
check "menu still open" "$(state Night)" "open true"

echo "== 4. one list at a time"
check "expand the archive" "$(say 'menu-files expand Archive')" "expand true"
s=$(qa_wait "menu-files state Archive ~ this true" 5); check "the archive's list is open" "$s" "this true"
check "the season's list closed" "$(state Night)" "this false"
say "menu-files fold scans" >/dev/null; s=$(state Archive); echo "  archive rows after opening scans: $(rows "$s")"
[ "$(rows "$s")" -gt 20 ] && echo "  ok: a long list (scrolls)" || { echo "  FAIL: scans did not open: $s"; FAIL=1; }
shot "archive"
say "menu-files expand Archive" >/dev/null; check "expanding it again closes it" "$(qa_wait "menu-files state Archive ~ expanded none" 5)" "expanded none"

echo "== 5. Recent"
check "expand the finished torrent" "$(say 'menu-files expand Bunny')" "expand true"
s=$(qa_wait "menu-files state Bunny ~ this true" 5); check "its list is open" "$s" "this true"; check "footer" "$s" "footer 3 of 3 files"
shot "recent"

echo "== 6. closing the menu frees the list"
qa_cmd "menu close" "menu open"
check "collapsed again on reopen" "$(state Bunny)" "expanded none"
qa_cmd "menu close"

"$CLI" --socket "$SOCK" list 2>&1 | head -4
qa_end menu_files
