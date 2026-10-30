#!/usr/bin/env bash
# The menu bar menu's downloading rows with a real speed and time left ("42% · 3.1 MB/s · 4 min left", SPEC 8.2):
# two big downloads from a rate-limited local seeder (a headless seedwell daemon on 127.0.0.1), one torrent with
# no source (it says why: "Waiting for peers"). The open menu is drawn twice a few seconds apart and must differ
# (the rows refresh once a second while the menu is open). Shot: app/build/qa/menu-downloading-speed.png.
# Own QA root (seedwell-qa-menu); only processes carrying that root are signalled.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-menu"
source "$(dirname "$0")/env.sh"
qa_fresh
qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --count 2 --start 1000 --scale 60 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/orphan" --count 1 --start 3000 --scale 30 >/dev/null
qa_seeder 7201 "$QA_ROOT/seed/data" 1500000 || qa_end menu_speed
qa_seed "$QA_ROOT/seed/data" "$QA_ROOT/seed/torrents"/*.torrent
qa_app_start --window-size 1180x820 --show-window || qa_end menu_speed
for t in "$QA_ROOT/seed/torrents"/*.torrent; do "$CLI" --socket "$SOCK" add "$t" --peer 127.0.0.1:7201 >/dev/null; done
"$CLI" --socket "$SOCK" add "$QA_ROOT/orphan/torrents"/*.torrent >/dev/null
qa_cmd "appearance dark" "menu open"
# Both downloads show a rate and a time left (the transfer has run long enough to estimate it).
check "the rows have a rate and a time left" "$(qa_wait "menu-subtitles ~ left" 40)" "left"
qa_shot "menu-downloading-speed" menu
first=$(qa_ask "menu-subtitles")
sleep 3   # the transfer goes on: the open menu's rows must follow
qa_shot "menu-downloading-speed-later" menu
later=$(qa_ask "menu-subtitles")
if cmp -s "$OUT/menu-downloading-speed.png" "$OUT/menu-downloading-speed-later.png" && [ "$first" = "$later" ]; then
  echo "  FAIL: the open menu did not change in 3 s"; FAIL=1
else echo "  ok: the open menu refreshed live"; fi
qa_cmd "menu close"
"$CLI" --socket "$SOCK" list 2>&1 | head -6
qa_end menu_speed
