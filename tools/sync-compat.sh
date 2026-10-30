#!/usr/bin/env bash
# sync-compat: does this build's folder sync work with another Seedwell? (crates/sync-proto/PROTOCOL.md)
#
#   tools/sync-compat.sh OTHER                 OTHER = another `seedwell` binary (with the daemon), e.g. the
#                                              latest release's Seedwell.app/Contents/MacOS/seedwell
#   tools/sync-compat.sh --this BIN OTHER      this build's binary (default target/debug/seedwell)
#
# Starts both as headless daemons (`seedwell daemon`) with temporary state folders and HOME, on loopback only,
# and runs two rounds, each side the owner in turn: share a folder with a link, join it, sync files both ways,
# make the joiner Read only (its changes stop counting, it still receives), chat both ways in a channel, remove
# the joiner from the folder (it is cut off). Prints
# PASS or FAIL per step and exits 1 if any failed.
#
# The CLI tests run it with this build on both sides (crates/cli/tests/it/sync.rs). 0.1.6 is the first release
# with sync, so there is no older one to run against yet: from 0.1.7 on, CI should run it against the latest
# release's CLI as well.
set -euo pipefail
cd "$(dirname "$0")/.."

this="target/debug/seedwell"
if [ "${1:-}" = "--this" ]; then this="$2"; shift 2; fi
[ $# -eq 1 ] || { sed -n '2,15p' "$0"; exit 2; }
other="$1"
for b in "$this" "$other"; do
  [ -x "$b" ] || { echo "sync-compat: $b is not an executable (build this one with: tools/sysguard/heavy.sh cargo build -p trs-cli)" >&2; exit 2; }
done

# Short: a control socket's path must fit in sockaddr_un.
root=$(mktemp -d /tmp/swc.XXXXXX)
pids=()
cleanup() {
  for p in "${pids[@]:-}"; do [ -n "$p" ] && kill -TERM "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  rm -rf "$root"
}
trap cleanup EXIT
mkdir -p "$root/home"
export HOME="$root/home"
unset SEEDWELL_SOCKET

failed=0
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; failed=1; }

# A Python that runs: python3; on Windows "python3" may be only the Store's placeholder, so also
# python and the py launcher.
PY=""
for c in python3 python py; do
  if "$c" -c 'import sys' >/dev/null 2>&1; then PY="$c"; break; fi
done
[ -n "$PY" ] || { echo "sync-compat: no Python found (python3, python or py)"; exit 2; }

# A field of the JSON on stdin: `field a.0.b`.
field() {
  "$PY" -I -c '
import json, sys
v = json.load(sys.stdin)
for k in sys.argv[1].split("."):
    v = v[int(k)] if isinstance(v, list) else v.get(k)
    if v is None:
        break
print("" if v is None else v)' "$1"
}

# start NAME BINARY: a daemon; sets BIN_<NAME>, SOCK_<NAME>, PORT_<NAME>.
start() {
  local name="$1" bin="$2" dir="$root/$1"
  mkdir -p "$dir/state" "$dir/dl"
  "$bin" daemon --state-dir "$dir/state" --download-dir "$dir/dl" --bind 127.0.0.1 --no-dht --no-trackers \
    --permission full --socket "$root/$name.sock" >"$dir/out" 2>"$dir/err" &
  local pid=$!
  pids+=($pid)
  local line=""
  for _ in $(seq 1 600); do
    line=$(grep -m1 '^seedwell daemon ready' "$dir/out" 2>/dev/null || true)
    [ -n "$line" ] && break
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
  done
  [ -n "$line" ] || { echo "sync-compat: daemon $name did not start:"; cat "$dir/err"; exit 1; }
  local sock port
  sock=$(echo "$line" | tr ' ' '\n' | sed -n 's/^socket=//p')
  port=$(echo "$line" | tr ' ' '\n' | sed -n 's/^sync_port=//p')
  [ "$port" != "0" ] || { echo "sync-compat: daemon $name runs without folder sync"; exit 1; }
  eval "BIN_$name=\"\$bin\" SOCK_$name=\"\$sock\" PORT_$name=\"\$port\""
  echo "      $name: $("$bin" --version 2>/dev/null || echo "$bin"), sync port $port"
}

# cli NAME ARGS...: the CLI of that side, JSON out.
cli() {
  local name="$1"; shift
  local bin sock
  eval "bin=\$BIN_$name sock=\$SOCK_$name"
  "$bin" --socket "$sock" --json "$@"
}

# wait_for SECONDS COMMAND...: true once the command succeeds.
wait_for() {
  local secs="$1"; shift
  local end=$((SECONDS + secs))
  while [ $SECONDS -lt $end ]; do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  return 1
}

has() { [ "$(cat "$1" 2>/dev/null)" = "$2" ]; }
chat_has() { cli "$1" chat read "$2" --last 20 | grep -qF "\"$3\""; }
role_is() { [ "$(cli "$1" sync status "$2" | field role)" = "$3" ]; }
state_is() { [ "$(cli "$1" sync status "$2" | field state)" = "$3" ]; }

# round OWNER JOINER: the owner shares a folder, the joiner joins it, and so on.
round() {
  local owner="$1" joiner="$2" dir="$root/round-$1" id link dev oport
  local mine="$dir/owned" theirs="$dir/joined"
  mkdir -p "$mine"
  echo "seed from $owner" >"$mine/seed.txt"
  echo "--- $owner owns, $joiner joins"
  id=$(cli "$owner" sync add "$mine" | field id)
  link=$(cli "$owner" sync share "$id" --role read_write | field link)
  [ -n "$id" ] && [ -n "$link" ] || { fail "$owner shares a folder"; return; }
  echo "$link" | cli "$joiner" sync join - --to "$theirs" >/dev/null || { fail "$joiner joins with the link"; return; }
  eval "oport=\$PORT_$owner"
  cli "$joiner" sync address "$id" "127.0.0.1:$oport" >/dev/null
  if wait_for 60 has "$theirs/seed.txt" "seed from $owner"; then pass "$joiner joins and receives the files"; else fail "$joiner joins and receives the files"; return; fi
  echo "from $joiner" >"$theirs/back.txt"
  if wait_for 60 has "$mine/back.txt" "from $joiner"; then pass "$owner receives $joiner's change"; else fail "$owner receives $joiner's change"; fi

  dev=$(cli "$joiner" sync list | field device_id)
  cli "$owner" sync role "$id" "$dev" read_only >/dev/null || fail "$owner makes $joiner Read only"
  if wait_for 30 role_is "$joiner" "$id" read_only; then pass "$joiner learns it is Read only"; else fail "$joiner learns it is Read only"; fi
  echo "from $owner after the role change" >"$mine/after-role.txt"
  if wait_for 60 has "$theirs/after-role.txt" "from $owner after the role change"; then pass "a Read only $joiner still receives"; else fail "a Read only $joiner still receives"; fi
  echo "not taken" >"$theirs/late.txt"
  sleep 3
  if [ -e "$mine/late.txt" ]; then fail "$owner ignores a Read only device's change"; else pass "$owner ignores a Read only device's change"; fi

  # A channel between the two, the same way.
  local cid clink
  cid=$(cli "$owner" chat new "Plans $owner" | field id)
  clink=$(cli "$owner" chat share "$cid" | field link)
  if [ -n "$cid" ] && echo "$clink" | cli "$joiner" chat join - >/dev/null; then
    cli "$joiner" chat address "$cid" "127.0.0.1:$oport" >/dev/null
    for side in "$owner" "$joiner"; do cli "$side" chat agents "$cid" post >/dev/null; done
    echo "hello from $owner" | cli "$owner" chat send "$cid" - >/dev/null
    if wait_for 60 chat_has "$joiner" "$cid" "hello from $owner"; then pass "$joiner gets $owner's channel message"; else fail "$joiner gets $owner's channel message"; fi
    echo "hello from $joiner" | cli "$joiner" chat send "$cid" - >/dev/null
    if wait_for 30 chat_has "$owner" "$cid" "hello from $joiner"; then pass "$owner gets $joiner's channel message"; else fail "$owner gets $joiner's channel message"; fi
  else
    fail "$owner shares a channel with $joiner"
  fi

  cli "$owner" sync remove-device "$id" "$dev" >/dev/null || fail "$owner removes $joiner"
  if wait_for 30 state_is "$joiner" "$id" error; then pass "$joiner learns it was removed"; else fail "$joiner learns it was removed"; fi
  echo "after the removal" >"$mine/after-removal.txt"
  sleep 3
  if [ -e "$theirs/after-removal.txt" ]; then fail "a removed $joiner gets nothing new"; else pass "a removed $joiner gets nothing new"; fi
}

echo "sync-compat: this build ($this) with $other"
start this "$this"
start other "$other"
round other this
round this other
for p in "${pids[@]}"; do kill -0 "$p" 2>/dev/null || fail "a daemon stopped (see its log above)"; done
if [ $failed -eq 0 ]; then echo "sync-compat: PASS"; else echo "sync-compat: FAIL"; for n in this other; do echo "--- $n stderr"; tail -20 "$root/$n/err"; done; exit 1; fi
