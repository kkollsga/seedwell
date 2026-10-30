#!/usr/bin/env bash
# The add popover: Cancel + Start (no Select Files…, Pause or Start Now), selection mode in the main window (SPEC 7A.6,
# 7A.7, 8.6): the chevron opens the checklist, the title opens the torrent in the main window (selected, scrolled to,
# files open), Cancel is a normal button and ⇧ slides a "Keep .torrent file" caption out under it. Own QA root
# (seedwell-qa-addpop2); the fake Trash is a folder in it (SEEDWELL_QA_TRASH_DIR): the real Trash never receives
# anything. Shots: app/build/qa/addpop2-*.png, selmode-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-addpop2"
source "$(dirname "$0")/env.sh"
BIN="$QA_ROOT/fake-trash"
DL="$HOMEROOT/Downloads"
state() { qa_ask "starting state" | tail -1; }
held() { qa_wait "starting state ~ items $1" 10; }
shot_popover() { qa_shot "addpop2-$1" popover; }
shot_main() { qa_shot "addpop2-$1"; }
shot_sel() { qa_shot "selmode-$1"; }
shot_pop_sel() { qa_shot "selmode-$1" popover; }
ticked() { qa_ask "ui state" | sed 's/.*choice/choice/' | tail -1 | sed -n 's/.*checked \([0-9]*\) .*/\1/p'; }

qa_fresh
mkdir -p "$DL" "$BIN"
QA_WATCH="~/Downloads/watch" QA_START_DELAY=120 qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special season --start 1 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special bunny --start 3 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib2" --special season --start 2 >/dev/null
cp "$QA_ROOT"/lib2/torrents/*Night.Harbor*.torrent "$DL/season2.torrent"
cp "$QA_ROOT"/lib/torrents/*Night.Harbor*.torrent "$DL/season.torrent"
cp "$QA_ROOT"/lib/torrents/*Bunny*.torrent "$DL/bunny.torrent"
QA_APP_ENV="SEEDWELL_QA_TRASH_DIR=$BIN" qa_app_start --window-size 1180x820 --show-window || qa_end addpop2
qa_cmd "appearance dark"

echo "== countdown block: chevron, no Select Files…"
qa_cmd "open-file $DL/bunny.torrent"; held 1
qa_cmd "starting show"; state
shot_popover "countdown"
echo "== the chevron ends the countdown and opens the list; ⇧ caption on the compact block"
qa_cmd "starting choose"; qa_wait "starting state ~ picked" 10
shot_popover "expanded"
qa_cmd "starting shift on"
shot_popover "expanded-shift"
qa_cmd "starting fold"; state
shot_popover "shift"
qa_cmd "starting shift off"
shot_popover "compact-after-shift"
qa_cmd "starting cancel keep"; held 0 >/dev/null
check "bunny.torrent kept" "$([ -e "$DL/bunny.torrent" ] && echo yes || echo NO)" "yes"

echo "== awaiting a choice: compact (the main window lists no files for a torrent that still waits for a choice: it has Choose…)"
qa_setting transfers.ask_before_adding=multi_file
qa_cmd "open-file $DL/season.torrent"; held 1
shot_popover "compact"
qa_cmd "starting title"
shot_main "main-after-title-awaiting"
qa_cmd "starting cancel"; held 0 >/dev/null
qa_until 5 test -e "$BIN/season.torrent"
check "season.torrent trashed" "$([ -e "$BIN/season.torrent" ] && echo yes || echo NO)" "yes"
cp "$QA_ROOT"/lib/torrents/*Night.Harbor*.torrent "$DL/season.torrent"

echo "== selection mode: title click on a countdown torrent ends the countdown and opens the window in selection mode"
qa_setting transfers.ask_before_adding=never
qa_cmd "open-file $DL/season.torrent"; held 1
qa_cmd "starting show"
shot_pop_sel "popover-countdown"
qa_cmd "starting title"; qa_wait "starting state ~ picked" 10
shot_sel "window-after-countdown-title"
shot_pop_sel "popover-after-countdown-title"
qa_cmd "starting cancel keep"; held 0 >/dev/null

echo "== selection mode: title click on a torrent that waits for a choice"
qa_setting transfers.ask_before_adding=multi_file
qa_cmd "open-file $DL/season2.torrent"; held 1
shot_pop_sel "popover-awaiting"
qa_cmd "starting title"
shot_sel "window-awaiting"
echo "== a tick made in the window shows in the popover (same model)"
before=$(ticked); echo "   ticked before: $before"
# The first episode's box in the window's list: what its click calls on the shared model.
qa_cmd "choice tick E01"
after=$(ticked); echo "   ticked after the first episode's box in the window: $after"
[ -n "$after" ] && [ "$after" != "$before" ] && echo "  ok: the tick reached the shared model" || { echo "  FAIL: ticked stayed '$after'"; FAIL=1; }
shot_sel "window-tick"
qa_cmd "starting fold"
shot_pop_sel "popover-tick"
qa_cmd "starting shift on"
shot_sel "window-shift"
qa_cmd "starting shift off" "starting cancel"; held 0 >/dev/null
qa_until 5 test -e "$BIN/season2.torrent"
check "season2.torrent trashed" "$([ -e "$BIN/season2.torrent" ] && echo yes || echo NO)" "yes"

echo "== Start with the list never opened takes every file"
cp "$QA_ROOT"/lib/torrents/*Night.Harbor*.torrent "$DL/season3.torrent"
qa_cmd "open-file $DL/season3.torrent"; held 1
echo "   ticked: $(ticked) (of 13)"
qa_cmd "starting ok"; held 0 >/dev/null
qa_until 10 qa_listed all Harbor
"$CLI" --socket "$SOCK" list 2>&1 | head -4

echo "== magnet link while fetching: no caption"
"$CLI" --socket "$SOCK" add "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Magnet.Test" >/dev/null 2>&1
held 1 >/dev/null
qa_cmd "starting show" "starting choose" "starting shift on"; state
shot_popover "magnet"
qa_cmd "starting shift off" "starting cancel" "starting close"
qa_end addpop2
