#!/usr/bin/env bash
# The "Starting in 7 s" notifier (SPEC 7A.7, 8.6): the popover with one torrent, three torrents and a paused
# countdown, dark and light; the menu bar row; a watch-folder add that raises the popover by itself; a
# frame sequence (and a screen recording when macOS allows it) of the countdown; and the restart that
# brings the popover back with a fresh countdown. Own QA root (seedwell-qa-start); shots go to
# app/build/qa/starting-*.png. Nothing outside the QA root is touched; only processes whose command
# line carries this root's --state-dir are ever signalled.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-start"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
WATCH="$HOMEROOT/Watch"
TORR="$QA_ROOT/torrents"
REPORT="$CTL.bench"

# A display that stays awake for the run, without touching anybody else's caffeinate.
qa_wake() { detach caffeinate -d -u -t 3600 >/dev/null 2>&1 & echo $! > "$QA_ROOT/caffeinate.pid"; sleep 0.5; }
qa_pid() { pgrep -f "SeedwellApp --state-dir $STATE" | head -1; }
stop_app() {
  qa_cmd "quit" >/dev/null 2>&1
  for i in $(seq 1 40); do [ -z "$(qa_pid)" ] && break; sleep 0.5; done
  local p; p=$(qa_pid); [ -n "$p" ] && kill "$p" 2>/dev/null
  return 0
}
set_delay() { python3 - "$STATE/config.toml" "$1" <<'PY'
import re, sys
p, v = sys.argv[1], sys.argv[2]
s = open(p).read()
s = re.sub(r'(?m)^start_delay_secs = \d+$', f'start_delay_secs = {v}', s)
open(p, 'w').write(s)
PY
}
add() { "$CLI" --socket "$SOCK" add "$@" >/dev/null 2>&1 || echo "add failed: $*"; }
state() { rm -f "$REPORT"; qa_cmd "starting state" >/dev/null; tail -1 "$REPORT" 2>/dev/null; }
# The popover is the one window above the menu level that is about 320 wide.
popover_line() { for i in 1 2 3 4 5 6 7 8; do l=$("$OUT/winid" Seedwell | awk '$2>0 && $4 ~ /^3[0-9][0-9]x/ {print; exit}'); [ -n "$l" ] && break; sleep 0.3; done; echo "$l"; }
shot_popover() { # name
  local l id; l=$(popover_line); id=${l%% *}
  [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/starting-$1.png" && echo "  starting-$1 (popover window $id)" || echo "  MISSING starting-$1"
  # ... and the menu bar around it, to see the anchor
  local xy wh x y w h
  xy=$(echo "$l" | awk '{print $3}'); wh=$(echo "$l" | awk '{print $4}')
  x=${xy%,*}; y=${xy#*,}; w=${wh%x*}; h=${wh#*x}
  [ -n "$id" ] && screencapture -x -R"$((x > 80 ? x - 80 : 0)),0,$((w + 160)),$((y + h + 24))" "$OUT/starting-$1-bar.png"
}

if [ "${1:-}" != "reuse" ]; then
  stop_app; sleep 1
  rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT" "$WATCH"
  QA_START_DELAY=60 qa_write_config
  cat >> "$STATE/config.toml" <<TOML
[[folders.watch]]
path = "~/Watch"
mode = "auto"
after_add = "leave"
TOML
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/swarm" --count 5 >/dev/null
  mkdir -p "$TORR"; cp "$QA_ROOT/swarm/torrents"/*.torrent "$TORR/"
  qa_app_start --window-size 1180x820
  sleep 2
fi
TS=($(ls "$TORR"/*.torrent))
echo "torrents: ${#TS[@]}"

# ---- the real path: a watch-folder add raises the popover by itself (no QA command) -------------------
cp "${TS[0]}" "$WATCH/"
for i in $(seq 1 30); do s=$(state); case "$s" in *"shown true"*) break;; esac; sleep 0.5; done
echo "watch-folder add: $s"
case "$s" in *"shown true, items 1"*) echo "  OK: the popover appeared for a watch-folder add";; *) echo "  FAIL: no popover for the watch-folder add";; esac

for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" >/dev/null
  qa_cmd "starting show" >/dev/null; sleep 1.2
  shot_popover "one-$mode"
done

# ---- three torrents -------------------------------------------------------------------------------
add "${TS[1]}"; add "${TS[2]}"
sleep 1.5
for mode in dark light; do
  qa_cmd "appearance $mode" >/dev/null
  qa_cmd "starting show" >/dev/null; sleep 1.2
  shot_popover "three-$mode"
done

# ---- paused ----------------------------------------------------------------------------------------
qa_cmd "starting pause" >/dev/null; sleep 1
for mode in dark light; do
  qa_cmd "appearance $mode" >/dev/null
  qa_cmd "starting show" >/dev/null; sleep 1.2
  shot_popover "paused-$mode"
done
state
qa_cmd "starting resume" >/dev/null

# ---- the menu bar menu lists them; the popover steps aside ---------------------------------------
qa_cmd "appearance dark" "starting close" >/dev/null
qa_cmd "menu open" >/dev/null; sleep 1.2
id=$("$OUT/winid" Seedwell | awk '$2>0 {print $1}' | head -1)
[ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/starting-menu-dark.png" && echo "  starting-menu-dark"
qa_cmd "menu close" >/dev/null

# ---- the countdown as a frame sequence: a watch-folder add, the popover rises by itself and closes by itself ----
for i in 1 2 3; do qa_cmd "starting now" >/dev/null; done   # clear the board: start what is still waiting
set_delay 9; sleep 3
qa_cmd "starting close" "appearance dark" >/dev/null
rm -f "$OUT"/starting-seq-*.png "$OUT"/starting-countdown.mov
cp "${TS[3]}" "$WATCH/"
id=""
for i in $(seq 1 40); do l=$(popover_line); id=${l%% *}; [ -n "$id" ] && break; sleep 0.2; done
if [ -n "$id" ]; then
  xy=$(echo "$l" | awk '{print $3}'); wh=$(echo "$l" | awk '{print $4}')
  x=${xy%,*}; y=${xy#*,}; w=${wh%x*}; h=${wh#*x}
  # a short screen recording of the same spot (needs the Screen Recording permission the captures already use)
  screencapture -x -v -V 11 -R"$((x > 40 ? x - 40 : 0)),0,$((w + 80)),$((y + h + 20))" "$OUT/starting-countdown.mov" >/dev/null 2>&1 &
  REC=$!
  n=0
  while [ $n -lt 12 ]; do
    n=$((n + 1)); screencapture -x -o -l "$id" "$OUT/starting-seq-$(printf %02d $n).png" 2>/dev/null || break
    sleep 0.6
  done
  echo "  $n frames starting-seq-NN.png"
  wait $REC 2>/dev/null
  sleep 1; echo "after the countdown: $(state)"
else
  echo "  FAIL: no popover for the sequence"
fi

# ---- a restart during the wait: held again, a fresh countdown, the popover is back ----------------------
set_delay 40; sleep 3
cp "${TS[4]}" "$WATCH/"
for i in $(seq 1 30); do s=$(state); case "$s" in *"shown true"*) break;; esac; sleep 0.5; done
echo "before the restart: $s"
sleep 8
echo "  after 8 s: $(state)"
stop_app; sleep 3
echo "app quit"
qa_app_start --window-size 1180x820
sleep 2.5
s=$(state); echo "after the restart: $s"
case "$s" in *"shown true, items 1"*"Starting in 3"*|*"shown true, items 1"*"Starting in 40"*) echo "  OK: back with a fresh countdown";; *) echo "  CHECK the line above";; esac
qa_cmd "appearance dark" >/dev/null
shot_popover "restart-dark"
