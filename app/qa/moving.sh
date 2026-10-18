#!/usr/bin/env bash
# The "Moving… 42%" row (SPEC 7A.5): a finished torrent is moved to another volume (a throwaway disk
# image mounted under the QA root) while the copy is throttled to 1 MiB/s (advanced.move_limit_mib).
# Run after scenario.sh; the copy of the ~170 MB torrent takes minutes, so captures can catch it.
set -euo pipefail
source "$(dirname "$0")/env.sh"
qa_guard
VOL="$QA_ROOT/vol2"; IMG="$QA_ROOT/vol2.sparseimage"
if [ ! -d "$VOL/.fseventsd" ] && ! mount | grep -q " on $VOL "; then
  rm -f "$IMG"; mkdir -p "$VOL"
  hdiutil create -size 2g -type SPARSE -fs APFS -volname SeedwellQA "$IMG" >/dev/null
  hdiutil attach "$IMG" -mountpoint "$VOL" -nobrowse >/dev/null
fi
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/mover" --count 1 --start 11 --scale 300 --trackers "z=http://127.0.0.1:7101/announce" >/dev/null
"$CLI" --socket "$SOCK" settings set transfers.max_active_downloads=10 >/dev/null
"$CLI" --socket "$SOCK" add "$QA_ROOT/mover/torrents"/*.torrent --save-path "$QA_ROOT/mover/data" >/dev/null || true
ID=$("$CLI" --socket "$SOCK" list --json --limit 500 | python3 -c "
import json,sys
print(next(t['id'] for t in json.load(sys.stdin)['transfers'] if '/mover/data/' in t['save_path']))")
for i in $(seq 1 60); do
  st=$("$CLI" --socket "$SOCK" info "$ID" --no-files --no-peers --no-trackers --json | python3 -c "import json,sys;print(json.load(sys.stdin)['transfer']['state'])")
  case "$st" in seeding|low-power) break;; esac; sleep 1
done
"$CLI" --socket "$SOCK" settings set transfers.max_active_downloads=3 >/dev/null
# Moves are unlimited by default; throttle this one so the green bar in the menu bar icon can be captured.
"$CLI" --socket "$SOCK" settings set advanced.move_limit_mib=1 >/dev/null
"$CLI" --socket "$SOCK" move "$ID" "$VOL/moved"
echo "moving $ID to $VOL/moved"
