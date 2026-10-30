#!/usr/bin/env bash
# The menu bar menu's Sharing line with the combined upload rate at its right edge ("–" while idle), the
# Folder Sync line only once a folder is synced, and Settings › Folder Sync with the editable sync port and
# its router line, before and after the port moves (SPEC 8.2, 7G.5). Shots into app/build/qa/sync-port-*.png.
# Own QA root (seedwell-qa-syncport); only processes carrying it are signalled.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-syncport"
source "$(dirname "$0")/env.sh"
titles() { qa_ask "menu-titles" | grep '^menu:' | tail -1; }
port() { "$CLI" --socket "$SOCK" sync list --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["port"])'; }

qa_fresh
qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --count 2 --start 1000 >/dev/null
qa_app_start --window-size 1180x820 || qa_end sync_port
qa_add_all "$QA_ROOT/seed/torrents" "$QA_ROOT/seed/data"
qa_until 20 "$CLI" --socket "$SOCK" sync list
qa_until 20 eval '[ "$(qa_count finished)" -ge 2 ]'
qa_cmd "appearance dark"

# No synced folder: no Folder Sync line; the Sharing line carries the idle rate.
qa_cmd "menu open"
qa_shot sync-port-menu-no-folders menu
t=$(qa_wait "menu-titles ~ Sharing 2 torrents –" 10); echo "   $t"
lacks "no Folder Sync line" "$t" "Folder Sync"
check "Sharing line with the idle rate" "$t" "Sharing 2 torrents –"
qa_cmd "menu close"

# One synced folder: the line is there.
mkdir -p "$HOMEROOT/Docs"; echo hi > "$HOMEROOT/Docs/a.txt"
"$CLI" --socket "$SOCK" sync add "$HOMEROOT/Docs" >/dev/null
qa_cmd "menu open"
check "Folder Sync line with a folder" "$(qa_wait "menu-titles ~ Folder Sync" 10)" "Folder Sync"
qa_shot sync-port-menu-one-folder menu
qa_cmd "menu close"

# Settings › Folder Sync: the port field, then the port moved.
qa_cmd "window open" "window size 1180x820" "settings folderSync"
qa_shot sync-port-settings settings
before=$(port)
want=$(python3 -c 'import socket; s=socket.socket(); s.bind(("0.0.0.0",0)); print(s.getsockname()[1])')
"$CLI" --socket "$SOCK" sync settings --port "$want" | grep -i port
qa_until 10 eval '[ "$(port)" = "$want" ]'
now=$(port)
check "port $before -> $want" "$now" "$want"
# Settings shows the new port once it reloaded (the core's settings event).
qa_cmd "settings folderSync"
qa_shot sync-port-settings-moved settings
qa_end sync_port
