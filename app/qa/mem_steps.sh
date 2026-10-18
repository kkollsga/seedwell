#!/usr/bin/env bash
# Footprint after each step: fresh start, Settings window, main window (Downloads), (Sharing), closed.
set -uo pipefail
source "$(dirname "$0")/env.sh"
qa_app_stop; sleep 3
qa_app_start
sleep "${SETTLE:-25}"
PID=$(pgrep -f "SeedwellApp --state-dir $STATE" | head -1)
fp() { printf "%-34s %s\n" "$1" "$(footprint -p $PID | grep 'phys_footprint:' | head -1 | awk '{print $2, $3}')"; }
# ColorSync keeps one 272 KB tone-curve table per curve of every colour conversion (mostly untouched
# pages, but address space and allocator bookkeeping). The status item window and the menu pop-up window
# are pinned to sRGB (StatusMenuController), which keeps it at a handful: more than 12 means a window
# went back to the display profile.
cs() { printf "%-34s %s\n" "  ColorSync 272 KB tables" "$(heap $PID 2>/dev/null | grep -o '272KB\[[0-9]*\]' | head -1)"; }
fp "fresh, nothing opened"; cs
qa_cmd "settings transfers"; sleep 2; fp "settings window open"
qa_cmd "settings close"; sleep 4; fp "settings closed"
qa_cmd "window open" "section downloads"; sleep 2; fp "main window, Downloads"
qa_cmd "section sharing"; sleep 3; fp "main window, Sharing (500 torrents)"
qa_cmd "window close"; sleep 5; fp "main window closed"
qa_cmd "window open" "section sharing"; sleep 2; qa_cmd "window close"; sleep 5; fp "closed again"
qa_cmd "menu open"; sleep 2; fp "menu open"; cs
qa_cmd "menu close"; sleep 3; fp "menu closed"
footprint -p $PID | sed -n 5,9p
