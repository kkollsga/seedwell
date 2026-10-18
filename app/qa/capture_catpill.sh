#!/usr/bin/env bash
# The category pill of the add popover (SPEC 8.6, 7A.2): the pill in the corner, its menu, and a change TV -> Other
# (the subtitle's destination, the checklist). Own QA root (seedwell-qa-catpill). Shots: app/build/qa/catpill-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-catpill"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
REPORT="$CTL.bench"; BIN="$QA_ROOT/fake-trash"; DL="$HOMEROOT/Downloads"
state() { rm -f "$REPORT"; qa_cmd "starting state" >/dev/null; tail -1 "$REPORT" 2>/dev/null; }
popover_line() { for i in 1 2 3 4 5 6 7 8; do l=$("$OUT/winid" Seedwell | awk '$2>0 && $4 ~ /^3[0-9][0-9]x/ {print; exit}'); [ -n "$l" ] && break; sleep 0.3; done; echo "$l"; }
shot_popover() { local l id; l=$(popover_line); id=${l%% *}; [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/catpill-$1.png" && echo "  catpill-$1 (popover $id, $(echo "$l" | awk '{print $4}'))" || echo "  MISSING $1"; }
pkill -f "SeedwellApp --state-dir $STATE" 2>/dev/null; sleep 1
rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT" "$DL" "$BIN"
QA_START_DELAY=120 qa_write_config
sed -i '' 's/^ask_before_adding = .*/ask_before_adding = "multi_file"/' "$STATE/config.toml"
cat >> "$STATE/config.toml" <<TOML
[folders.categories.other]
finished_dir = "~/Downloads/OtherStuff"
[[folders.watch]]
path = "~/Downloads/watch"
mode = "auto"
after_add = "leave"
TOML
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special season --start 1 >/dev/null
cp "$QA_ROOT"/lib/torrents/*Night.Harbor*.torrent "$DL/season.torrent"
: > "$CTL"; qa_wake
open -n --env "SEEDWELL_QA_TRASH_DIR=$BIN" "$APP" --args --state-dir "$STATE" --home-root "$HOMEROOT" --qa-control "$CTL" --qa-loopback-trackers --window-size 1180x820 --show-window
for i in $(seq 1 60); do [ -S "$SOCK" ] && break; sleep 0.5; done; sleep 2
qa_cmd "appearance dark" >/dev/null
qa_cmd "open-file $DL/season.torrent" >/dev/null; sleep 3; echo "$(state)"
shot_popover "compact"
qa_cmd "starting fold" >/dev/null; sleep 1.2
shot_popover "expanded-tv"
echo "== change TV -> Other"
qa_cmd "starting category Other" >/dev/null; sleep 2; echo "$(state)"
shot_popover "expanded-other"
qa_cmd "starting fold" >/dev/null; sleep 1.2
shot_popover "compact-other"
qa_cmd "starting category TV" >/dev/null; sleep 1.5; qa_cmd "starting fold" >/dev/null; sleep 1
echo "== the dropdown: a real click on the pill (menu windows are above the popover)"
qa_cmd "click at=290,15 hover win=popover" >/dev/null; sleep 1.2
id=$("$OUT/winid" Seedwell | awk '$2>0 {print $1" "$4}' | sort -k2 | head -3); echo "$id"
for w in $("$OUT/winid" Seedwell | awk '$2>0 {print $1}' | tail -1); do screencapture -x -o -l "$w" "$OUT/catpill-menu.png"; done
qa_cmd "starting cancel keep" >/dev/null
