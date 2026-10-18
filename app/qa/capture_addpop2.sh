#!/usr/bin/env bash
# The add popover: Cancel + Start (no Select Files…, Pause or Start Now), selection mode in the main window (SPEC 7A.6, 7A.7, 8.6): the chevron opens the checklist, the title
# opens the torrent in the main window (selected, scrolled to, files open), Cancel is a normal button and ⇧ slides
# a "Keep .torrent file" caption out under it. Own QA root (seedwell-qa-addpop2); the fake Trash is a folder in it
# (SEEDWELL_QA_TRASH_DIR): the real Trash never receives anything. Shots: app/build/qa/addpop2-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-addpop2"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
REPORT="$CTL.bench"
BIN="$QA_ROOT/fake-trash"
DL="$HOMEROOT/Downloads"
state() { rm -f "$REPORT"; qa_cmd "starting state" >/dev/null; tail -1 "$REPORT" 2>/dev/null; }
set_ask() { sed -i '' "s/^ask_before_adding = .*/ask_before_adding = \"$1\"/" "$STATE/config.toml"; sleep 5; }
popover_line() { for i in 1 2 3 4 5 6 7 8; do l=$("$OUT/winid" Seedwell | awk '$2>0 && $4 ~ /^3[0-9][0-9]x/ {print; exit}'); [ -n "$l" ] && break; sleep 0.3; done; echo "$l"; }
shot_popover() { local l id; l=$(popover_line); id=${l%% *}
  [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/addpop2-$1.png" && echo "  addpop2-$1 (popover $id, $(echo "$l" | awk '{print $4}'))" || echo "  MISSING addpop2-$1"; }
shot_main() { qa_cmd "activate main" >/dev/null; local id; id=$("$OUT/winid" Seedwell | awk '$2==0 {print $1" "$4}' | sort -t' ' -k2 | tail -1 | cut -d' ' -f1)
  [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/addpop2-$1.png" && echo "  addpop2-$1"; }

if [ "${1:-}" != "reuse" ]; then
  pkill -f "SeedwellApp --state-dir $STATE" 2>/dev/null; sleep 1
  rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT" "$DL" "$BIN"
  QA_START_DELAY=120 qa_write_config
  cat >> "$STATE/config.toml" <<TOML
[[folders.watch]]
path = "~/Downloads/watch"
mode = "auto"
after_add = "leave"
TOML
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special season --start 1 >/dev/null
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special bunny --start 3 >/dev/null
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib2" --special season --start 2 >/dev/null
  cp "$QA_ROOT"/lib2/torrents/*Night.Harbor*.torrent "$DL/season2.torrent"
  cp "$QA_ROOT"/lib/torrents/*Night.Harbor*.torrent "$DL/season.torrent"
  cp "$QA_ROOT"/lib/torrents/*Bunny*.torrent "$DL/bunny.torrent"
  : > "$CTL"; qa_wake
  open -n --env "SEEDWELL_QA_TRASH_DIR=$BIN" "$APP" --args --state-dir "$STATE" --home-root "$HOMEROOT" --qa-control "$CTL" --qa-loopback-trackers --window-size 1180x820 --show-window
  for i in $(seq 1 60); do [ -S "$SOCK" ] && break; sleep 0.5; done
  sleep 2
fi
qa_cmd "appearance dark" >/dev/null

echo "== countdown block: chevron, no Select Files…"
qa_cmd "open-file $DL/bunny.torrent" >/dev/null; sleep 3
qa_cmd "starting show" >/dev/null; sleep 1; echo "$(state)"
shot_popover "countdown"
echo "== the chevron ends the countdown and opens the list; ⇧ caption on the compact block"
qa_cmd "starting choose" >/dev/null; sleep 1.5; echo "$(state)"
shot_popover "expanded"
qa_cmd "starting shift on" >/dev/null; sleep 0.8
shot_popover "expanded-shift"
qa_cmd "starting fold" >/dev/null; sleep 1; echo "$(state)"
shot_popover "shift"
qa_cmd "starting shift off" >/dev/null; sleep 0.6
shot_popover "compact-after-shift"
qa_cmd "starting cancel keep" >/dev/null; sleep 2
echo "bunny.torrent kept: $([ -e "$DL/bunny.torrent" ] && echo yes || echo NO)"

echo "== awaiting a choice: compact (the main window lists no files for a torrent that still waits for a choice: it has Choose…)"
set_ask multi_file
qa_cmd "open-file $DL/season.torrent" >/dev/null; sleep 3; echo "$(state)"
shot_popover "compact"
qa_cmd "starting title" >/dev/null; sleep 1.5
shot_main "main-after-title-awaiting"
qa_cmd "starting cancel" >/dev/null; sleep 2
echo "season.torrent trashed: $([ -e "$BIN/season.torrent" ] && echo yes || echo NO)"; cp "$QA_ROOT"/lib/torrents/*Night.Harbor*.torrent "$DL/season.torrent"

shot_sel() { qa_cmd "activate main" >/dev/null; local id; id=$("$OUT/winid" Seedwell | awk '$2==0 {print $1" "$4}' | sort -t' ' -k2 | tail -1 | cut -d' ' -f1)
  [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/selmode-$1.png" && echo "  selmode-$1"; }
shot_pop_sel() { local l id; l=$(popover_line); id=${l%% *}; [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/selmode-$1.png" && echo "  selmode-$1"; }
ticked() { rm -f "$REPORT"; qa_cmd "ui state" >/dev/null; sleep 0.2; sed 's/.*choice/choice/' "$REPORT" | tail -1 | sed -n 's/.*checked \([0-9]*\) .*/\1/p'; }

echo "== selection mode: title click on a countdown torrent ends the countdown and opens the window in selection mode"
set_ask never
qa_cmd "open-file $DL/season.torrent" >/dev/null; sleep 3
qa_cmd "starting show" >/dev/null; sleep 1
shot_pop_sel "popover-countdown"
qa_cmd "starting title" >/dev/null; sleep 3; echo "$(state)"
shot_sel "window-after-countdown-title"
shot_pop_sel "popover-after-countdown-title"
qa_cmd "starting cancel keep" >/dev/null; sleep 2

echo "== selection mode: title click on a torrent that waits for a choice"
set_ask multi_file
qa_cmd "open-file $DL/season2.torrent" >/dev/null; sleep 3
shot_pop_sel "popover-awaiting"
qa_cmd "starting title" >/dev/null; sleep 2.5
shot_sel "window-awaiting"
echo "== a tick made in the window shows in the popover (same model)"
echo "   ticked before: $(ticked)"
qa_cmd "click at=336,176 hover win=main" >/dev/null; sleep 0.8
echo "   ticked after a click on the first episode's box in the window: $(ticked)"
shot_sel "window-tick"
qa_cmd "starting fold" >/dev/null; sleep 1.2
shot_pop_sel "popover-tick"
qa_cmd "starting shift on" >/dev/null; sleep 0.8
shot_sel "window-shift"
qa_cmd "starting shift off" >/dev/null; sleep 0.5
qa_cmd "starting cancel" >/dev/null; sleep 2
echo "season2.torrent trashed: $([ -e "$BIN/season2.torrent" ] && echo yes || echo NO)"

echo "== Start with the list never opened takes every file"
cp "$QA_ROOT"/lib/torrents/*Night.Harbor*.torrent "$DL/season3.torrent"
qa_cmd "open-file $DL/season3.torrent" >/dev/null; sleep 3
echo "   ticked: $(ticked) (of 13)"
qa_cmd "starting ok" >/dev/null; sleep 3
"$CLI" --socket "$SOCK" list 2>&1 | head -4

echo "== magnet link while fetching: no caption"
"$CLI" --socket "$SOCK" add "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Magnet.Test" >/dev/null 2>&1
sleep 2
qa_cmd "starting show" "starting choose" >/dev/null; sleep 1.5
qa_cmd "starting shift on" >/dev/null; sleep 0.8; echo "$(state)"
shot_popover "magnet"
qa_cmd "starting shift off" "starting cancel" "starting close" >/dev/null; sleep 1
