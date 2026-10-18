#!/usr/bin/env bash
# Cancel on a held torrent (SPEC 7A.6, 7A.7, 8.6): Cancel removes the torrent AND moves its .torrent file to the
# Trash; Shift-Cancel keeps the file; no Shift variant without a file (magnet link). Own QA root
# (seedwell-qa-cancel). The .torrent files live in the QA home's Downloads; the "Trash" is a folder in the QA root
# (SEEDWELL_QA_TRASH_DIR, QA only): the real Trash never receives anything. Shots: app/build/qa/cancel-*.png.
# Only processes whose command line carries this root's --state-dir are signalled.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-cancel"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
REPORT="$CTL.bench"
BIN="$QA_ROOT/fake-trash"
DL="$HOMEROOT/Downloads"
WATCH="$DL/watch"
fails=0
check() { if [ "$2" = "$3" ]; then echo "  OK   $1"; else echo "  FAIL $1 (got '$2', want '$3')"; fails=$((fails+1)); fi; }
exists() { [ -e "$1" ] && echo yes || echo no; }
state() { rm -f "$REPORT"; qa_cmd "starting state" >/dev/null; tail -1 "$REPORT" 2>/dev/null; }
add() { qa_cmd "open-file $1" >/dev/null; sleep 3; }
popover_line() { for i in 1 2 3 4 5 6 7 8; do l=$("$OUT/winid" Seedwell | awk '$2>0 && $4 ~ /^3[0-9][0-9]x/ {print; exit}'); [ -n "$l" ] && break; sleep 0.3; done; echo "$l"; }
shot_popover() { local l id; l=$(popover_line); id=${l%% *}
  [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/cancel-$1.png" && echo "  cancel-$1 (popover window $id, $(echo "$l" | awk '{print $4}'))" || echo "  MISSING cancel-$1"; }

if [ "${1:-}" != "reuse" ]; then
  pkill -f "SeedwellApp --state-dir $STATE" 2>/dev/null; sleep 1
  rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT" "$DL" "$WATCH" "$BIN"
  QA_START_DELAY=120 qa_write_config
  sed -i '' 's/^ask_before_adding = .*/ask_before_adding = "multi_file"/' "$STATE/config.toml"
  cat >> "$STATE/config.toml" <<TOML
[[folders.watch]]
path = "~/Downloads/watch"
mode = "auto"
after_add = "leave"
TOML
  for n in 1 2 3 4 5; do
    python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib$n" --special season --start $n >/dev/null
    cp "$QA_ROOT"/lib$n/torrents/*.torrent "$DL/season$n.torrent"
  done
  : > "$CTL"; qa_wake
  open -n --env "SEEDWELL_QA_TRASH_DIR=$BIN" "$APP" --args --state-dir "$STATE" --home-root "$HOMEROOT" --qa-control "$CTL" --qa-loopback-trackers --window-size 1180x820 --show-window
  for i in $(seq 1 60); do [ -S "$SOCK" ] && break; sleep 0.5; done
  sleep 2
fi
qa_cmd "appearance dark" >/dev/null
ls -la "$DL" | sed 's/^/  /'

echo "== 1. Finder open, plain Cancel: torrent removed, .torrent to the (fake) Trash"
add "$DL/season1.torrent"; echo "$(state)"
shot_popover "normal"
qa_cmd "starting shift on" >/dev/null; sleep 0.6; echo "$(state)"
shot_popover "shift"
qa_cmd "starting shift off" >/dev/null; sleep 0.6
qa_cmd "starting cancel" >/dev/null; sleep 2; echo "after: $(state)"
check "file left the Downloads folder" "$(exists "$DL/season1.torrent")" no
check "file is in the fake Trash" "$(exists "$BIN/season1.torrent")" yes

echo "== 2. Finder open, Shift-Cancel: torrent removed, file stays"
add "$DL/season2.torrent"
qa_cmd "starting cancel keep" >/dev/null; sleep 2; echo "after: $(state)"
check "file stays" "$(exists "$DL/season2.torrent")" yes
check "not in the Trash" "$(exists "$BIN/season2.torrent")" no
check "torrent gone" "$("$CLI" --socket "$SOCK" list 2>&1 | grep -c 'Night' )" 0

echo "== 3. Expanded checklist: Cancel (Escape's action) trashes; Shift-Cancel title in the checklist"
add "$DL/season3.torrent"
qa_cmd "starting fold" >/dev/null; sleep 1
shot_popover "expanded"
qa_cmd "starting shift on" >/dev/null; sleep 0.6
shot_popover "expanded-shift"
qa_cmd "starting shift off" >/dev/null; sleep 0.4
qa_cmd "starting cancel" >/dev/null; sleep 2; echo "after: $(state)"
check "file trashed from the expanded view" "$(exists "$BIN/season3.torrent")" yes

echo "== 4. Watch folder (auto): the file left in the watch folder is trashed"
cp "$QA_ROOT/lib4/torrents/"*.torrent "$WATCH/wf.torrent"; sleep 6; echo "$(state)"
qa_cmd "starting show" >/dev/null; sleep 1
shot_popover "watch"
qa_cmd "starting cancel" >/dev/null; sleep 2; echo "after: $(state)"
check "watch file trashed" "$(exists "$BIN/wf.torrent")" yes
check "watch folder empty" "$(exists "$WATCH/wf.torrent")" no

echo "== 5. The file was replaced by another torrent's: never trashed"
add "$DL/season5.torrent"
cp "$DL/season2.torrent" "$DL/season5.torrent"
qa_cmd "starting cancel" >/dev/null; sleep 2; echo "after: $(state)"
check "a non-matching file stays" "$(exists "$DL/season5.torrent")" yes
check "and is not in the Trash" "$(exists "$BIN/season5.torrent")" no

echo "== 6. Magnet link: no file, no Shift variant"
"$CLI" --socket "$SOCK" add "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Magnet.Test" >/dev/null 2>&1
sleep 2
qa_cmd "starting show" "starting choose" >/dev/null; sleep 1.5
qa_cmd "starting shift on" >/dev/null; sleep 0.6; echo "$(state)"
shot_popover "magnet"
qa_cmd "starting shift off" >/dev/null
qa_cmd "starting cancel" >/dev/null; sleep 1; echo "after: $(state)"
qa_cmd "starting shift off" "starting close" >/dev/null
echo "fake trash: $(ls "$BIN" | tr '\n' ' ')"
[ "$fails" = 0 ] && echo "ALL OK" || echo "$fails FAILED"
