#!/usr/bin/env bash
# `seedwell restart` on an isolated QA app (never the installed one, which this never looks for): below permission
# full it is refused with the request to ask for; at full the CLI answers once the app accepted, the app's own
# installer (install.sh --restart, in the app bundle) quits that process the proper way and opens the app again on
# the same isolated state (its relaunch arguments carry --state-dir and --home-root), and the CLI waits until the new
# run answers. Fails when the CLI does not come back with a new run, the old process lives on, or the state did not
# survive (a torrent added before is gone), or the agent activity log starts empty again (the restart's own entry is
# kept on disk, so the new run lists it). No shots. Own QA root (seedwell-qa-restart).
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-restart"
source "$(dirname "$0")/env.sh"
qa_fresh
qa_write_config
qa_app_start || qa_end restart
cli() { "$CLI" --socket "$SOCK" "$@"; }
# The app of this root, whatever arguments it was opened with (a relaunch puts --background first).
app_pid() { pgrep -f "SeedwellApp.*--state-dir $STATE" | head -1; }
# same LABEL GOT WANTED: exactly equal (a miss sets FAIL=1).
same() { if [ "$2" = "$3" ]; then echo "  ok: $1"; else echo "  FAIL: $1: wanted '$3', got '$2'"; FAIL=1; fi; }

old=$(app_pid)
[ -n "$old" ] || { echo "  FAIL: the QA app does not run"; FAIL=1; qa_end restart; }
cli add "magnet:?xt=urn:btih:5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed&dn=QA+Restart+Probe" --paused >/dev/null \
  || { echo "  FAIL: add"; FAIL=1; }

# Below full: refused, with the request that asks for it; nothing quits.
qa_set_config permission '"control"'
out=$(cli restart 2>&1); rc=$?
same "control is refused (exit 4)" "$rc" "4"
check "the refusal names the request" "$out" "seedwell agents request full"
same "nothing quit" "$(app_pid)" "$old"
qa_set_config permission '"full"'

# At full: the CLI answers, waits and prints the new run.
out=$(cli restart --timeout 3m 2>&1); rc=$?
echo "$out" | sed 's/^/    /'
same "restart exits 0" "$rc" "0"
check "it says what happens" "$out" "Restarting Seedwell: it quits and opens again"
check "it waited for the new run" "$out" "Seedwell is running again: process "
new=$(app_pid)
if [ -n "$new" ] && [ "$new" != "$old" ]; then echo "  ok: a new process runs"; else echo "  FAIL: no new process (old $old, now ${new:-none})"; FAIL=1; fi
check "the new process is the one the CLI named" "$out" "process ${new:-none},"
if kill -0 "$old" 2>/dev/null; then echo "  FAIL: the old process $old still runs"; FAIL=1; else echo "  ok: the old process is gone"; fi
check "the same state: the torrent added before is there" "$(cli list all 2>&1)" "QA Restart Probe"
check "the activity log survived: the new run lists the restart" "$(cli activity --limit 50 2>&1)" "restart Seedwell"
check "the restart's log says it reopened the app" "$(cat "$STATE/logs/update.log" 2>/dev/null)" "Opened Seedwell again"
qa_end restart
