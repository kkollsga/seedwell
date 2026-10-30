#!/usr/bin/env bash
# A new download takes the window and the menu (SPEC 8.2, 8.3): after a multi-file torrent finished and
# the window sits in Sharing (or a tracker tab), every way of adding a torrent that downloads must
#   - switch the main window to Downloads, select the new row (Get Info stays closed),
#   - show it at the top of the menu bar menu's Downloading section, at once, reopened and live.
# Paths: control-socket add (= open a .torrent / magnet link), watch folder, and the add popover
# (Start Now, Download/OK for a multi-file torrent, countdown expiry). Also: window closed while the
# torrent is added -> it opens on Downloads with the row selected; and Settings is not taken over
# (only a badge on Downloads).
# Shots: app/build/qa/newdl-*.png (before, after, menu). A local rate-limited seeder (headless seedwell
# daemon on 127.0.0.1) serves everything. Own QA root (seedwell-qa-newdl); only processes carrying that root
# are signalled.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-newdl"
source "$(dirname "$0")/env.sh"
WATCH="$HOMEROOT/Watch"
add() { "$CLI" --socket "$SOCK" add "$@" >/dev/null 2>&1 || echo "add failed: $*"; }
big() { ls "$QA_ROOT"/big/torrents/*.torrent | sed -n "$1p"; }
where() { qa_ask "where" | tail -1; }
menu_titles() { qa_ask "menu-titles" | tail -1; }
shot_menu() { # name: the menu, opened as a click opens it; its titles in $MT
  qa_cmd "menu open"
  qa_shot "newdl-$1" menu
  MT=$(menu_titles)
  qa_cmd "menu close"
}
shot_main() { qa_shot "newdl-$1"; }
# Back to the start: the finished multi-file torrent selected in Sharing, no new download yet.
park() { qa_cmd "window open" "tab sharing" "select Night Harbor"; }
wait_for() { qa_wait "where ~ $2" "$1"; }  # secs, pattern in `where`

qa_fresh
mkdir -p "$WATCH"
QA_WATCH="~/Watch" QA_START_DELAY=0 qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special season --start 1 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/arch" --special season --start 2 --seed 9 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/big" --count 8 --start 1000 --scale 80 >/dev/null
qa_seeder 7201 "$QA_ROOT/seed/data" 2500000 || qa_end new_download
qa_seed "$QA_ROOT/seed/data" "$QA_ROOT"/seed/torrents/*.torrent
qa_seed "$QA_ROOT/arch/data" "$QA_ROOT"/arch/torrents/*.torrent
qa_seed "$QA_ROOT/big/data" "$QA_ROOT"/big/torrents/*.torrent
qa_app_start --window-size 1180x820 --show-window || qa_end new_download
qa_cmd "appearance dark"
echo "== a multi-file torrent downloads from the local seeder and finishes"
add "$QA_ROOT"/seed/torrents/*Night*.torrent --peer 127.0.0.1:7201
qa_until 60 qa_listed recent Night || { echo "  FAIL: Night Harbor did not finish"; FAIL=1; }
"$CLI" --socket "$SOCK" list 2>&1 | head -4

echo "== 1. window in Sharing, finished multi-file selected -> add a magnet-less torrent file (open .torrent / socket add)"
park
qa_wait "where ~ selection 1" 5 >/dev/null
shot_main "before"
add "$(big 1)" --peer 127.0.0.1:7201
w=$(wait_for 10 "section downloads"); check "window follows the new download" "$w" "section downloads"
check "new row selected" "$w" "selection 1"
check "Get Info closed" "$w" "info none"
shot_main "after"
shot_menu "menu-new"
check "menu: Downloading section first" "$MT" "menu: Downloading|"

echo "== 2. window on a tracker tab -> watch folder"
qa_cmd "tab tracker Public"
cp "$(big 2)" "$WATCH/"
w=$(wait_for 12 "section downloads"); check "watch folder add switches" "$w" "section downloads"

echo "== 3. menu open (live) while a torrent is added"
park
qa_cmd "menu open"
add "$(big 3)" --peer 127.0.0.1:7201
t=$(qa_wait "menu-titles ~ menu: Downloading|" 10); echo "  live menu: $t"
check "live menu shows Downloading first" "$t" "menu: Downloading|"
qa_shot "newdl-menu-live" menu
qa_cmd "menu close"

echo "== 4. window closed -> opens on Downloads with the row selected"
park; qa_cmd "window close"
n=$(qa_count downloading)
add "$(big 4)" --peer 127.0.0.1:7201
qa_until 10 eval '[ "$(qa_count downloading)" -gt "$n" ]'
qa_cmd "window open"
w=$(wait_for 5 "selection 1"); check "opens on Downloads" "$w" "section downloads"; check "row selected" "$w" "selection 1"

echo "== 5. Settings is not taken over: only a badge on Downloads"
qa_cmd "settings general"
add "$(big 5)" --peer 127.0.0.1:7201
w=$(wait_for 10 "badge 1"); check "stays in settings" "$w" "section settings"; check "badge on Downloads" "$w" "badge 1"
shot_main "badge"
qa_cmd "tab downloads"; w=$(where); check "badge clears on arrival" "$w" "badge 0"

echo "== 6. add popover: countdown expiry, Start Now and OK"
qa_set_config start_delay_secs 5
park
add "$(big 6)" --peer 127.0.0.1:7201
qa_wait "starting state ~ items 1" 5 >/dev/null
w=$(where); check "held torrent does not switch" "$w" "section sharing"
w=$(wait_for 15 "section downloads"); check "countdown expiry switches" "$w" "section downloads"
qa_set_config start_delay_secs 120
park
add "$(big 7)" --peer 127.0.0.1:7201
qa_wait "starting state ~ items 1" 5 >/dev/null
qa_cmd "starting now"
w=$(wait_for 8 "section downloads"); check "Start Now switches" "$w" "section downloads"
qa_set_config start_delay_secs 0; qa_setting transfers.ask_before_adding=multi_file
park
add "$QA_ROOT"/arch/torrents/*.torrent --peer 127.0.0.1:7201
qa_wait "starting state ~ items 1" 10 >/dev/null
w=$(where); check "a torrent waiting for its file choice does not switch" "$w" "section sharing"
qa_cmd "starting all Night" "starting ok Night"
w=$(wait_for 10 "section downloads"); check "OK in the popover switches" "$w" "section downloads"
qa_setting transfers.ask_before_adding=never

echo "== 7. a restart with downloads running does not count as new"
park
qa_app_quit
qa_app_start --window-size 1180x820 --show-window || qa_end new_download
qa_cmd "tab sharing"
sleep 6   # the engine resumes its downloads meanwhile: none of them may take the window
w=$(where); check "window stays in Sharing after a launch" "$w" "section sharing"

"$CLI" --socket "$SOCK" list 2>&1 | head -8
qa_end new_download
