#!/usr/bin/env bash
# Every view and tab of the standard scene (scenario.sh, built when it does not run yet), dark and light, drawn
# in-process (qa_shot) into app/build/qa/: Downloads with Get Info, Sharing (Popular, Active with the Moving row,
# Error, Dead), tracker tabs, search, context menus, the menu bar menu, choosers incl. Change ▸ category in place,
# every Settings tab and the tracker editor. The scene stays up for capture_changes / capture_play.
set -uo pipefail
source "$(dirname "$0")/env.sh"
qa_guard
qa_scene || exit 1
first_download() { "$CLI" --socket "$SOCK" list downloading --json 2>/dev/null | python3 -c "import json,sys;d=json.load(sys.stdin)['transfers'];print(d[0]['id'] if d else '')"; }

for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "tracker none" "search " "info none" "scroll top"
  qa_cmd "section downloads"; qa_shot "downloads-$mode"
  qa_cmd "tracker tracker-y"; qa_shot "downloads-filter-$mode"
  qa_cmd "tracker none"
  id=$(first_download)
  if [ -n "$id" ]; then
    # `tracker none` shows Sharing: back to Downloads, where the row is
    qa_cmd "section downloads" "select $id" "info $id"; qa_wait "windows ~ info" >/dev/null || { echo "  FAIL: Get Info did not open"; FAIL=1; }
    qa_shot "downloads-info-$mode"; qa_shot "downloads-info-card-$mode" info; qa_cmd "info none"
  fi
  # Sharing: Popular, Active (with the Moving… row) on top; Dormant, Error and Dead further down
  qa_cmd "section sharing" "scroll top"; qa_shot "sharing-$mode"
  qa_cmd "scroll bottom"; qa_shot "sharing-bottom-$mode"; qa_cmd "scroll top"
  qa_cmd "tracker tracker-z"; qa_shot "sharing-tracker-z-$mode"
  qa_cmd "tracker tracker-x"; qa_shot "sharing-tracker-x-dead-$mode"
  qa_cmd "tracker public"; qa_shot "sharing-public-$mode"
  qa_cmd "scroll bottom"; qa_shot "sharing-public-error-$mode"; qa_cmd "scroll top"
  qa_cmd "tracker none"
  qa_cmd "search Common"; qa_shot "sharing-search-$mode"; qa_cmd "search "
  # context menus and the menu bar menu (their real items on a stand-in panel)
  qa_cmd "section downloads" "contextmenu Public.Domain"; qa_shot "contextmenu-download-$mode" menu; qa_cmd "contextmenu close"
  qa_cmd "section sharing" "contextmenu Big.Buck"; qa_shot "contextmenu-sharing-$mode" menu; qa_cmd "contextmenu close"
  qa_cmd "menu open"; qa_shot "menu-bar-menu-$mode" menu; qa_cmd "menu close"
  # chooser: an unsure torrent (Other), a TV pack, and Change ▸ Movie updating the window in place
  # (changed back to TV afterwards so the light pass sees the same torrent)
  qa_cmd "section downloads" "chooser 0"; qa_wait "windows ~ chooser" >/dev/null; qa_shot "chooser-$mode" chooser; qa_cmd "chooser-close"
  qa_cmd "chooser 1"; qa_wait "windows ~ chooser" >/dev/null; qa_shot "chooser-tv-$mode" chooser
  qa_cmd "chooser-category movie"; qa_shot "chooser-changed-$mode" chooser
  qa_cmd "chooser-category tv" "chooser-close"
  # settings
  for tab in general transfers folders rules network agents advanced; do
    qa_cmd "settings $tab"; qa_shot "settings-$tab-$mode" settings
  done
  qa_cmd "tracker-edit tracker-z"; qa_shot "settings-trackers-$mode" settings
  qa_cmd "settings close"
done
qa_cmd "appearance dark"
[ "$FAIL" = 0 ] && echo "capture_all: PASS" || echo "capture_all: FAIL"
exit "$FAIL"
