#!/usr/bin/env bash
# Captures every view and tab of the running QA app (scenario.sh first), dark and light, with
# `screencapture -x -o -l <window id>`. PNGs go to app/build/qa/.
#
# The windows must draw in their ACTIVE state (colour traffic lights, accent selection): the display is
# kept awake (macOS refuses activation while it sleeps) and `activate <window>` precedes every shot.
set -uo pipefail
source "$(dirname "$0")/env.sh"
mkdir -p "$OUT"
qa_wake 7200

main_id() { "$OUT/winid" Seedwell | awk '$2==0 && $4 ~ /^1[0-9][0-9][0-9]x/ {print $1; exit}'; }
small_id() { "$OUT/winid" Seedwell | awk '$2==0 && $4 !~ /^1[1-9][0-9][0-9]x/ {print $1; exit}'; }
shot_main() { qa_cmd "activate main" >/dev/null; local id; id=$(main_id); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"; }
shot_other() { qa_cmd "activate $2" >/dev/null; local id; id=$(small_id); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"; }
shot_popups() { # a menu is a window of layer > 0
  local id; id=$("$OUT/winid" Seedwell | awk '$2>0 {print $1}' | head -1)
  [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"
}
first_download() { "$CLI" --socket "$SOCK" list downloading --json 2>/dev/null | python3 -c "import json,sys;d=json.load(sys.stdin)['transfers'];print(d[0]['id'] if d else '')"; }

for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "tracker none" "search " "info none" "scroll top"
  qa_cmd "section downloads"; shot_main "downloads-$mode"
  qa_cmd "tracker tracker-y"; shot_main "downloads-filter-$mode"
  qa_cmd "tracker none"
  id=$(first_download)
  if [ -n "$id" ]; then qa_cmd "select $id" "info $id"; sleep 1; shot_main "downloads-info-$mode"; qa_cmd "info none"; fi
  # Sharing: Popular, Active (with the Moving… row) on top; Dormant, Error and Dead further down
  qa_cmd "section sharing" "scroll top"; shot_main "sharing-$mode"
  qa_cmd "scroll bottom"; shot_main "sharing-bottom-$mode"; qa_cmd "scroll top"
  qa_cmd "tracker tracker-z"; sleep 1; shot_main "sharing-tracker-z-$mode"
  qa_cmd "tracker tracker-x"; shot_main "sharing-tracker-x-dead-$mode"
  qa_cmd "tracker public"; shot_main "sharing-public-$mode"
  qa_cmd "scroll bottom"; shot_main "sharing-public-error-$mode"; qa_cmd "scroll top"
  qa_cmd "tracker none"
  qa_cmd "search Common"; shot_main "sharing-search-$mode"; qa_cmd "search "
  # context menus (real NSMenu popups)
  qa_cmd "section downloads"
  qa_cmd "contextmenu Public.Domain"; sleep 1; shot_popups "contextmenu-download-$mode"; qa_cmd "contextmenu close"
  qa_cmd "section sharing"
  qa_cmd "contextmenu Big.Buck"; sleep 1; shot_popups "contextmenu-sharing-$mode"; qa_cmd "contextmenu close"
  # status item menu
  qa_cmd "menu open"; sleep 1; shot_popups "menu-bar-menu-$mode"; qa_cmd "menu close"
  # chooser: an unsure torrent (Other), a TV pack, and Change ▸ Movie updating the window in place
  # (changed back to TV afterwards so the light pass sees the same torrent)
  qa_cmd "section downloads" "chooser 0"; sleep 1.5; shot_other "chooser-$mode" chooser; qa_cmd "chooser-close"
  qa_cmd "chooser 1"; sleep 1.5; shot_other "chooser-tv-$mode" chooser
  qa_cmd "chooser-category movie"; sleep 1.5; shot_other "chooser-changed-$mode" chooser
  qa_cmd "chooser-category tv"; sleep 1; qa_cmd "chooser-close"
  # settings
  for tab in general transfers folders rules network agents advanced; do
    qa_cmd "settings $tab"; sleep 0.5; shot_other "settings-$tab-$mode" settings
  done
  qa_cmd "tracker-edit tracker-z"; sleep 0.8; shot_other "settings-trackers-$mode" settings
  qa_cmd "settings close"
done
qa_cmd "appearance dark"
ls "$OUT"/*.png | wc -l
