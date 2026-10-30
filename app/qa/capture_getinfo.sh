#!/usr/bin/env bash
# Get Info in the running app (SPEC 8.3): the card the core draws up (GetInfoCard) as the open popover has it
# (`getinfo-card`) and its values as drawn (`ui dump win=info`): the sections and lines, a refused tracker (the Problem
# in words, Ask Tracker Again, "Announcing stopped until you ask."), a tracker that is down (when it is tried again) and
# one that answers; the live status ticks and the torrent is read again while the card is open; the seeding goal
# pop-up; Get Info follows ↓.
# Loopback trackers only (7101 ok, 7103 refuses, 7104 down). Own QA root (seedwell-qa-getinfo); shots in app/build/qa.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-getinfo"
source "$(dirname "$0")/env.sh"
report() { qa_ask "$1"; }
card() { report "getinfo-card"; }
# The values the popover draws (its selectable texts), one per line.
drawn() { report "ui dump win=info" | sed -n "s/^ *AXStaticText ' = \(.*\)' x.*/\1/p" | sort -u; }
# Get Info's popover alone, and the window with it.
info_shot() { qa_shot "$1" info; qa_shot "$1-window"; }

qa_fresh
qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/swarm" --count 3 \
  --trackers "ok=http://127.0.0.1:7101/announce,refused=http://127.0.0.1:7103/announce,down=http://127.0.0.1:7104/announce" >/dev/null
qa_trackers_start
qa_app_start --window-size 1180x820 --show-window || qa_end getinfo
qa_add_all "$QA_ROOT/swarm/torrents" "$QA_ROOT/swarm/data"
# All three checked and announced once (the refused one is "Announcing stopped", the down one retries).
qa_until 30 eval '[ "$(qa_count finished)" -ge 3 ]' || echo "  (not all three are sharing)"
qa_until 30 eval 'grep -q . "$QA_ROOT/trk/7101.log" && grep -q . "$QA_ROOT/trk/7103.log"' || echo "  (the trackers heard nothing yet)"
qa_cmd "appearance dark" "window open" "window size 1180x820" "section sharing"
NAMES=()
while IFS= read -r n; do NAMES+=("$n"); done < <("$CLI" --socket "$SOCK" list 2>/dev/null | tail -n +2 | awk '{print $NF}')
echo "== ${#NAMES[@]} torrents: ${NAMES[*]}"

for n in "${NAMES[@]}"; do
  echo "== $n"
  out=$(report "getinfo $n"); echo "  $out"
  check "the popover shows its row" "$(report getinfo-window)" "shown, child true"
  # The tracker's verdict reaches the card once it announced.
  qa_wait "getinfo-card ~ tracker http" 10 >/dev/null
  c=$(card)
  for want in "section General" "section Peers" "section Trackers" "section Rule and goal" "Title: " "Category: " "Size: " \
      "State: " "Added: " "Ratio: " "Uploaded: " "Downloaded: " "Location: " "[Show in Finder]" "Files: " "Info-hash: " "Type: " \
      "Connected: " "Swarm (tracker): " "Rule: " "Then: menu " "buttons Show in Finder, Remove…"; do
    check "line '$want'" "$c" "$want"
  done
  d=$(drawn)
  title=$(echo "$c" | sed -n 's/^getinfo-card: \(.*\) | .*/\1/p')
  check "the popover draws the title" "$d" "$title"
  hash=$(echo "$c" | sed -n 's/^ *Info-hash: //p')
  check "and the info-hash" "$d" "$hash"
  case "$c" in
    *:7103/*)
      qa_wait "getinfo-card ~ Problem: The tracker refused this torrent" 10 >/dev/null; c=$(card); d=$(drawn)
      check "a refusal is explained" "$c" "Problem: The tracker refused this torrent"
      check "and waits for the user" "$c" "Next try: Not until you ask the tracker again"
      check "Ask Tracker Again" "$c" "[Ask Tracker Again] Announcing stopped until you ask."
      check "the tracker's live status" "$c" "Announcing stopped: the tracker refused this torrent."
      check "the popover draws the problem" "$d" "The tracker refused this torrent"
      info_shot "getinfo-refused";;
    *:7104/*)
      qa_wait "getinfo-card ~ Retrying tracker" 10 >/dev/null; c=$(card)
      check "a tracker that is down is tried again" "$c" "Retrying tracker"
      info_shot "getinfo-down";;
    *:7101/*)
      qa_wait "getinfo-card ~ Tracker OK" 10 >/dev/null; c=$(card)
      check "an answering tracker" "$c" "Tracker OK"
      lacks "nothing to ask again" "$c" "[Ask Tracker Again]"
      lacks "no problem" "$c" "Problem: "
      info_shot "getinfo-ok";;
    *) echo "  FAIL: no tracker on the card"; FAIL=1;;
  esac
  # The card ticks (its live status counts down) and reads the torrent again while it is open: time has to pass.
  sleep 3
  c2=$(card)
  [ "$c" != "$c2" ] && echo "  ok: the open card moved in 3 s" || echo "  note: nothing on the card changed in 3 s"
  diff <(echo "$c") <(echo "$c2") | grep '^[<>]' | head -4 | sed 's/^/    /'
  qa_cmd "info none"
done

echo "== Get Info follows ↓"
qa_cmd "info none" "key escape list" "key down" "key i cmd"
first=$(qa_wait "getinfo-window ~ shown, child true" 5)
qa_cmd "key down"
second=$(qa_wait "getinfo-window !~ $(echo "$first" | sed -n "s/.*row '\([^']*\)'.*/\1/p")" 5)
check "Get Info opened" "$first" "shown, child true"
check "and moved with ↓" "$second" "shown, child true"
[ "$first" != "$second" ] && echo "  ok: another row ($second)" || { echo "  FAIL: the same row after ↓"; FAIL=1; }
qa_cmd "info none"
qa_end getinfo
