#!/usr/bin/env bash
# The Seedwell 0.0.1 round of UI changes, dark and light, into app/build/qa/: rows with speed and peers, Get Info
# (Downloads, Sharing, private tracker with masked URLs), the tracker summary, the Play icons on Recently Finished
# and Sharing rows, the window header and the sidebar gear; the Dock icon follows the window (`policy`: regular
# while it is open, accessory once it closed; a quiet QA run shows no Dock icon, it reports the one it would show).
# Uses the standard scene (scenario.sh, built when it does not run) with live numbers (live.sh).
set -uo pipefail
source "$(dirname "$0")/env.sh"
qa_guard
qa_scene || exit 1
[ -f "$QA_ROOT/live-ready" ] || bash "$(dirname "$0")/live.sh" >/dev/null 2>&1 || echo "live.sh failed (no live numbers)"
info() { qa_cmd "getinfo $1"; qa_wait "windows ~ info" >/dev/null || echo "  (Get Info of $1 did not open)"; }
for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "tracker none" "search " "info none"
  qa_cmd "section downloads" "scroll top"; qa_shot "downloads-$mode"
  qa_cmd "scroll bottom"; qa_shot "downloads-recent-play-$mode"; qa_cmd "scroll top"
  qa_cmd "window size 1180x1150"
  info Public.Domain.Radio.Plays.Vol.286; qa_shot "info-downloads-$mode" info; qa_cmd "info none"
  qa_cmd "section sharing" "scroll top"; qa_shot "sharing-$mode"
  info ubuntu-26.04; qa_shot "info-sharing-$mode" info
  qa_cmd "info-scroll bottom"; qa_shot "info-sharing-bottom-$mode" info; qa_cmd "info none"
  info Hidden.Rivers.2016.1080p.BluRay; qa_cmd "info-scroll bottom"; qa_shot "info-private-trackers-$mode" info; qa_cmd "info none"
  qa_cmd "window size 1180x820"
  qa_cmd "tracker tracker-z"; qa_shot "sharing-tracker-$mode"; qa_cmd "tracker none"
done
qa_cmd "appearance dark" "section downloads" "scroll bottom"
for g in play.circle play.circle.fill play.fill; do
  qa_cmd "playglyph $g"; qa_shot "play-glyph-$g"
done
qa_cmd "playglyph play.circle" "scroll top"
echo "== Dock icon: shown while the window is open, gone once it closed"
check "window open: in the Dock" "$(qa_ask "window open" "policy")" "policy: regular, main open"
check "window closed: not in the Dock" "$(qa_ask "window close" "policy")" "policy: accessory, main closed"
qa_cmd "window open" "appearance dark"
[ "$FAIL" = 0 ] && echo "capture_changes: PASS" || echo "capture_changes: FAIL"
exit "$FAIL"
