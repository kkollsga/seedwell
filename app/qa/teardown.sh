#!/usr/bin/env bash
# Tears the QA scene down: the app, the headless seeders and leechers, the loopback trackers, the
# caffeinate that keeps the display awake, and the throwaway disk images. The data under the QA root
# stays (delete the folder when it is not needed); nothing outside it was ever touched.
source "$(dirname "$0")/env.sh"
qa_guard
qa_cmd "quit" 2>/dev/null || true
for i in $(seq 1 60); do pgrep -f "SeedwellApp --state-dir $STATE" >/dev/null || break; sleep 1; done
for pat in "SeedwellApp --state-dir $STATE" "seedwell daemon --state-dir $QA_ROOT" "qa_tracker.py --port" "caffeinate -d -u -t"; do
  for pid in $(pgrep -f "$pat" 2>/dev/null); do kill "$pid" 2>/dev/null || true; done
done
for v in "$QA_ROOT/vol2" "$QA_ROOT/vol3" "$QA_ROOT/vol4"; do
  mount | grep -q " on $v " && hdiutil detach "$v" -force >/dev/null 2>&1
done
echo "QA scene torn down ($QA_ROOT kept)"
