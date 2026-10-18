#!/usr/bin/env bash
# Automatic trackers and favorites (SPEC 7E.1, 8.3; own QA root seedwell-qa-trk): torrents on several
# local / made-up tracker hosts (127.0.0.1, localhost, *.test names that do not resolve) are grouped by
# the registrable domain of their primary tracker; the sidebar menus (Info, Save as Favorite / Rename,
# Remove from Favorites), the favorite saved, its Info page, the inline edits. Local loopback trackers
# only. Shots: app/build/qa/trackers-auto-*.png and trackers-info-*.png. Tear down with
# QA_ROOT=/private/tmp/claude-501/seedwell-qa-trk app/qa/teardown.sh
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-trk"
source "$(dirname "$0")/env.sh"
qa_guard
mkdir -p "$OUT"
[ -x "$OUT/winid" ] || swiftc -O -o "$OUT/winid" "$REPO/app/qa/winid.swift" 2>/dev/null
main_id() { "$OUT/winid" Seedwell | awk '$2==0 && $4 ~ /^1[0-9][0-9][0-9]x/ {print $1; exit}'; }
shot() { qa_cmd "activate main" >/dev/null; sleep 0.4; local id; id=$(main_id); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"; }
# A context menu is its own window: capture the whole menu next to the main window.
shot_menu() { sleep 0.8; local id; id=$("$OUT/winid" Seedwell | awk '$2>0 {print $1}' | head -1); [ -n "$id" ] && screencapture -x -o -l "$id" "$OUT/$1.png" && echo "  $1"; }

if pgrep -fl 'state-dir /private/tmp/claude-501' | grep -v "seedwell-qa-trk" | grep -q SeedwellApp; then echo "another QA scene is running; wait"; exit 1; fi
qa_app_stop; sleep 1
rm -rf "$QA_ROOT"; mkdir -p "$QA_ROOT"
qa_write_config
T="a=http://127.0.0.1:7101/announce,c=http://tracker.example-a.test:7105/announce,c2=http://tracker.example-a.test:7105/announce,c3=http://bt.example-a.test:7105/announce,b=http://localhost:7102/announce,d=http://tracker.example-b.test:7106/announce,d2=http://tracker.example-b.test:7106/announce,e=http://announce.linuxdistro.co.uk:7107/announce,public=none"
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/swarm" --count 18 --trackers "$T" >/dev/null
qa_trackers_start
qa_app_start --window-size 1180x820
sleep 2
qa_add_all "$QA_ROOT/swarm/torrents" "$QA_ROOT/swarm/data"
sleep 5
for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "section sharing" >/dev/null; sleep 1
  shot "trackers-auto-sidebar-$mode"
  qa_cmd "tab tracker example-a" >/dev/null; sleep 1.5
  shot "trackers-auto-tab-$mode"
  qa_cmd "tracker-menu example-a" >/dev/null; shot_menu "trackers-auto-menu-$mode"; qa_cmd "tracker-menu close" >/dev/null
  qa_cmd "tracker-info example-a" >/dev/null; sleep 2
  shot "trackers-info-auto-$mode"
  qa_cmd "favorite example-a" >/dev/null; sleep 3
  shot "trackers-auto-saved-$mode"
  qa_cmd "tracker-menu Example-a" >/dev/null; shot_menu "trackers-info-menu-favorite-$mode"; qa_cmd "tracker-menu close" >/dev/null
  qa_cmd "tracker-info Example-a" >/dev/null; sleep 2
  shot "trackers-info-favorite-$mode"
  qa_cmd "fav editurl 0" >/dev/null; sleep 0.8
  shot "trackers-info-edit-$mode"
  qa_cmd "fav type tracker.example-a.test:7105" "fav commit" >/dev/null; sleep 0.5
  qa_cmd "fav addurl" >/dev/null; sleep 0.8
  shot "trackers-info-add-$mode"
  qa_cmd "fav type https://bt.example-a.test/announce?passkey=NOTASECRET123" "fav commit" >/dev/null; sleep 1
  shot "trackers-info-passkey-note-$mode"
  qa_cmd "fav addurl" "fav type no spaces!" "fav commit" >/dev/null; sleep 0.6
  shot "trackers-info-hint-$mode"
  qa_cmd "fav cancel" "fav rename Example-a" >/dev/null; sleep 0.8
  shot "trackers-info-rename-$mode"
  qa_cmd "fav type Example A" "fav commit" >/dev/null; sleep 1
  shot "trackers-info-renamed-$mode"
  qa_cmd "fav remove-ask Example A" >/dev/null; sleep 0.8
  shot "trackers-info-remove-$mode"
  qa_cmd "fav cancel" "tracker-info close" >/dev/null; sleep 0.8
  shot "trackers-info-back-$mode"
  [ "$mode" = dark ] && { qa_cmd "unfavorite Example A" "section sharing" >/dev/null; sleep 3; }
done
echo "passkey in trackers.toml? $(grep -c NOTASECRET123 "$STATE/trackers.toml" 2>/dev/null)"
cat "$STATE/trackers.toml" 2>/dev/null | head -20
echo "done; tear down with QA_ROOT=$QA_ROOT app/qa/teardown.sh"
