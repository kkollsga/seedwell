#!/usr/bin/env bash
# "Deselect everything, then tick one" in the main-window chooser and in the menu-bar popover, through the calls
# their clicks make on the one model both share (`choice tick|fold|all|pick`, `starting fold`; no event is posted).
# A season pack (Night Harbor S01, 8 episodes + Extras) is added with ask-before-adding so the checklist appears.
# Regression: an unticked box was a clear fill with a hairline, which hit-testing ignores, so after "none" no
# episode could be ticked again (the click fell through); that the box and the chevrons take a click on their whole
# frame is HitAreaTests' part (app/Tests). Asserts the shared model's ticks after every step (`ui state` prints
# `choice <id> checked N confirm B primary "..."`), that the main run loop stays responsive and that the CPU stays
# idle, and that every fold (season, Extras; in the window and in the popover) changes the expanded set while
# `checked` does not. Shots: app/build/qa/chooser-*.png.
# Own QA root (seedwell-qa-chooser). Only processes carrying that root's --state-dir are signalled.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-chooser"
source "$(dirname "$0")/env.sh"

qa_fresh
QA_START_DELAY=120 qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special season --start 1 >/dev/null
qa_app_start --window-size 1180x820 --show-window || qa_end chooser
qa_setting transfers.ask_before_adding=multi_file
"$CLI" --socket "$SOCK" add "$(ls "$QA_ROOT"/lib/torrents/*Night.Harbor*.torrent | head -1)" >/dev/null
qa_until 10 qa_listed awaiting-choice Harbor || { echo "  FAIL: the pack does not wait for a choice"; FAIL=1; }
QA_PID=$(qa_pid)

state() { qa_ask "ui state" | sed 's/.*choice/choice/' | tail -1; }
checked() { state | sed -n 's/.*checked \([0-9]*\) .*/\1/p'; }
# shot <name> <what>: the chooser window or the popover
shot() { qa_shot "chooser-$1" "$2"; }
# tick <window> <title> <expected checked> <what>: a box or a row's title in that window's checklist
tick() {
  qa_cmd "choice tick $2"
  local n; n=$(checked)
  if [ "$n" = "$3" ]; then echo "   ok [$1] $4 -> $n ticked"; else echo "   FAIL [$1] $4: wanted $3 ticked, got '$n' ($(state))"; FAIL=1; fi
}
# all|pick: the "Select all / none" box, a quick pick
step() { local win="$1" cmd="$2" want="$3" what="$4"
  qa_cmd "choice $cmd"
  local n; n=$(checked)
  if [ "$n" = "$want" ]; then echo "   ok [$win] $what -> $n ticked"; else echo "   FAIL [$win] $what: wanted $want ticked, got '$n' ($(state))"; FAIL=1; fi
}
expect() { local s; s=$(state); case "$s" in *"$1"*) echo "   ok: $1";; *) echo "   FAIL: wanted '$1' in: $s"; FAIL=1;; esac; }
alive() {
  local ms cpu; ms=$(qa_ask "ui ping" | grep '^ping:' | tail -1 | sed 's/.*served in \([0-9]*\) ms.*/\1/'); cpu=$(ps -o %cpu= -p "$QA_PID" | tr -d ' ' | tr ',' '.')
  if [ "${ms:-9999}" -lt 250 ]; then echo "   ok: run loop answered in ${ms} ms (cpu ${cpu}%)"; else echo "   FAIL: run loop took ${ms} ms"; FAIL=1; fi
  [ "$(echo "${cpu:-0} < 60" | bc)" = 1 ] || { echo "   FAIL: cpu ${cpu}%"; FAIL=1; }
}

# The rows by title: the season group, the Extras group, episode k.
SEASON="Season 1"; EXTRAS="Extras"
EP() { echo "E0$1"; }
# expanded groups of the shared model, `ui state` prints `expanded [g0,...]`
expanded() { state | sed -n 's/.*expanded \[\([^]]*\)\].*/\1/p'; }
# fold <window> <title> <what>: a group's chevron changes the expanded set and never the ticks
fold() {
  local before_e before_c after_e after_c
  before_e=$(expanded); before_c=$(checked)
  qa_cmd "choice fold $2"
  after_e=$(expanded); after_c=$(checked)
  if [ "$before_e" = "$after_e" ]; then echo "   FAIL [$1] $3: expanded stayed [$after_e]"; FAIL=1; qa_ask "starting state" | tail -1
  elif [ "$before_c" != "$after_c" ]; then echo "   FAIL [$1] $3: ticks changed $before_c -> $after_c"; FAIL=1
  else echo "   ok [$1] $3: expanded [$before_e] -> [$after_e], ticked $after_c"; fi
}

echo "== the popover and the chooser share one model"
qa_wait "ui state ~ choice" 10 >/dev/null
expect 'choice'
qa_cmd "chooser 0"; qa_wait "windows ~ chooser" 5 >/dev/null
shot "window-start" chooser

echo "== chooser: every chevron only folds and unfolds"
step chooser "pick All" 8 "quick pick All"
fold chooser "$SEASON" "season chevron (fold)"
fold chooser "$SEASON" "season chevron (unfold)"
fold chooser "$SEASON" "season chevron (fold)"
fold chooser "$EXTRAS" "Extras chevron (unfold)"
fold chooser "$EXTRAS" "Extras chevron (fold)"
fold chooser "$SEASON" "season chevron again (unfold)"
shot "chevron" chooser
echo "== chooser: untick the 8 episodes one by one"
for k in 1 2 3 4 5 6 7 8; do tick chooser "$(EP $k)" $((8 - k)) "untick S01E0$k"; done
expect 'confirm false'
shot "window-none" chooser
echo "== chooser: tick one box, then one more and off again, then the season box"
tick chooser "$(EP 3)" 1 "tick S01E03"
expect 'primary "Start"'
tick chooser "$(EP 5)" 2 "tick S01E05"
tick chooser "$(EP 5)" 1 "untick S01E05"
shot "window-one" chooser
tick chooser "$SEASON" 8 "season box (mixed -> all)"
tick chooser "$SEASON" 0 "season box (all -> none)"
tick chooser "$SEASON" 8 "season box (none -> all)"
tick chooser "$SEASON" 0 "season box (all -> none)"
tick chooser "$(EP 1)" 1 "tick S01E01 after the season went off"
step chooser "pick Latest season" 8 "quick pick Latest season"
alive

echo "== chooser: quick pick, then none through the season box, then one episode"
tick chooser "$SEASON" 0 "season box off"
tick chooser "$(EP 8)" 1 "tick S01E08"
alive
qa_cmd "chooser-close"

echo "== popover: compact first; the chevron opens the list in place"
qa_cmd "starting show"
shot "popover-compact" popover
qa_cmd "starting fold"
expect 'rows 10'
shot "popover-start" popover
echo "== popover: every chevron only folds and unfolds"
fold popover "$SEASON" "season chevron (fold)"
fold popover "$SEASON" "season chevron (unfold)"
fold popover "$SEASON" "season chevron (fold)"
fold popover "$EXTRAS" "Extras chevron (unfold)"
fold popover "$EXTRAS" "Extras chevron (fold)"
fold popover "$SEASON" "season chevron again (unfold)"
echo "== popover: Select none, then tick one episode"
step popover all 10 "Select all box (to all)"
step popover all 0 "Select none box (to none)"
expect 'confirm false'
shot "popover-none" popover
tick popover "$(EP 1)" 1 "tick S01E01"
expect 'primary "Start"'
tick popover "$(EP 1)" 0 "untick S01E01"
tick popover "$(EP 2)" 1 "tick S01E02"
shot "popover-one" popover
tick popover "$(EP 2)" 0 "untick S01E02"
tick popover "$SEASON" 8 "season box (none -> all)"
tick popover "$SEASON" 0 "season box (all -> none)"
tick popover "$(EP 4)" 1 "tick S01E04 right after"
step popover all 10 "Select all box (mixed -> all)"
step popover all 0 "Select none box"
tick popover "$(EP 6)" 1 "tick S01E06 (the list shows seven rows; the rest scrolls)"
for k in 1 2 3; do tick popover "$(EP $k)" $((1 + k)) "tick S01E0$k"; done
alive
shot "popover-end" popover
qa_end chooser
