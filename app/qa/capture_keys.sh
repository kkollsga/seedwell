#!/usr/bin/env bash
# The main window's keys in the running app (SPEC 8.3): ↓ picks the first row and moves on, ⇧↓ extends, ⌘A selects
# every row, ⌘I opens Get Info of the one selected row (not of several), Escape clears; Get Info follows ↑/↓ to the
# row moved to and closes when ⇧↓ leaves several selected; a new download while the "Add a Download" sheet is open only
# flags Downloads (the sheet, its text and the place stay), and so does a click on a title in the notifier (no dot
# while Downloads is shown). The rules are the core's (library_window.rs); the QA `key` command calls them as a key
# press does and `starting title` what the click on a title calls (no key or mouse events are posted: agents drive the
# app through commands). Own QA root (seedwell-qa-keys).
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-keys"
source "$(dirname "$0")/env.sh"
where() { qa_ask "where" | tail -1; }
key() { qa_cmd "key $*"; }
infowin() { qa_ask "getinfo-window" | tail -1; }
# A torrent added without --now waits in the notifier (the start delay below) for `starting title`.
add() { "$CLI" --socket "$SOCK" add "$@" >/dev/null 2>&1 || echo "add failed: $*"; }

qa_fresh
QA_START_DELAY=120 qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --count 7 --trackers public >/dev/null
TORRENTS=("$QA_ROOT"/lib/torrents/*.torrent)
qa_app_start --window-size 1180x820 --show-window || qa_end keys
for t in "${TORRENTS[@]:0:4}"; do add "$t" --now; done
qa_cmd "tab downloads"
qa_until 10 eval '[ "$(qa_count)" -ge 4 ]'
ROWS=$(qa_count)
# The window lists them and has taken each as a new download (the last one selected): no arrival comes later.
qa_wait "where ~ arrived $ROWS" 10 >/dev/null
echo "== $ROWS torrents"

echo "== arrows"
# The new downloads took the selection (the last one is selected); Escape in the list clears it (in the window it
# only drops a Remove… question).
key escape list
check "Escape: nothing selected" "$(where)" "selection 0"
key down
check "↓ picks the first row" "$(where)" "selection 1"
key down shift
check "⇧↓ extends the range" "$(where)" "selection 2"
key down
check "↓ moves to the next row alone" "$(where)" "selection 1"
key up shift
check "⇧↑ extends upwards" "$(where)" "selection 2"

echo "== ⌘A and ⌘I"
key i cmd
check "⌘I with several selected: no Get Info" "$(where)" "info none"
key a cmd
check "⌘A selects every row" "$(where)" "selection $ROWS"
key down
key i cmd
lacks "⌘I opens Get Info of the one selected row" "$(where)" "info none"
qa_wait "windows ~ info" >/dev/null
qa_shot "keys-getinfo"; qa_shot "keys-getinfo-card" info

echo "== Get Info follows the arrows"
qa_cmd "info none"; key escape list; key down; key i cmd
w1=$(where); key down; w2=$(where)
check "Get Info opened on the first row" "$w1" "selection 1"
lacks "there is a Get Info to follow" "$w1" "info none"
i1=${w1#*info }; i1=${i1%%,*}; i2=${w2#*info }; i2=${i2%%,*}
check "↓ keeps one row selected" "$w2" "selection 1"
case "$i2" in none|"$i1") echo "  FAIL: Get Info moves to the next row: $i1 -> $i2"; FAIL=1;; *) echo "  ok: Get Info moved with ↓ (${i1:0:8} -> ${i2:0:8})";; esac
check "the popover shows the row moved to" "$(qa_wait "getinfo-window ~ shown, child true" 5)" "shown, child true"
qa_shot "keys-getinfo-follows"
key up
w=$(where); i=${w#*info }; i=${i%%,*}
[ "$i" = "$i1" ] && echo "  ok: ↑ takes it back (${i:0:8})" || { echo "  FAIL: ↑ takes Get Info back to $i1: $w"; FAIL=1; }
key down shift
w=$(where)
check "⇧↓ selects two rows" "$w" "selection 2"
check "Get Info closes for several rows" "$w" "info none"
check "no popover is left" "$(qa_wait "getinfo-window ~ getinfo-window: none" 5)" "getinfo-window: none"
key escape

echo "== a new download while the add sheet is open"
qa_cmd "tab sharing" "add-sheet open"
check "the sheet is open" "$(where)" "sheet open"
add "${TORRENTS[4]}" --now
w=$(qa_wait "where ~ badge 1" 10)
check "the window stays on Sharing" "$w" "section sharing"
check "Downloads gets its dot" "$w" "badge 1"
check "the sheet stays open" "$w" "sheet open"
qa_shot "keys-sheet-busy"
qa_cmd "add-sheet close"

echo "== a click on a title in the notifier while the add sheet is open"
qa_cmd "tab downloads" "tab sharing" "add-sheet open" "add-sheet text magnet:?xt=urn:btih:qa-kept"
check "the dot went with a visit to Downloads" "$(where)" "badge 0"
check "the sheet has its text" "$(where)" "sheet open 'magnet:?xt=urn:btih:qa-kept'"
add "${TORRENTS[5]}"
qa_wait "starting state ~ items 1" 10 >/dev/null
qa_cmd "starting title"
w=$(where)
check "the window stays on Sharing" "$w" "section sharing"
check "nothing is selected behind the sheet" "$w" "selection 0"
check "Downloads gets its dot" "$w" "badge 1"
check "the sheet keeps its text" "$w" "sheet open 'magnet:?xt=urn:btih:qa-kept'"
qa_shot "keys-sheet-title-click"
# On Downloads behind the sheet: the row is listed there already, so no dot (it would stay on the place shown).
qa_cmd "add-sheet close" "tab downloads" "add-sheet open" "add-sheet text kept-on-downloads"
add "${TORRENTS[6]}"
qa_wait "starting state ~ items 2" 10 >/dev/null
qa_cmd "starting title"
w=$(where)
check "on Downloads the window stays" "$w" "section downloads"
check "no dot on Downloads shown" "$w" "badge 0"
check "the sheet keeps its text there too" "$w" "sheet open 'kept-on-downloads'"
# Closed: the click takes the window again (a fresh sheet starts empty).
qa_cmd "add-sheet close" "tab sharing" "starting title"
w=$(where)
check "without the sheet the click goes to Downloads" "$w" "section downloads"
check "and selects the row" "$w" "selection 1"
qa_cmd "add-sheet open"
check "a new sheet starts empty" "$(where)" "sheet open ''"
qa_cmd "add-sheet close" "starting close"
qa_end keys
