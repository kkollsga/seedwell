#!/usr/bin/env bash
# Play while downloading (SPEC 7F.3): a row with a Play button, the context menu, the Play With submenu (pack and
# single), the menu bar row's right-click menu, Get Info, the calm "streaming" note and Settings > General > Play
# videos with (closed and open), dark and light, into app/build/qa/play-*.png. Uses the standard scene (scenario.sh,
# built when it does not run). Nothing launches a player: the QA build blocks launching, and `play` only acts on
# unfinished rows. The one real launch is `./capture_play.sh launch-test <App.app> <video>`.
set -uo pipefail
source "$(dirname "$0")/env.sh"
qa_guard

if [ "${1:-}" = "launch-test" ]; then
  # The one real launch: a finished local test video in a universal player. Close the player afterwards.
  qa_cmd "play-app $2 $3"; qa_until 10 grep -q '^play-app:' "$CTL.bench"; tail -3 "$CTL.bench"; exit 0
fi
qa_scene || exit 1
ids() { "$CLI" --socket "$1" list ${3:-} --json --text "$2" | python3 -c "import json,sys;print(' '.join(t['id'] for t in json.load(sys.stdin)['transfers']))"; }

# A movie and a season pack that are downloading slowly (the seeder of scenario.sh is throttled to
# 150 KB/s), so the Play button and the pack's episode menus have something to show for the whole run.
prep_scene() {
  local seeder="$QA_ROOT/seeder/control.sock" id name t
  "$CLI" --socket "$seeder" settings set transfers.upload_limit=150000 >/dev/null
  # the seeder stops at its goal (ratio 1): keep it seeding
  for id in $(ids "$seeder" ""); do
    "$CLI" --socket "$seeder" goal "$id" --forever >/dev/null
    "$CLI" --socket "$seeder" resume "$id" >/dev/null
  done
  qa_setting transfers.max_active_downloads=8
  for name in Sintel Public.Domain.Theatre; do
    for id in $(ids "$SOCK" "$name"); do "$CLI" --socket "$SOCK" remove "$id" --delete-files -y >/dev/null; done
  done
  qa_until 10 eval '[ -z "$(ids "$SOCK" Sintel)$(ids "$SOCK" Public.Domain.Theatre)" ]'
  for name in Sintel Public.Domain.Theatre; do
    for t in "$QA_ROOT/seed2/torrents"/*"$name"*.torrent; do "$CLI" --socket "$SOCK" add "$t" --peer 127.0.0.1:7201 >/dev/null; done
  done
  # the pack is held for a choice (scenario.sh asks for multi-file torrents): take the default
  qa_until 15 eval '[ -n "$(ids "$SOCK" Public.Domain.Theatre awaiting-choice)" ]'
  for id in $(ids "$SOCK" Public.Domain.Theatre awaiting-choice); do "$CLI" --socket "$SOCK" start "$id" --suggested >/dev/null; done
  # both have their first pieces (the Play button needs some of the video)
  qa_until 30 eval '"$CLI" --socket "$SOCK" list downloading 2>/dev/null | grep -q Sintel'
}
[ "${NO_PREP:-0}" = "1" ] || prep_scene

qa_ask "play-players" | tail -8

movie=Sintel; pack=Theatre
for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "tracker none" "search " "info none" "scroll top" "section downloads"
  qa_shot "play-row-$mode"
  qa_cmd "contextmenu $movie"; qa_shot "play-contextmenu-$mode" menu; qa_cmd "contextmenu close"
  qa_cmd "contextmenu $movie playwith"; qa_shot "play-with-$mode" menu; qa_cmd "contextmenu close"
  qa_cmd "contextmenu $pack"; qa_shot "play-contextmenu-pack-$mode" menu; qa_cmd "contextmenu close"
  qa_cmd "contextmenu $pack playwith"; qa_shot "play-with-pack-$mode" menu; qa_cmd "contextmenu close"
  qa_cmd "contextmenu $movie menubar"; qa_shot "play-menubar-context-$mode" menu; qa_cmd "contextmenu close"
  id=$("$CLI" --socket "$SOCK" list downloading --json 2>/dev/null | python3 -c "import json,sys;d=json.load(sys.stdin)['transfers'];print(next((t['id'] for t in d if '$pack' in t['name']),''))")
  if [ -n "$id" ]; then
    qa_cmd "select $id" "info $id"; qa_wait "windows ~ info" >/dev/null; qa_shot "play-info-pack-$mode"; qa_cmd "info none"
  else
    echo "  FAIL: no downloading $pack"; FAIL=1
  fi
  qa_cmd "play $movie"; qa_shot "play-notice-$mode"
  qa_cmd "settings general"; qa_shot "play-settings-$mode" settings
  qa_cmd "settings-popup"; qa_shot "play-settings-popup-$mode" menu; qa_cmd "contextmenu close"
  qa_cmd "settings close"
done
qa_cmd "appearance dark"
[ "$FAIL" = 0 ] && echo "capture_play: PASS" || echo "capture_play: FAIL"
exit "$FAIL"
