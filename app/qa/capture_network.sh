#!/usr/bin/env bash
# Settings > Network (encryption and uTP now do something; no LSD / IPv6 toggles): dark and light
# into app/build/qa/settings-network-*.png. Own QA root (seedwell-qa-network), nothing is added.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-network"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
[ -x "$OUT/winid" ] || swiftc -O -o "$OUT/winid" "$REPO/app/qa/winid.swift" 2>/dev/null
main_id() { "$OUT/winid" Seedwell | awk '$2==0 && $4 ~ /^1[0-9][0-9][0-9]x/ {print $1; exit}'; }
qa_wake 600
qa_app_stop; sleep 1
rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT"
qa_write_config
qa_app_start --window-size 1180x820
sleep 2
QA_PID=$(pgrep -f "SeedwellApp --state-dir $STATE" | head -1); export QA_PID
for mode in dark light; do
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "settings network" >/dev/null; sleep 1
  qa_cmd "activate main" >/dev/null; sleep 0.5
  id=$(main_id)
  [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/settings-network-$mode.png" && echo "  settings-network-$mode"
done
echo "done; tear down with: QA_ROOT=$QA_ROOT app/qa/teardown.sh"
