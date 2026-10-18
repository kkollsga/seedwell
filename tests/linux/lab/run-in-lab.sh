#!/usr/bin/env bash
# run-in-lab.sh NAME SCRIPT [env VAR=val ...] : run lab/SCRIPT inside a fresh lab container, copy /out back to $RESULTS/NAME
# Needs the image `trs-lab` (see lab/Dockerfile) built from a context containing the Linux seedwell + tracker-probe.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
name="$1"; script="$2"; shift 2
RESULTS="${RESULTS:-$HERE/../results/lab}"; mkdir -p "$RESULTS/$name"
docker rm -f "lab-$name" >/dev/null 2>&1 || true
envs=(); for kv in "$@"; do envs+=(-e "$kv"); done
docker run -d --name "lab-$name" --cap-add NET_ADMIN --cap-add NET_RAW ${LAB_ARGS:-} "${envs[@]+"${envs[@]}"}" trs-lab sleep infinity >/dev/null
docker exec "lab-$name" mkdir -p /lab /in /out
docker cp "$HERE/." "lab-$name:/lab/"
[[ -d "${INPUTS:-}" ]] && docker cp "$INPUTS/." "lab-$name:/in/"
docker exec "${envs[@]+"${envs[@]}"}" "lab-$name" bash "/lab/$script" 2>&1 | tee "$RESULTS/$name/run.log" || true
docker cp "lab-$name:/out/." "$RESULTS/$name/" || true
[[ "${KEEP:-0}" = 1 ]] || docker rm -f "lab-$name" >/dev/null
