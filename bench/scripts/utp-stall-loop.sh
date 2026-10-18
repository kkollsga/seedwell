#!/bin/bash
# Loop the uTP hot bench with per-connection tracing until a round takes >= STALL_S seconds.
#   bench/scripts/utp-stall-loop.sh OUTDIR [max_runs] [rounds_per_run] [stall_s]
# Needs TRS_BENCH_BIN. Per run it keeps OUTDIR/run-K/{bench.log,lo0.tsv,stderr.log} (stderr.log holds the
# TRS_UTP_TRACE lines: RTO, fast-retransmit loss, MTU probe result, zero window, persist probe, and a
# snapshot of every busy connection every 500 ms). lo0.tsv is the `netstat -I lo0 -b` timeline
# (wall ms, Obytes, every 100 ms) for finding when the transfer went quiet.
set -u
OUT=${1:?outdir}; MAX=${2:-10}; ROUNDS=${3:-10}; STALL=${4:-8}
cd "$(dirname "$0")/../.." || exit 1
mkdir -p "$OUT"
export TRS_FORBID_REAL_HOME=1 TRS_UTP_TRACE=1 BENCH_KEEP_PROFILE=1
for k in $(seq 1 "$MAX"); do
  d="$OUT/run-$k"; mkdir -p "$d"
  python3 -I bench/scripts/lo0-timeline.py "$d/lo0.tsv" 900 & TL=$!
  python3 -I bench/scripts/bench.py hot --clients torrentrs --n 200 --campaign "utpstall-$(date +%H%M%S)" --rounds "$ROUNDS" > "$d/bench.log" 2>&1
  kill $TL 2>/dev/null
  prof=$(ls -dt bench/runs/profiles/*hot-n200-torrentrs-r1 2>/dev/null | head -1)
  [ -n "$prof" ] && cp "$prof/stderr.log" "$d/stderr.log" 2>/dev/null && rm -rf "$prof"
  grep round "$d/bench.log"
  if awk -v s="$STALL" '/ round [0-9]+:/ { for (i=1;i<=NF;i++) if ($i=="in") { v=$(i+1); sub("s","",v); if (v+0 >= s) f=1 } } END { exit !f }' "$d/bench.log"; then
    echo "STALL reproduced in $d"; exit 0
  fi
done
echo "no stall in $MAX runs"
