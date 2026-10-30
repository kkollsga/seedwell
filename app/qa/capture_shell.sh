#!/usr/bin/env bash
# The window shell and the menu bar menu (the capture_files.sh scene, built here): the menu at rest and with hover
# (Open, Quit), the title area in Downloads and Sharing, rows with video icons, Settings as a tab, and the Play
# button on the largest video. Shots `shell-*.png`, dark and light, into app/build/qa/.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-files"
source "$(dirname "$0")/env.sh"
bash "$(dirname "$0")/capture_files.sh" scene >/dev/null || { echo "the files scene failed"; FAIL=1; qa_end shell; }
for mode in ${MODES:-dark light}; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "tracker none" "search " "info none" "files collapse-all" "section downloads"
  qa_shot "shell-downloads-$mode"
  qa_cmd "section sharing"; qa_shot "shell-sharing-$mode"
  qa_cmd "settings general"; qa_shot "shell-settings-general-$mode" settings
  qa_cmd "settings folders"; qa_shot "shell-settings-folders-$mode" settings
  qa_cmd "section downloads"
  qa_cmd "menu open"; qa_shot "shell-menu-rest-$mode" menu
  qa_cmd "menu-hover open"; qa_shot "shell-menu-hover-open-$mode" menu
  qa_cmd "menu-hover quit"; qa_shot "shell-menu-hover-quit-$mode" menu
  qa_cmd "menu-hover none" "menu close"
done
echo "--- play (largest video, launch blocked):"
out=$(qa_ask "play-row Buck" "play-row Night" "play-row Archive")
echo "$out"
check "Play on the movie" "$out" "play-row: Big Buck Bunny"
qa_end shell
