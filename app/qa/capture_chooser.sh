#!/usr/bin/env bash
# "Deselect everything, then tick one" in the main-window chooser and in the menu-bar popover, with real
# mouse events posted into those two windows (NSEvent, never the global pointer). A season pack
# (Night Harbor S01, 8 episodes + Extras) is added with ask-before-adding so the checklist appears.
# Regression: an unticked box was a clear fill with a hairline, which hit-testing ignores, so after
# "none" no episode could be ticked again (the click fell through). Asserts the shared model's ticks
# after every click (`ui state` prints `choice <id> checked N confirm B primary "..."`), that the main
# run loop stays responsive and that the CPU stays idle. Also clicks every disclosure chevron (season,
# Extras; in the window and in the popover), on the glyph and beside it, and asserts that the expanded set
# changes while `checked` does not (the chevron once fell through to the row's tick). Captures: app/build/qa/chooser-*.png.
# Own QA root (seedwell-qa-chooser). Only processes carrying that root's --state-dir are signalled.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-chooser"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
FAIL=0
[ -x "$OUT/winid" ] || swiftc -O -o "$OUT/winid" "$REPO/app/qa/winid.swift" 2>/dev/null

if [ "${1:-}" != "reuse" ]; then
  pkill -f "SeedwellApp --state-dir $STATE" 2>/dev/null; sleep 1
  rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT"
  QA_START_DELAY=120 qa_write_config
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special season --start 1 >/dev/null
  qa_app_start --window-size 1180x820 --show-window
  sleep 2
  sed -i '' 's/^ask_before_adding = .*/ask_before_adding = "multi_file"/' "$STATE/config.toml"; sleep 5
  "$CLI" --socket "$SOCK" add "$(ls "$QA_ROOT"/lib/torrents/*Night.Harbor*.torrent | head -1)" >/dev/null
  sleep 3
fi
export QA_PID; QA_PID=$(pgrep -f "SeedwellApp --state-dir $STATE" | head -1)
[ -n "$QA_PID" ] || { echo "QA app not running"; exit 1; }

state() { rm -f "$CTL.bench"; qa_cmd "ui state" >/dev/null; sleep 0.2; sed 's/.*choice/choice/' "$CTL.bench" | tail -1; }
checked() { state | sed -n 's/.*checked \([0-9]*\) .*/\1/p'; }
# shot <name> <window-width-regex>: the popover is ~346 wide, the chooser 640
shot() { local id; id=$("$OUT/winid" Seedwell | awk -v w="$2" '$4 ~ w {print $1; exit}'); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/chooser-$1.png" && echo "  chooser-$1"; }
# click <window> <x,y> <expected checked> <what>
click() {
  qa_cmd "click at=$2 hover win=$1" >/dev/null; sleep 0.3
  local n; n=$(checked)
  if [ "$n" = "$3" ]; then echo "   ok [$1] $4 -> $n ticked"; else echo "   FAIL [$1] $4: wanted $3 ticked, got '$n' ($(state))"; FAIL=1; fi
}
expect() { if state | grep -qF -- "$1"; then echo "   ok: $1"; else echo "   FAIL: wanted '$1' in: $(state)"; FAIL=1; fi; }
alive() {
  qa_cmd "ui ping" >/dev/null; sleep 0.3
  local ms cpu; ms=$(grep '^ping:' "$CTL.bench" | tail -1 | sed 's/.*served in \([0-9]*\) ms.*/\1/'); cpu=$(ps -o %cpu= -p "$QA_PID" | tr -d ' ' | tr ',' '.')
  if [ "${ms:-9999}" -lt 250 ]; then echo "   ok: run loop answered in ${ms} ms (cpu ${cpu}%)"; else echo "   FAIL: run loop took ${ms} ms"; FAIL=1; fi
  [ "$(echo "${cpu:-0} < 60" | bc)" = 1 ] || { echo "   FAIL: cpu ${cpu}%"; FAIL=1; }
}

# Layout (window points, y from the top; measured from the screenshots).
# Popover: click points are the screenshot's x and (y - 26): the window is the 320 pt content plus a 13 pt margin all round and
# the click helper measures y from the top of the content view. Compact first: the header chevron 37,19 (there is no Select Files… button any more); opened: header chevron 37,19,
# Select all box 39,58, season chevron 47,88, season box 75,88, episode k box 89,(116+28(k-1)), its title 150,y; with the season
# folded the Extras row is at y 116.
POPSEL="37,19"
PE() { echo "89,$((116 + 28 * ($1 - 1)))"; }; PT() { echo "150,$((116 + 28 * ($1 - 1)))"; }
# Chooser (content = window, so points are screenshot pixels): quick picks y 327 (All 44, Latest season 252); season chevron 48,372,
# season box 83,372; episode k box 99,(409+34(k-1)), its title 300,y; with the season folded the Extras row is at y 413.
CE() { echo "99,$((409 + 34 * ($1 - 1)))"; }; CT() { echo "300,$((409 + 34 * ($1 - 1)))"; }
# expanded groups of the shared model, `ui state` prints `expanded [g0,...]`
expanded() { state | sed -n 's/.*expanded \[\([^]]*\)\].*/\1/p'; }
# chev <window> <x,y> <what>: a click on a chevron changes the expanded set and never the ticks
chev() {
  local before_e before_c after_e after_c
  before_e=$(expanded); before_c=$(checked)
  qa_cmd "click at=$2 hover win=$1" >/dev/null; sleep 0.4
  after_e=$(expanded); after_c=$(checked)
  if [ "$before_e" = "$after_e" ]; then echo "   FAIL [$1] $3: expanded stayed [$after_e]"; FAIL=1; qa_cmd "starting state" >/dev/null; tail -1 "$CTL.bench"; shot "fail-${2/,/_}" '^3[0-9][0-9]x'
  elif [ "$before_c" != "$after_c" ]; then echo "   FAIL [$1] $3: ticks changed $before_c -> $after_c"; FAIL=1
  else echo "   ok [$1] $3: expanded [$before_e] -> [$after_e], ticked $after_c"; fi
}

echo "== the popover and the chooser share one model"
expect 'choice'
qa_cmd "chooser first" >/dev/null; sleep 1.5
shot "window-start" '^640x'

echo "== chooser: every chevron only folds and unfolds"
click chooser 44,327 8 "quick pick All"
chev chooser 48,372 "season chevron on the glyph (fold)"
chev chooser 38,383 "season chevron beside the glyph (unfold)"
chev chooser 56,360 "season chevron at the other corner (fold)"
chev chooser 48,413 "Extras chevron on the glyph (unfold)"
chev chooser 40,421 "Extras chevron beside the glyph (fold)"
chev chooser 38,383 "season chevron again (unfold)"
shot "chevron" '^640x'
echo "== chooser: untick the 8 episodes one by one"
for k in 1 2 3 4 5 6 7 8; do click chooser "$(CE $k)" $((8 - k)) "untick S01E0$k"; done
expect 'confirm false'
shot "window-none" '^640x'
echo "== chooser: tick one box, then one row title, then untick the season box"
click chooser "$(CE 3)" 1 "tick S01E03"
expect 'primary "Start"'
click chooser "$(CT 5)" 2 "tick S01E05 by its title"
click chooser "$(CT 5)" 1 "untick S01E05 by its title"
shot "window-one" '^640x'
click chooser 83,372 8 "season box (mixed -> all)"
click chooser 83,372 0 "season box (all -> none)"
click chooser 83,372 8 "season box (none -> all)"
click chooser 83,372 0 "season box (all -> none)"
click chooser "$(CE 1)" 1 "tick S01E01 after the season went off"
click chooser 252,327 8 "quick pick Latest season"
alive

echo "== chooser: quick pick, then none through the season box, then one episode"
click chooser 83,372 0 "season box off"
click chooser "$(CE 8)" 1 "tick S01E08"
alive
qa_cmd "chooser-close" >/dev/null; sleep 1

echo "== popover: compact first; the chevron opens the list in place"
qa_cmd "starting show" >/dev/null; sleep 1.5
shot "popover-compact" '^3[0-9][0-9]x'
qa_cmd "click at=$POPSEL hover win=popover" >/dev/null; sleep 1
expect 'rows 10'
shot "popover-start" '^3[0-9][0-9]x'
echo "== popover: every chevron only folds and unfolds"
chev popover 47,88 "season chevron on the glyph (fold)"
chev popover 38,95 "season chevron beside the glyph (unfold)"
chev popover 56,82 "season chevron at the other corner (fold)"
chev popover 47,116 "Extras chevron on the glyph (unfold)"
chev popover 38,123 "Extras chevron beside the glyph (fold)"
chev popover 38,95 "season chevron again (unfold)"
echo "== popover: Select none, then tick one episode"
click popover 39,58 10 "Select all box (to all)"
click popover 39,58 0 "Select none box (to none)"
expect 'confirm false'
shot "popover-none" '^3[0-9][0-9]x'
click popover "$(PE 1)" 1 "tick S01E01 box"
expect 'primary "Start"'
click popover "$(PE 1)" 0 "untick S01E01 box"
click popover "$(PT 2)" 1 "tick S01E02 by its title"
shot "popover-one" '^3[0-9][0-9]x'
click popover "$(PT 2)" 0 "untick S01E02 by its title"
click popover 75,88 8 "season box (none -> all)"
click popover 75,88 0 "season box (all -> none)"
click popover "$(PE 4)" 1 "tick S01E04 right after"
click popover 39,58 10 "Select all box (mixed -> all)"
click popover 39,58 0 "Select none box"
click popover "$(PE 6)" 1 "tick S01E06 (the list shows seven rows; the rest scrolls)"
for k in 1 2 3; do click popover "$(PE $k)" $((1 + k)) "tick S01E0$k"; done
alive
shot "popover-end" '^3[0-9][0-9]x'

[ $FAIL = 0 ] && echo "chooser: PASS" || echo "chooser: FAIL"
# Own teardown only.
qa_cmd "quit" >/dev/null 2>&1; sleep 2
for pid in $(pgrep -f "SeedwellApp --state-dir $STATE" 2>/dev/null); do kill "$pid" 2>/dev/null; done
exit $FAIL
