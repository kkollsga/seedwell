#!/usr/bin/env bash
# Play while downloading (SPEC 7F.3): a row with a Play button, the context menu, the Play With
# submenu (pack and single), the menu bar row's submenu, Get Info, the calm "streaming" note and
# Settings > General > Play videos with (closed and open), dark and light, into app/build/qa/play-*.png.
# Run after scenario.sh. Nothing launches a player: the QA build blocks launching, and `play` only
# acts on unfinished rows. The one real launch is `./capture_play.sh launch-test <App.app> <video>`.
set -uo pipefail
source "$(dirname "$0")/env.sh"
mkdir -p "$OUT"
qa_wake 7200

main_id() { "$OUT/winid" Seedwell | awk '$2==0 && $4 ~ /^1[0-9][0-9][0-9]x/ {print $1; exit}'; }
small_id() { "$OUT/winid" Seedwell | awk '$2==0 && $4 !~ /^1[1-9][0-9][0-9]x/ {print $1; exit}'; }
shot_main() { qa_cmd "activate main" >/dev/null; local id; id=$(main_id); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"; }
shot_other() { qa_cmd "activate $2" >/dev/null; local id; id=$(small_id); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"; }
shot_popups() { # a menu is a window of layer > 0; wait for it
  local id; for i in 1 2 3 4 5 6 7 8; do id=$("$OUT/winid" Seedwell | awk '$2>0 {print $1}' | head -1); [ -n "$id" ] && break; sleep 0.4; done
  [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1" || echo "  MISSING $1"; }

if [ "${1:-}" = "launch-test" ]; then
  # The one real launch: a finished local test video in a universal player. Close the player afterwards.
  qa_cmd "play-app $2 $3"; sleep 6; cat "$CTL.bench" | tail -3; exit 0
fi

# A movie and a season pack that are downloading slowly (the seeder of scenario.sh is throttled to
# 150 KB/s), so the Play button and the pack's episode menus have something to show for the whole run.
prep_scene() {
  local seeder="$QA_ROOT/seeder/control.sock"
  "$CLI" --socket "$seeder" settings set transfers.upload_limit=150000 >/dev/null
  # the seeder stops at its goal (ratio 1): keep it seeding
  for id in $("$CLI" --socket "$seeder" list --json | python3 -c "import json,sys;print(' '.join(t['id'] for t in json.load(sys.stdin)['transfers']))"); do
    "$CLI" --socket "$seeder" goal "$id" --forever >/dev/null
    "$CLI" --socket "$seeder" resume "$id" >/dev/null
  done
  "$CLI" --socket "$SOCK" settings set transfers.max_active_downloads=8 >/dev/null
  for name in Sintel Public.Domain.Theatre; do
    for id in $("$CLI" --socket "$SOCK" list --json --text "$name" | python3 -c "import json,sys;print(' '.join(t['id'] for t in json.load(sys.stdin)['transfers']))"); do
      "$CLI" --socket "$SOCK" remove "$id" --delete-files -y >/dev/null
    done
  done
  sleep 2
  for name in Sintel Public.Domain.Theatre; do
    for t in "$QA_ROOT/seed2/torrents"/*"$name"*.torrent; do "$CLI" --socket "$SOCK" add "$t" --peer 127.0.0.1:7201 >/dev/null; done
  done
  sleep 3
  # the pack is held for a choice (scenario.sh asks for multi-file torrents): take the default
  for id in $("$CLI" --socket "$SOCK" list awaiting-choice --json --text Public.Domain.Theatre | python3 -c "import json,sys;print(' '.join(t['id'] for t in json.load(sys.stdin)['transfers']))"); do
    "$CLI" --socket "$SOCK" choose "$id" --default >/dev/null
  done
  sleep 6
}
[ "${NO_PREP:-0}" = "1" ] || prep_scene

qa_cmd "play-players"; tail -8 "$CTL.bench"

movie=Sintel; pack=Theatre
for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "tracker none" "search " "info none" "scroll top" "section downloads"
  sleep 1; shot_main "play-row-$mode"
  qa_cmd "contextmenu $movie"; sleep 1; shot_popups "play-contextmenu-$mode"; qa_cmd "contextmenu close"
  qa_cmd "contextmenu $movie playwith"; sleep 1; shot_popups "play-with-$mode"; qa_cmd "contextmenu close"
  qa_cmd "contextmenu $pack"; sleep 1; shot_popups "play-contextmenu-pack-$mode"; qa_cmd "contextmenu close"
  qa_cmd "contextmenu $pack playwith"; sleep 1; shot_popups "play-with-pack-$mode"; qa_cmd "contextmenu close"
  qa_cmd "contextmenu $movie menubar"; sleep 1; shot_popups "play-menubar-submenu-$mode"; qa_cmd "contextmenu close"
  id=$("$CLI" --socket "$SOCK" list downloading --json 2>/dev/null | python3 -c "import json,sys;d=json.load(sys.stdin)['transfers'];print(next((t['id'] for t in d if '$pack' in t['name']),''))")
  if [ -n "$id" ]; then qa_cmd "select $id" "info $id"; sleep 1; shot_main "play-info-pack-$mode"; qa_cmd "info none"; fi
  qa_cmd "play $movie"; sleep 0.6; shot_main "play-notice-$mode"
  qa_cmd "settings general"; sleep 0.6; shot_other "play-settings-$mode" settings
  qa_cmd "settings-popup"; sleep 1; shot_popups "play-settings-popup-$mode"; qa_cmd "contextmenu close"
  qa_cmd "settings close"
done
qa_cmd "appearance dark"
ls "$OUT"/play-*.png | wc -l
