#!/usr/bin/env bash
# The "Moving… 42%" row (SPEC 7A.5): a finished torrent is moved to another volume (a throwaway disk
# image mounted under the QA root) while the copy is throttled to 1 MiB/s (advanced.move_limit_mib).
# Run after scenario.sh; the copy of the ~170 MB torrent takes minutes, so captures can catch it.
set -euo pipefail
source "$(dirname "$0")/env.sh"
qa_guard
VOL="$QA_ROOT/vol2"; IMG="$QA_ROOT/vol2.sparseimage"
if ! mount | grep -q " on $VOL "; then
  rm -f "$IMG"; mkdir -p "$VOL"
  hdiutil create -size 2g -type SPARSE -fs APFS -volname SeedwellQA "$IMG" >/dev/null
  hdiutil attach "$IMG" -mountpoint "$VOL" -nobrowse >/dev/null
fi
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/mover" --count 1 --start 11 --scale 300 --trackers "z=http://127.0.0.1:7101/announce" >/dev/null
qa_setting transfers.max_active_downloads=10
"$CLI" --socket "$SOCK" add "$QA_ROOT/mover/torrents"/*.torrent --save-path "$QA_ROOT/mover/data" >/dev/null || true
ID=$("$CLI" --socket "$SOCK" list --json --limit 500 | python3 -c "
import json,sys
print(next(t['id'] for t in json.load(sys.stdin)['transfers'] if '/mover/data/' in t['save_path']))")
seeding() { "$CLI" --socket "$SOCK" info "$1" --no-files --no-peers --no-trackers --json 2>/dev/null | python3 -c "import json,sys;sys.exit(0 if json.load(sys.stdin)['transfer']['state'] in ('seeding','low-power') else 1)"; }
qa_until 60 seeding "$ID" || echo "the mover is not seeding yet"
qa_setting transfers.max_active_downloads=3
# Moves are unlimited by default; the scene's config.toml (qa_write_config) throttles them to 1 MiB/s so the green
# bar in the menu bar icon can be captured. Agents may not set advanced.* settings.
grep -q '^move_limit_mib = 1$' "$STATE/config.toml" || { echo "config.toml has no move_limit_mib = 1"; exit 1; }
"$CLI" --socket "$SOCK" move "$ID" "$VOL/moved"
echo "moving $ID to $VOL/moved"
