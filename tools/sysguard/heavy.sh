#!/usr/bin/env bash
# heavy: runs one heavy command (cargo build/test, a bench, a VM) only when the Mac has room, and
# only one at a time across every agent and shell.
#
#   tools/sysguard/heavy.sh cargo test --release -p trs-engine
#
# Waits (printing why) while another heavy job holds the lock or memory is at or below MIN_FREE
# (default 40 %, from kern.memorystatus_level), then runs the command with CARGO_BUILD_JOBS=4 unless
# the caller set it. The lock is a directory, released when the command exits, even on Ctrl-C.
set -uo pipefail
MIN_FREE="${MIN_FREE:-40}"
# Machine-specific settings (untracked, optional): HEAVY_LOCK, SYSGUARD_LOG, SYSGUARD_EXTRA.
LOCAL_ENV="$(dirname "$0")/local.env"
# shellcheck source=/dev/null
[ -f "$LOCAL_ENV" ] && . "$LOCAL_ENV"
LOCK="${HEAVY_LOCK:-${TMPDIR:-/tmp}/seedwell-heavy.lock}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
[ $# -gt 0 ] || { echo "usage: heavy.sh <command...>" >&2; exit 2; }

said=""
while true; do
  if mkdir "$LOCK" 2>/dev/null; then
    echo "$$ $(date +%T) $*" > "$LOCK/owner"
    f=$(sysctl -n kern.memorystatus_level)
    [ "$f" -gt "$MIN_FREE" ] && break
    rm -rf "$LOCK"
    why="memory ${f}% free (need > ${MIN_FREE}%)"
  else
    owner=$(cat "$LOCK/owner" 2>/dev/null)
    pid=${owner%% *}
    # A lock left by a process that no longer exists is stale.
    if [ -n "$pid" ] && ! kill -0 "$pid" 2>/dev/null; then rm -rf "$LOCK"; continue; fi
    why="another heavy job is running: ${owner:-unknown}"
  fi
  [ "$why" != "$said" ] && echo "heavy: waiting, $why" >&2 && said="$why"
  sleep 10
done
trap 'rm -rf "$LOCK"' EXIT INT TERM
"$@"
