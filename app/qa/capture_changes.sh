#!/usr/bin/env bash
# The Seedwell 0.0.1 round of UI changes, dark and light, into app/build/qa/: rows with speed and peers,
# Get Info (Downloads, Sharing, private tracker with masked URLs), the tracker summary, the Play icons on
# Recently Finished and Sharing rows, the window header and the sidebar gear, the Dock icon. Needs scenario.sh
# and live.sh (live numbers) first. Dock shots only crop the Dock, never the rest of the screen.
set -uo pipefail
source "$(dirname "$0")/env.sh"
mkdir -p "$OUT"
qa_wake 7200
main_id() { "$OUT/winid" Seedwell | awk '$2==0 && $4 ~ /^1[0-9][0-9][0-9]x/ {print $1; exit}'; }
shot() { qa_cmd "activate main" >/dev/null; local id; id=$(main_id); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"; }
# The Dock strip only (the Dock is on the main display, bottom): the icon while a window is open.
shot_dock() {
  local b; b=$(osascript -e 'tell application "Finder" to get bounds of window of desktop' | tr -d ' ')
  local w h; w=$(echo "$b" | cut -d, -f3); h=$(echo "$b" | cut -d, -f4)
  screencapture -x -R"$((w/2-660)),$((h-110)),1320,110" "$OUT/$1.png" && echo "  $1"
}
for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "tracker none" "search " "info none"
  qa_cmd "section downloads" "scroll top"; shot "downloads-$mode"
  qa_cmd "scroll bottom"; shot "downloads-recent-play-$mode"; qa_cmd "scroll top"
  qa_cmd "window size 1180x1150"
  qa_cmd "getinfo Public.Domain.Radio.Plays.Vol.286"; sleep 1.5; shot "info-downloads-$mode"; qa_cmd "info none"
  qa_cmd "section sharing" "scroll top"; shot "sharing-$mode"
  qa_cmd "getinfo ubuntu-26.04"; sleep 1.5; shot "info-sharing-$mode"
  qa_cmd "info-scroll bottom"; shot "info-sharing-bottom-$mode"; qa_cmd "info none"
  qa_cmd "getinfo Hidden.Rivers.2016.1080p.BluRay"; sleep 1.5; qa_cmd "info-scroll bottom"; shot "info-private-trackers-$mode"; qa_cmd "info none"
  qa_cmd "window size 1180x820"
  qa_cmd "tracker tracker-z"; sleep 1; shot "sharing-tracker-$mode"; qa_cmd "tracker none"
  qa_cmd "where"
done
qa_cmd "appearance dark" "section downloads" "scroll bottom"
for g in play.circle play.circle.fill play.fill; do
  qa_cmd "playglyph $g"; sleep 1.2; shot "play-glyph-$g"
done
qa_cmd "playglyph play.circle" "scroll top"
# Dock: icon visible while the window is open (policy regular), gone when the last window closes.
qa_cmd "policy-real" "window open"; sleep 1; shot_dock "dock-window-open"
qa_cmd "window close"; sleep 1.5; shot_dock "dock-window-closed"; qa_cmd "window open"
qa_cmd "appearance dark"
