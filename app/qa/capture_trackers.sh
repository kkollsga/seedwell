#!/usr/bin/env bash
# Automatic trackers and favorites (SPEC 7E.1, 8.3; own QA root seedwell-qa-trk): torrents on several
# local / made-up tracker hosts (127.0.0.1, localhost, *.test names that do not resolve) are grouped by
# the registrable domain of their primary tracker; the sidebar menus (Info, Save as Favorite / Rename,
# Remove from Favorites), the favorite saved, its Info page, the inline edits. Local loopback trackers
# only. Shots: app/build/qa/trackers-auto-*.png and trackers-info-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-trk"
source "$(dirname "$0")/env.sh"
favorites() { "$CLI" --socket "$SOCK" trackers --json 2>/dev/null | python3 -c "import json,sys;print(len(json.load(sys.stdin)['profiles']))"; }
qa_fresh
qa_write_config
T="a=http://127.0.0.1:7101/announce,c=http://tracker.example-a.test:7105/announce,c2=http://tracker.example-a.test:7105/announce,c3=http://bt.example-a.test:7105/announce,b=http://localhost:7102/announce,d=http://tracker.example-b.test:7106/announce,d2=http://tracker.example-b.test:7106/announce,e=http://announce.linuxdistro.co.uk:7107/announce,public=none"
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/swarm" --count 18 --trackers "$T" >/dev/null
qa_trackers_start
qa_app_start --window-size 1180x820 || qa_end trackers
qa_add_all "$QA_ROOT/swarm/torrents" "$QA_ROOT/swarm/data"
qa_until 30 eval '[ "$(qa_count finished)" -ge 18 ]' || { echo "  FAIL: not all 18 torrents are sharing"; FAIL=1; }
for mode in dark light; do
  echo "== $mode"
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "section sharing"
  qa_shot "trackers-auto-sidebar-$mode"
  qa_cmd "tab tracker example-a"
  qa_shot "trackers-auto-tab-$mode"
  qa_cmd "tracker-menu example-a"; qa_shot "trackers-auto-menu-$mode" menu; qa_cmd "tracker-menu close"
  qa_cmd "tracker-info example-a"
  qa_shot "trackers-info-auto-$mode"
  qa_cmd "favorite example-a"
  qa_until 10 eval '[ "$(favorites)" -ge 1 ]' || { echo "  FAIL: example-a is not a favorite"; FAIL=1; }
  qa_shot "trackers-auto-saved-$mode"
  qa_cmd "tracker-menu Example-a"; qa_shot "trackers-info-menu-favorite-$mode" menu; qa_cmd "tracker-menu close"
  qa_cmd "tracker-info Example-a"
  qa_shot "trackers-info-favorite-$mode"
  qa_cmd "fav editurl 0"
  qa_shot "trackers-info-edit-$mode"
  qa_cmd "fav type tracker.example-a.test:7105" "fav commit" "fav addurl"
  qa_shot "trackers-info-add-$mode"
  qa_cmd "fav type https://bt.example-a.test/announce?passkey=NOTASECRET123" "fav commit"
  qa_shot "trackers-info-passkey-note-$mode"
  qa_cmd "fav addurl" "fav type no spaces!" "fav commit"
  qa_shot "trackers-info-hint-$mode"
  qa_cmd "fav cancel" "fav rename Example-a"
  qa_shot "trackers-info-rename-$mode"
  qa_cmd "fav type Example A" "fav commit"
  qa_shot "trackers-info-renamed-$mode"
  qa_cmd "fav remove-ask Example A"
  qa_shot "trackers-info-remove-$mode"
  qa_cmd "fav cancel" "tracker-info close"
  qa_shot "trackers-info-back-$mode"
  if [ "$mode" = dark ]; then
    qa_cmd "unfavorite Example A" "section sharing"
    qa_until 10 eval '[ "$(favorites)" = 0 ]' || { echo "  FAIL: Example A is still a favorite"; FAIL=1; }
  fi
done
n=$(grep -c NOTASECRET123 "$STATE/trackers.toml" 2>/dev/null); echo "passkey in trackers.toml? $n"
cat "$STATE/trackers.toml" 2>/dev/null | head -20
qa_end trackers
