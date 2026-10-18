#!/usr/bin/env bash
# The context menu and the menu bar's right-click menu of a held row (SPEC 8.5, 7A.6): a countdown row and a row that
# waits for a choice offer Start, Cancel and Choose Files… (no Pause or Start Now). Own QA root (seedwell-qa-heldmenu);
# fake Trash in the root. Shots: app/build/qa/heldmenu-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-heldmenu"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
BIN="$QA_ROOT/fake-trash"; DL="$HOMEROOT/Downloads"
shot_popups() { local id; id=$("$OUT/winid" Seedwell | awk '$2>0 {print $1}' | head -1); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/heldmenu-$1.png" && echo "  heldmenu-$1" || echo "  MISSING $1"; }
set_ask() { sed -i '' "s/^ask_before_adding = .*/ask_before_adding = \"$1\"/" "$STATE/config.toml"; sleep 5; }
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
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special archive --start 2 >/dev/null
cp "$QA_ROOT"/lib/torrents/*Night.Harbor*.torrent "$DL/season.torrent"
cp "$QA_ROOT"/lib/torrents/*Archive*.torrent "$DL/archive.torrent"
: > "$CTL"; qa_wake
open -n --env "SEEDWELL_QA_TRASH_DIR=$BIN" "$APP" --args --state-dir "$STATE" --home-root "$HOMEROOT" --qa-control "$CTL" --qa-loopback-trackers --window-size 1180x820 --show-window
for i in $(seq 1 60); do [ -S "$SOCK" ] && break; sleep 0.5; done; sleep 2
qa_cmd "appearance dark" >/dev/null
qa_cmd "open-file $DL/season.torrent" >/dev/null; sleep 3
set_ask always
qa_cmd "open-file $DL/archive.torrent" >/dev/null; sleep 3
qa_cmd "starting close" "window open" "section downloads" >/dev/null; sleep 1.5
for who in "Night.Harbor:countdown" "Public.Domain:awaiting"; do
  name=${who%%:*}; kind=${who##*:}
  qa_cmd "contextmenu $name"; sleep 1; shot_popups "context-$kind"; qa_cmd "contextmenu close"; sleep 1.2; sleep 1
  qa_cmd "contextmenu $name menubar"; sleep 1; shot_popups "menubar-$kind"; qa_cmd "contextmenu close"; sleep 1.2; sleep 1
done
pid=$(pgrep -f "SeedwellApp --state-dir $STATE" | head -1); [ -n "$pid" ] && kill "$pid"
