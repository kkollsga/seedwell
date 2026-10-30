#!/usr/bin/env bash
# The "Starting in 7 s" notifier (SPEC 7A.7, 8.6): the popover with one torrent, three torrents and a paused
# countdown, dark and light; the menu bar row; a watch-folder add that raises the popover by itself; the countdown
# as a sequence of shots, from the watch-folder add until it started by itself; and the restart that brings the
# popover back with a fresh countdown. Own QA root (seedwell-qa-start); shots app/build/qa/starting-*.png (the
# popover's content on a stand-in of its background: a quiet QA run shows no popover at the menu bar).
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-start"
source "$(dirname "$0")/env.sh"
WATCH="$HOMEROOT/Watch"
TORR="$QA_ROOT/torrents"
add() { "$CLI" --socket "$SOCK" add "$@" >/dev/null 2>&1 || echo "add failed: $*"; }
state() { qa_ask "starting state" | tail -1; }
shot_popover() { qa_shot "starting-$1" popover; }

qa_fresh
mkdir -p "$WATCH"
QA_WATCH="~/Watch" QA_START_DELAY=60 qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/swarm" --count 5 >/dev/null
mkdir -p "$TORR"; cp "$QA_ROOT/swarm/torrents"/*.torrent "$TORR/"
qa_app_start --window-size 1180x820 || qa_end starting
TS=($(ls "$TORR"/*.torrent))
echo "torrents: ${#TS[@]}"

# ---- the real path: a watch-folder add raises the popover by itself (no QA command) -------------------
# The watch folder takes only files newer than its stamp (`since`, whole seconds, written at the first launch).
qa_until 10 grep -q '^since = ' "$STATE/config.toml"
since=$(sed -n 's/^since = //p' "$STATE/config.toml" | head -1)
qa_until 3 eval '[ "$(date +%s)" -gt "${since:-0}" ]'
cp "${TS[0]}" "$WATCH/"
s=$(qa_wait "starting state ~ shown true" 15); echo "watch-folder add: $s"
check "the popover appeared for a watch-folder add" "$s" "shown true, items 1"

for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "starting show"
  shot_popover "one-$mode"
done

# ---- three torrents -------------------------------------------------------------------------------
add "${TS[1]}"; add "${TS[2]}"
check "three torrents wait" "$(qa_wait "starting state ~ items 3" 10)" "items 3"
for mode in dark light; do
  qa_cmd "appearance $mode" "starting show"
  shot_popover "three-$mode"
done

# ---- paused ----------------------------------------------------------------------------------------
qa_cmd "starting pause"
for mode in dark light; do
  qa_cmd "appearance $mode" "starting show"
  shot_popover "paused-$mode"
done
state
qa_cmd "starting resume"

# ---- the menu bar menu lists them; the popover steps aside ---------------------------------------
qa_cmd "appearance dark" "starting close" "menu open"
qa_shot "starting-menu-dark" menu
qa_cmd "menu close"

# ---- the countdown as a sequence of shots: a watch-folder add, the popover rises by itself and closes by itself ----
qa_cmd "starting now" "starting now" "starting now"   # clear the board: start what is still waiting
qa_set_config start_delay_secs 9
qa_cmd "starting close" "appearance dark"
rm -f "$OUT"/starting-seq-*.png
cp "${TS[3]}" "$WATCH/"
if qa_wait "starting state ~ shown true" 15 >/dev/null; then
  n=0; keep=$FAIL
  # One shot a second until the countdown has run out and the torrent started (the popover closes by itself).
  while [ $n -lt 14 ]; do
    n=$((n + 1))
    qa_shot "starting-seq-$(printf %02d $n)" popover >/dev/null || break
    sleep 0.6   # the countdown runs: the next frame shows it one step on
  done
  FAIL=$keep   # the shot after the popover closed by itself is missing, as it should be
  echo "  $((n - 1)) frames starting-seq-NN.png"
  [ $n -gt 3 ] || { echo "  FAIL: too few frames of the countdown"; FAIL=1; }
  s=$(qa_wait "starting state ~ shown false" 15); echo "after the countdown: $s"
  check "the popover closed by itself" "$s" "shown false"
else
  echo "  FAIL: no popover for the sequence"; FAIL=1
fi
# a frame that went missing because the popover had closed is not a failure
[ -n "${QA_SHOT_LOG:-}" ] && sed -i '' '/^MISSING starting-seq-/d' "$QA_SHOT_LOG"

# ---- a restart during the wait: held again, a fresh countdown, the popover is back ----------------------
qa_set_config start_delay_secs 40
cp "${TS[4]}" "$WATCH/"
s=$(qa_wait "starting state ~ shown true" 15); echo "before the restart: $s"
sleep 8   # part of the countdown runs out before the restart
echo "  after 8 s: $(state)"
qa_app_quit
echo "app quit"
qa_app_start --window-size 1180x820 || qa_end starting
s=$(qa_wait "starting state ~ shown true" 10); echo "after the restart: $s"
case "$s" in *"shown true, items 1"*"Starting in 3"*|*"shown true, items 1"*"Starting in 40"*) echo "  ok: back with a fresh countdown";; *) echo "  FAIL: not back with a fresh countdown"; FAIL=1;; esac
qa_cmd "appearance dark"
shot_popover "restart-dark"
qa_end starting
