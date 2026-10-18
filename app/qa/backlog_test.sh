#!/usr/bin/env bash
# First-launch backlog offer (SPEC 7A.1): .torrent files that were already in ~/Downloads (the isolated
# home) are not mass-added; the app offers to add them once. Runs in its own temp root.
set -euo pipefail
export QA_ROOT=/private/tmp/claude-501/seedwell-qa-backlog
source "$(dirname "$0")/env.sh"
qa_guard
qa_app_stop || true; sleep 1
rm -rf "$QA_ROOT"; mkdir -p "$HOMEROOT/Downloads" "$STATE"
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/gen" --count 4 --trackers "public=" >/dev/null
cp "$QA_ROOT/gen/torrents"/*.torrent "$HOMEROOT/Downloads/"
# a normal first launch: no config.toml yet, defaults watch ~/Downloads
: > "$CTL"
open -n "$APP" --args --state-dir "$STATE" --home-root "$HOMEROOT" --qa-control "$CTL"
sleep 6
mkdir -p "$OUT"
id=$("$OUT/winid" Seedwell | awk '$2>0 {print $1; exit}')
[ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/backlog-offer-dark.png" && echo "captured the offer (window $id)"
qa_cmd "alert 1"
sleep 5
echo "torrents in the app:"; "$CLI" --socket "$SOCK" list | head -8
echo "left in Downloads: $(ls "$HOMEROOT/Downloads" | grep -c torrent) ; in Trash: $(ls "$HOMEROOT/.Trash" 2>/dev/null | grep -c torrent)"
qa_cmd "quit"
