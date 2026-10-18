#!/usr/bin/env bash
# Real mouse clicks (NSEvent posted into the QA window, never the global pointer) on a multi-file row,
# its chevron and its file tree, in every order a user might "click on and off": asserts that the
# selection (row and file) keeps following the clicks, that the main run loop stays responsive, and that
# no Tasks or handlers pile up. Captures into app/build/qa/clicks-*.png. Own QA root (seedwell-qa-clicks).
#   capture_clicks.sh [reuse]     reuse keeps the running scene
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-clicks"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
FAIL=0
shot() { qa_cmd "activate main" >/dev/null; local id; id=$("$OUT/winid" Seedwell | awk '$2==0 {print $1" "$4}' | sort -t' ' -k2 | tail -1 | cut -d' ' -f1); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/clicks-$1.png" && echo "  clicks-$1"; }
last_state() { tail -1 "$CTL.bench" 2>/dev/null | sed 's/^state: //'; }
# step "<label>" <wait> <commands...>: run, then print the state line
step() { local label="$1" wait="$2"; shift 2; qa_cmd "$@" >/dev/null; sleep "$wait"; qa_cmd "ui state" >/dev/null; echo "$label => $(last_state)"; }
# expect <substring of the state line>
expect() { if last_state | grep -qF -- "$1"; then echo "   ok: $1"; else echo "   FAIL: wanted '$1' in: $(last_state)"; FAIL=1; fi; }
ping_ok() { qa_cmd "ui ping" >/dev/null; sleep 0.3; local ms; ms=$(grep '^ping:' "$CTL.bench" | tail -1 | sed 's/.*served in \([0-9]*\) ms.*/\1/'); [ "${ms:-9999}" -lt 250 ] && echo "   ok: run loop answered in ${ms} ms" || { echo "   FAIL: run loop took ${ms} ms"; FAIL=1; }; }

TRS="$REPO/target/release/seedwell"
SEEDER_SOCK="$QA_ROOT/seeder/control.sock"

if [ "${1:-}" != "reuse" ]; then
  bash "$(dirname "$0")/teardown.sh" >/dev/null 2>&1
  sleep 1
  rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT"
  qa_write_config
  # downloading (slowly, from a rate-limited local seeder): a season pack and the 2,000-file archive;
  # finished (data in place): a movie folder, a corpus folder and some single files
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special season --start 1 >/dev/null
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special archive --start 2 >/dev/null
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special bunny --start 3 >/dev/null
  python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --count 4 --start 10 >/dev/null
  mkdir -p "$QA_ROOT/seeder"
  printf 'version = 1\n[transfers]\nupload_limit = 40000\n' > "$QA_ROOT/seeder/config.toml"
  TRS_FORBID_REAL_HOME=1 HOME="$QA_ROOT/seeder-home" detach "$TRS" daemon --state-dir "$QA_ROOT/seeder" --download-dir "$QA_ROOT/seed/data" --listen-port 7201 --bind 127.0.0.1 --no-dht --no-trackers --permission full --socket "$SEEDER_SOCK" >"$QA_ROOT/seeder.log" 2>&1 &
  for i in $(seq 1 40); do [ -S "$SEEDER_SOCK" ] && break; sleep 0.25; done
  for t in "$QA_ROOT/seed/torrents"/*.torrent; do "$TRS" --socket "$SEEDER_SOCK" add "$t" --save-path "$QA_ROOT/seed/data" >/dev/null; done
  qa_app_start --window-size 1180x820 --show-window
  for t in "$QA_ROOT/lib/torrents"/*.torrent; do "$CLI" --socket "$SOCK" add "$t" --save-path "$QA_ROOT/lib/data" >/dev/null 2>&1 || echo "add failed $t"; done
  "$CLI" --socket "$SOCK" add "$QA_ROOT/seed/torrents"/*.torrent --peer 127.0.0.1:7201 >/dev/null
  sleep 15
fi

# ---- the sequence (positions: 1180x820 window, Downloads: Night Harbor row y=79, Photo Archive row y=145;
# open Night Harbor tree: Season 1 y=141, Extras 162, Subs 183, then files every 21 pt)
qa_cmd "section downloads" "window size 1180x820" "files collapse-all" "info none" >/dev/null
sleep 1
click() { qa_cmd "click $1" >/dev/null; sleep "${2:-0.5}"; }
echo "== rows: select, switch, command toggle"
click "at=700,79 hover"; click "at=700,145 hover"; click "at=700,79 hover"
qa_cmd "ui state" >/dev/null; expect 'rows ["Night Harbor'
click "at=700,79 command"; qa_cmd "ui state" >/dev/null; expect 'rows []'
click "at=700,79 command"; qa_cmd "ui state" >/dev/null; expect 'rows ["Night Harbor'
echo "== chevron, file, row, file"
click "at=266,79" 1.2; qa_cmd "ui state" >/dev/null; expect 'expanded ["'
click "at=500,141 hover"; qa_cmd "ui state" >/dev/null; expect 'files '; expect 'nodes [1]'; shot "file-selected"
click "at=700,79 hover"; qa_cmd "ui state" >/dev/null; expect 'rows ["Night Harbor'; expect 'files - '
click "at=500,183 hover"; qa_cmd "ui state" >/dev/null; expect 'rows []'; expect 'nodes [13]'
click "at=500,183 command"; qa_cmd "ui state" >/dev/null; expect 'nodes []'
echo "== rapid row / file alternation across the 1 s refresh"
for i in $(seq 1 10); do echo "click at=700,79 hover" >> "$CTL"; sleep 0.3; echo "click at=500,183 hover" >> "$CTL"; sleep 0.3; done
sleep 1; click "at=500,141 hover"; qa_cmd "ui state" >/dev/null; expect 'nodes [1]'
click "at=700,79 hover"; qa_cmd "ui state" >/dev/null; expect 'rows ["Night Harbor'; shot "row-selected"
echo "== rapid chevron toggling (odd count ends collapsed), then Escape clears a file selection"
for i in $(seq 1 11); do echo "click at=266,79" >> "$CTL"; sleep 0.12; done
sleep 1.5; qa_cmd "ui state" >/dev/null; expect 'expanded []'
click "at=266,79" 1.2; click "at=500,141 hover"; qa_cmd "key escape" >/dev/null; sleep 0.4; qa_cmd "ui state" >/dev/null
echo "== run loop"
ping_ok
echo "== double click on a file, then a row"
click "at=500,183 double" 0.8; click "at=700,79 hover"; qa_cmd "ui state" >/dev/null; expect 'rows ["Night Harbor'
ping_ok
shot "end"
[ $FAIL = 0 ] && echo "clicks: PASS" || echo "clicks: FAIL"
# Own teardown only (the shared teardown.sh kills every caffeinate / tracker on the machine).
qa_cmd "quit" >/dev/null 2>&1; sleep 2
for pat in "SeedwellApp --state-dir $STATE" "seedwell daemon --state-dir $QA_ROOT"; do for pid in $(pgrep -f "$pat" 2>/dev/null); do kill "$pid" 2>/dev/null; done; done
exit $FAIL
