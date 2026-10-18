#!/usr/bin/env bash
# "Not enough space" on a row (SPEC 7A / 8.3): a torrent bigger than its destination volume (a 24 MB disk
# image mounted under the QA root) is added with that volume as its save path. The engine checks the
# bytes still to come against the free space when the download starts and puts the reason on the row
# ("Not enough space: 71.3 MB still to download, 22.9 MB free · Resume to try again").
# Run after scenario.sh (scenario.sh calls it); the image is detached by teardown.sh.
set -euo pipefail
source "$(dirname "$0")/env.sh"
qa_guard
VOL="$QA_ROOT/vol3"; IMG="$QA_ROOT/vol3.dmg"
if ! mount | grep -q " on $VOL "; then
  rm -f "$IMG"; mkdir -p "$VOL"
  hdiutil create -size 24m -type UDIF -fs "HFS+" -volname SeedwellTiny "$IMG" >/dev/null
  hdiutil attach "$IMG" -mountpoint "$VOL" -nobrowse >/dev/null
fi
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/nospace" --count 1 --start 14 --scale 100 --trackers "z=http://127.0.0.1:7101/announce" >/dev/null
"$CLI" --socket "$SOCK" add "$QA_ROOT/nospace/torrents"/*.torrent --save-path "$VOL/dl" >/dev/null || true
# The chooser already warns about the space (smart add); accept its default so the download starts and
# the engine's own check puts the reason on the row.
ID=$("$CLI" --socket "$SOCK" list --json --limit 500 | python3 -c "
import json,sys
print(next((t['id'] for t in json.load(sys.stdin)['transfers'] if '/vol3/' in t['save_path']), ''))")
[ -n "$ID" ] && "$CLI" --socket "$SOCK" choose "$ID" --default >/dev/null 2>&1 || true
echo "added a torrent that does not fit on $VOL"
