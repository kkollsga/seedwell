#!/usr/bin/env bash
# Cancel on a held torrent (SPEC 7A.6, 7A.7, 8.6): Cancel removes the torrent AND moves its .torrent file to the
# Trash; Shift-Cancel keeps the file; no Shift variant without a file (magnet link). Own QA root
# (seedwell-qa-cancel). The .torrent files live in the QA home's Downloads; the "Trash" is a folder in the QA root
# (SEEDWELL_QA_TRASH_DIR, QA only): the real Trash never receives anything. Shots: app/build/qa/cancel-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-cancel"
source "$(dirname "$0")/env.sh"
BIN="$QA_ROOT/fake-trash"
DL="$HOMEROOT/Downloads"
WATCH="$DL/watch"
exists() { [ -e "$1" ] && echo yes || echo no; }
same() { if [ "$2" = "$3" ]; then echo "  ok: $1"; else echo "  FAIL: $1 (got '$2', want '$3')"; FAIL=1; fi; }
state() { qa_ask "starting state" | tail -1; }
add() { qa_cmd "open-file $1"; qa_wait "starting state ~ items 1" 10 >/dev/null; }
cancelled() { qa_wait "starting state ~ items 0" 10; }
shot_popover() { qa_shot "cancel-$1" popover; }

qa_fresh
mkdir -p "$DL" "$WATCH" "$BIN"
QA_WATCH="~/Downloads/watch" QA_START_DELAY=120 qa_write_config
sed -i '' 's/^ask_before_adding = .*/ask_before_adding = "multi_file"/' "$STATE/config.toml"
for n in 1 2 3 4 5; do
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib$n" --special season --start $n >/dev/null
  cp "$QA_ROOT"/lib$n/torrents/*.torrent "$DL/season$n.torrent"
done
QA_APP_ENV="SEEDWELL_QA_TRASH_DIR=$BIN" qa_app_start --window-size 1180x820 --show-window || qa_end cancel
qa_cmd "appearance dark"
ls -la "$DL" | sed 's/^/  /'

echo "== 1. Finder open, plain Cancel: torrent removed, .torrent to the (fake) Trash"
add "$DL/season1.torrent"; state
shot_popover "normal"
qa_cmd "starting shift on"; state
shot_popover "shift"
qa_cmd "starting shift off" "starting cancel"; echo "after: $(cancelled)"
qa_until 5 test -e "$BIN/season1.torrent"
same "file left the Downloads folder" "$(exists "$DL/season1.torrent")" no
same "file is in the fake Trash" "$(exists "$BIN/season1.torrent")" yes

echo "== 2. Finder open, Shift-Cancel: torrent removed, file stays"
add "$DL/season2.torrent"
qa_cmd "starting cancel keep"; echo "after: $(cancelled)"
same "file stays" "$(exists "$DL/season2.torrent")" yes
same "not in the Trash" "$(exists "$BIN/season2.torrent")" no
same "torrent gone" "$("$CLI" --socket "$SOCK" list 2>&1 | grep -c 'Night' )" 0

echo "== 3. Expanded checklist: Cancel (Escape's action) trashes; Shift-Cancel title in the checklist"
add "$DL/season3.torrent"
qa_cmd "starting fold"
shot_popover "expanded"
qa_cmd "starting shift on"
shot_popover "expanded-shift"
qa_cmd "starting shift off" "starting cancel"; echo "after: $(cancelled)"
qa_until 5 test -e "$BIN/season3.torrent"
same "file trashed from the expanded view" "$(exists "$BIN/season3.torrent")" yes

echo "== 4. Watch folder (auto): the file left in the watch folder is trashed"
cp "$QA_ROOT/lib4/torrents/"*.torrent "$WATCH/wf.torrent"
qa_wait "starting state ~ items 1" 15
qa_cmd "starting show"
shot_popover "watch"
qa_cmd "starting cancel"; echo "after: $(cancelled)"
qa_until 5 test -e "$BIN/wf.torrent"
same "watch file trashed" "$(exists "$BIN/wf.torrent")" yes
same "watch folder empty" "$(exists "$WATCH/wf.torrent")" no

echo "== 5. The file was replaced by another torrent's: never trashed"
add "$DL/season5.torrent"
cp "$DL/season2.torrent" "$DL/season5.torrent"
qa_cmd "starting cancel"; echo "after: $(cancelled)"
same "a non-matching file stays" "$(exists "$DL/season5.torrent")" yes
same "and is not in the Trash" "$(exists "$BIN/season5.torrent")" no

echo "== 6. Magnet link: no file, no Shift variant"
"$CLI" --socket "$SOCK" add "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Magnet.Test" >/dev/null 2>&1
qa_wait "starting state ~ items 1" 10 >/dev/null
qa_cmd "starting show" "starting choose" "starting shift on"; state
shot_popover "magnet"
qa_cmd "starting shift off" "starting cancel"; echo "after: $(cancelled)"
qa_cmd "starting shift off" "starting close"
echo "fake trash: $(ls "$BIN" | tr '\n' ' ')"
qa_end cancel
