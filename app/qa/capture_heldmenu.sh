#!/usr/bin/env bash
# The context menu and the menu bar's right-click menu of a held row (SPEC 8.5, 7A.6): a countdown row and a row that
# waits for a choice offer Start, Cancel and Choose Files… (no Pause or Start Now). Own QA root (seedwell-qa-heldmenu);
# fake Trash in the root. Shots: app/build/qa/heldmenu-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-heldmenu"
source "$(dirname "$0")/env.sh"
BIN="$QA_ROOT/fake-trash"; DL="$HOMEROOT/Downloads"
qa_fresh
mkdir -p "$DL" "$BIN"
QA_WATCH="~/Downloads/watch" QA_START_DELAY=120 qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special season --start 1 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special archive --start 2 >/dev/null
cp "$QA_ROOT"/lib/torrents/*Night.Harbor*.torrent "$DL/season.torrent"
cp "$QA_ROOT"/lib/torrents/*Archive*.torrent "$DL/archive.torrent"
QA_APP_ENV="SEEDWELL_QA_TRASH_DIR=$BIN" qa_app_start --window-size 1180x820 --show-window || qa_end heldmenu
qa_cmd "appearance dark" "open-file $DL/season.torrent"
qa_wait "starting state ~ items 1" 10 >/dev/null
qa_setting transfers.ask_before_adding=always
qa_cmd "open-file $DL/archive.torrent"
qa_wait "starting state ~ items 2" 10 >/dev/null
qa_cmd "starting close" "window open" "section downloads"
for who in "Night.Harbor:countdown" "Public.Domain:awaiting"; do
  name=${who%%:*}; kind=${who##*:}
  check "$kind: Start, Cancel, Choose Files…, no Pause" "$(qa_ask "rowcommands $name")" "Choose Files"
  qa_cmd "contextmenu $name"; qa_shot "heldmenu-context-$kind" menu; qa_cmd "contextmenu close"
  qa_cmd "contextmenu $name menubar"; qa_shot "heldmenu-menubar-$kind" menu; qa_cmd "contextmenu close"
done
qa_end heldmenu
