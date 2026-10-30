#!/usr/bin/env bash
# A finished torrent that gets more files is a download again (SPEC 8.2, 8.3, 7B.2): the user first picks
# S01 of a private two-season pack, it finishes and seeds (Recent / Sharing); then S02 is ticked in the
# file tree. The row must move to Downloads (progress, speed, time left), the menu must list it under
# Downloading, the window must switch to Downloads (the user re-selected files), and the local mock
# tracker must see the true `left`, no second `started` and `completed` exactly once.
# Shots app/build/qa/redl-*.png. A local rate-limited seeder (headless daemon on 127.0.0.1) and a local
# mock tracker (qa_tracker.py on 7105) serve it. Own QA root (seedwell-qa-redl); only processes carrying
# that root are signalled.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-redl"
source "$(dirname "$0")/env.sh"
TLOG="$QA_ROOT/trk/7105.log"
where() { qa_ask "where" | tail -1; }
count() { grep -c "$1" "$TLOG" 2>/dev/null || true; }
shot_menu() { # name: the menu as a click opens it; its titles in $MT
  qa_cmd "menu open"
  qa_shot "redl-$1" menu
  MT=$(qa_ask "menu-titles" | tail -1)
  qa_cmd "menu close"
}
shot_main() { qa_shot "redl-$1"; }
row_line() { "$CLI" --socket "$SOCK" list 2>/dev/null | grep -i harbor | head -1; }

qa_fresh
mkdir -p "$QA_ROOT/trk"
QA_START_DELAY=0 qa_write_config
sed -i '' 's/^ask_before_adding = .*/ask_before_adding = "multi_file"/' "$STATE/config.toml"
detach python3 -I "$REPO/app/qa/qa_tracker.py" --port 7105 --mode ok --interval 1800 --log "$TLOG" >"$TLOG.out" 2>&1 &
qa_until 5 qa_listening 7105
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special twoseasons --start 1 >/dev/null
T=$(ls "$QA_ROOT"/seed/torrents/*Harbor*.torrent | head -1)
qa_seeder 7201 "$QA_ROOT/seed/data" 2500000 || qa_end redownload
qa_seed "$QA_ROOT/seed/data" "$T"
qa_app_start --window-size 1180x820 --show-window || qa_end redownload
qa_cmd "appearance dark"

echo "== 1. S01 only: add (held for the choice), choose S01, it finishes and seeds"
"$CLI" --socket "$SOCK" add "$T" --peer 127.0.0.1:7201 >/dev/null 2>&1 || echo "add failed"
ID=$(python3 -c "import json;print(json.load(open('$QA_ROOT/seed/manifest.json'))[0]['infohash'])")
qa_until 10 qa_listed awaiting-choice Harbor
"$CLI" --socket "$SOCK" start "$ID" --skip 6-13 >/dev/null 2>&1 || echo "start failed"
qa_until 60 qa_listed recent Harbor
"$CLI" --socket "$SOCK" list recent 2>&1 | head -3
check "S01 finished and is Recent" "$("$CLI" --socket "$SOCK" list recent 2>&1)" "Harbor"
# The tracker hears the finish of a partial selection, if at all, with the next announce.
qa_until 5 grep -q completed "$TLOG" || true
echo "  tracker: completed=$(count completed) started=$(count started)"
[ "$(count completed)" = 0 ] || { echo "  FAIL: completed for a partial selection"; FAIL=1; }
qa_cmd "window open" "tab sharing" "files collapse-all"
shot_main "before-window"
shot_menu "before-menu"
check "menu has Recent" "$MT" "Recent"

echo "== 2. tick S02 in the file tree"
qa_cmd "files expand Harbor"
qa_wait "ui state ~ trees [\"" 5 >/dev/null; qa_wait "ui state !~ :-" 5 >/dev/null   # the file tree is loaded
qa_cmd "files folders Harbor" "files skip Harbor S02"
w=$(qa_wait "where ~ section downloads" 10); check "the window follows to Downloads" "$w" "section downloads"
qa_until 10 eval 'row_line | grep -q ownload'
echo "  row: $(row_line)"
check "engine: downloading again" "$(row_line)" "ownload"
# It announces the new `left` (a regular announce once the selection grew).
qa_until 15 eval 'tail -1 "$TLOG" | grep -q "left=[1-9]"' || true
shot_main "window"
shot_menu "menu"
check "menu: Downloading lists it" "$MT" "Downloading|"
lacks "menu: not under Recent" "$MT" "Recent"
echo "  tracker so far: $(grep -v '^$' "$TLOG" | sed 's/^[0-9]* 7105 [^ ]* //')"
check "left is true while downloading again" "$(tail -1 "$TLOG")" "left="
[ "$(count started)" = 1 ] || { echo "  FAIL: started count $(count started)"; FAIL=1; }
[ "$(count completed)" = 0 ] || { echo "  FAIL: completed before it finished"; FAIL=1; }

echo "== 3. it finishes again: one completed, back in Recent"
qa_until 120 qa_listed recent Harbor || { echo "  FAIL: it did not finish again"; FAIL=1; }
qa_until 10 grep -q completed "$TLOG"
echo "  tracker: $(sed 's/^[0-9]* 7105 [^ ]* //' "$TLOG" | tr '\n' ' ')"
check "completed once" "$(count completed)" "1"
[ "$(count started)" = 1 ] || { echo "  FAIL: started count $(count started)"; FAIL=1; }
shot_menu "finished-menu"
check "menu: Recent again" "$MT" "Recent"
qa_end redownload
