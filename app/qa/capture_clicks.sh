#!/usr/bin/env bash
# A multi-file row, its disclosure triangle and its file tree, in every order a user might "click on and off",
# through the calls those clicks make (`rowclick`, `files toggle`, `files select ... [command]`; no event is
# posted): asserts that the selection (row and file) keeps following them, that the main run loop stays
# responsive, and that no Tasks or handlers pile up. That a click lands on the control it is drawn over is
# HitAreaTests' part (app/Tests). Shots into app/build/qa/clicks-*.png. Own QA root (seedwell-qa-clicks).
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-clicks"
source "$(dirname "$0")/env.sh"
shot() { qa_shot "clicks-$1"; }
last_state() { tail -1 "$CTL.bench" 2>/dev/null | sed 's/^state: //'; }
# expect <substring of the state line>
expect() { if last_state | grep -qF -- "$1"; then echo "   ok: $1"; else echo "   FAIL: wanted '$1' in: $(last_state)"; FAIL=1; fi; }
ping_ok() { local ms; ms=$(qa_ask "ui ping" | grep '^ping:' | tail -1 | sed 's/.*served in \([0-9]*\) ms.*/\1/'); [ "${ms:-9999}" -lt 250 ] && echo "   ok: run loop answered in ${ms} ms" || { echo "   FAIL: run loop took ${ms} ms"; FAIL=1; }; }
# act <command>: what one click calls
act() { qa_cmd "$1"; }

qa_fresh
qa_write_config
# downloading (slowly, from a rate-limited local seeder): a season pack and the 2,000-file archive;
# finished (data in place): a movie folder, a corpus folder and some single files
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special season --start 1 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --special archive --start 2 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special bunny --start 3 >/dev/null
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --count 4 --start 10 >/dev/null
qa_seeder 7201 "$QA_ROOT/seed/data" 40000 || qa_end clicks
qa_seed "$QA_ROOT/seed/data" "$QA_ROOT/seed/torrents"/*.torrent
qa_app_start --window-size 1180x820 --show-window || qa_end clicks
for t in "$QA_ROOT/lib/torrents"/*.torrent; do "$CLI" --socket "$SOCK" add "$t" --save-path "$QA_ROOT/lib/data" >/dev/null 2>&1 || echo "add failed $t"; done
"$CLI" --socket "$SOCK" add "$QA_ROOT/seed/torrents"/*.torrent --peer 127.0.0.1:7201 >/dev/null
qa_until 30 eval 'qa_listed downloading Night && qa_listed downloading Archive' || { echo "  FAIL: the pack and the archive do not download"; FAIL=1; }

# ---- the sequence: Night Harbor (a season pack: Season 1 is file node 1, its Subs folder node 13) and Photo Archive
T=Night.Harbor
qa_cmd "section downloads" "window size 1180x820" "files collapse-all" "info none"
qa_wait "where !~ rows 0" 5 >/dev/null
echo "== rows: select, switch, command toggle"
act "rowclick Night Harbor"; act "rowclick Photo Archive"; act "rowclick Night Harbor"
qa_cmd "ui state"; expect 'rows ["Night Harbor'
act "rowclick Night Harbor command"; qa_cmd "ui state"; expect 'rows []'
act "rowclick Night Harbor command"; qa_cmd "ui state"; expect 'rows ["Night Harbor'
echo "== triangle, file, row, file"
act "files toggle $T"; qa_wait "ui state !~ :-" 5 >/dev/null; qa_cmd "ui state"; expect 'expanded ["'
act "files select $T Season"; qa_cmd "ui state"; expect 'files '; expect 'nodes [1]'; shot "file-selected"
act "rowclick Night Harbor"; qa_cmd "ui state"; expect 'rows ["Night Harbor'; expect 'files - '
act "files select $T Subs"; qa_cmd "ui state"; expect 'rows []'; expect 'nodes [13]'
act "files select $T Subs command"; qa_cmd "ui state"; expect 'nodes []'
echo "== rapid row / file alternation across the 1 s refresh"
# A user clicking fast: the lines arrive 0.3 s apart, whatever the app is doing.
for i in $(seq 1 10); do echo "rowclick Night Harbor" >> "$CTL"; sleep 0.3; echo "files select $T Subs" >> "$CTL"; sleep 0.3; done
act "files select $T Season"; qa_cmd "ui state"; expect 'nodes [1]'
act "rowclick Night Harbor"; qa_cmd "ui state"; expect 'rows ["Night Harbor'; shot "row-selected"
echo "== rapid triangle toggling (odd count ends collapsed), then Escape clears a file selection"
for i in $(seq 1 11); do echo "files toggle $T" >> "$CTL"; sleep 0.12; done
qa_cmd "ui state"; expect 'expanded []'
act "files toggle $T"; qa_wait "ui state !~ :-" 5 >/dev/null; act "files select $T Season"; qa_cmd "key escape list" "ui state"; expect 'nodes []'
echo "== run loop"
ping_ok
echo "== double click on a file (two clicks on its one-click handler), then a row"
echo "files select $T Subs" >> "$CTL"; act "files select $T Subs"; act "rowclick Night Harbor"; qa_cmd "ui state"; expect 'rows ["Night Harbor'
ping_ok
shot "end"
qa_end clicks
