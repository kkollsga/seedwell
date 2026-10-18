#!/usr/bin/env bash
# sysguard: watches memory, swap and load on this Mac while agents build, test and bench Seedwell,
# and steps in before macOS runs out of memory (a 16 GB Mac mini once hit ~44 GB of footprints and
# WindowServer's watchdog rebooted it).
#
#   tools/sysguard/sysguard.sh            run in the foreground; one line per state change
#   SYSGUARD_LOG=/path/log                also append every sample (default: $TMPDIR/seedwell-sysguard.log)
#
# Levels, from `kern.memorystatus_level` (the % of memory macOS considers available):
#   ok        > WARN_FREE (25)
#   warn      <= 25 %, or swap > 4 GB, or 1-min load > 3x cores: printed, nothing is touched
#   critical  <= CRIT_FREE (12) %, or swap > 8 GB: this project's heavy jobs are stopped, newest
#             first, until memory is back above WARN_FREE: `cargo test` / `cargo build` / rustc /
#             test binaries / bench_client / seedwell daemons started from this repo (or a folder
#             in SYSGUARD_EXTRA). Nothing else on the Mac is ever touched; colima is reported,
#             not stopped (it may hold someone else's containers).
set -uo pipefail
WARN_FREE="${WARN_FREE:-25}"
CRIT_FREE="${CRIT_FREE:-12}"
INTERVAL="${INTERVAL:-5}"
# Machine-specific settings (untracked, optional): SYSGUARD_LOG, SYSGUARD_EXTRA (an extra regex of folders).
LOCAL_ENV="$(dirname "$0")/local.env"
# shellcheck source=/dev/null
[ -f "$LOCAL_ENV" ] && . "$LOCAL_ENV"
LOG="${SYSGUARD_LOG:-${TMPDIR:-/tmp}/seedwell-sysguard.log}"
NCPU=$(sysctl -n hw.ncpu)
# Our processes: command lines that mention the repo (also as an agent's dashed project folder name).
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OURS="${REPO_ROOT}|seedwell-qa|$(echo "$REPO_ROOT" | tr / -)/${SYSGUARD_EXTRA:+|$SYSGUARD_EXTRA}"

free_pct() { sysctl -n kern.memorystatus_level; }
swap_mb() { sysctl -n vm.swapusage | awk '{for(i=1;i<=NF;i++) if($i=="used") {v=$(i+2); sub(/M/,"",v); sub(/,/,".",v); print int(v)}}'; }
load1() { sysctl -n vm.loadavg | awk '{gsub(",",".",$2); print $2}'; }

# Heavy processes of ours, newest (highest pid) first: "pid rss_mb command".
ours_heavy() {
  ps -axo pid=,rss=,command= | awk -v re="$OURS" '
    $0 ~ re && $3 ~ /(rustc|cargo|bench_client|seedwell|deps\/|python3)/ && $0 !~ /sysguard|heavy\.sh/ {
      printf "%s %d %s\n", $1, $2/1024, substr($0, index($0,$3), 90) }' | sort -rn
}

stop_ours() {
  local killed=0
  while read -r pid rss cmd; do
    [ -z "$pid" ] && continue
    kill -TERM "$pid" 2>/dev/null && { echo "$(date +%T) STOPPED pid $pid (${rss} MB): $cmd"; killed=$((killed+1)); }
    sleep 1
    [ "$(free_pct)" -gt "$WARN_FREE" ] && break
  done < <(ours_heavy)
  [ "$killed" -eq 0 ] && [ "${told:-}" != 1 ] && told=1 && echo "$(date +%T) CRITICAL but none of this project's jobs are running; biggest processes:" && \
    ps -axo rss=,comm= | sort -rn | head -5 | awk '{printf "    %6d MB  %s\n", $1/1024, $2}'
}

mkdir -p "$(dirname "$LOG")"
last=""
echo "$(date +%T) sysguard: warn <= ${WARN_FREE}% free, critical <= ${CRIT_FREE}% free, every ${INTERVAL}s, log $LOG"
while true; do
  f=$(free_pct); s=$(swap_mb); l=$(load1)
  level=ok
  awk -v l="$l" -v n="$NCPU" 'BEGIN{exit !(l > 3*n)}' && level=warn
  { [ "$f" -le "$WARN_FREE" ] || [ "${s:-0}" -gt 4096 ]; } && level=warn
  { [ "$f" -le "$CRIT_FREE" ] || [ "${s:-0}" -gt 8192 ]; } && level=critical
  line="$(date +%T) $level free ${f}% swap ${s}MB load ${l}"
  echo "$line" >> "$LOG"
  if [ "$level" != "$last" ]; then
    echo "$line"
    if [ "$level" != ok ]; then
      ours_heavy | head -6 | awk '{printf "    ours: pid %s %s MB %s\n", $1, $2, substr($0, index($0,$3))}'
      pgrep -q -f "com.apple.Virtualization.VirtualMachine" 2>/dev/null && echo "    a VM is running (colima?)"
    fi
    last=$level
    told=0
  fi
  [ "$level" = critical ] && stop_ours
  sleep "$INTERVAL"
done
