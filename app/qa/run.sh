#!/usr/bin/env bash
# The QA runner: app/qa/run.sh [script ...|all]
#
# Runs the given capture scripts (names with or without capture_ / .sh: `run.sh keys chooser`), or every one
# (`all`, the default), one at a time; each uses its own QA root and tears it down. Prints per script PASS or FAIL,
# its time and its shots, then a summary; exits non-zero when any failed. Logs: app/build/qa/logs/<script>.log.
#
# The app under test is quiet (--qa-control, QuietRun): it never becomes the active or frontmost app, never takes
# the focus or a key, and shows no window on a screen; the shots are drawn in-process. Run it any time, while you
# work, with the display asleep or the screen locked. Build the app first (rebuild.sh or SEEDWELL_PROFILE=dev bash
# app/build.sh, through tools/sysguard/heavy.sh).
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/env.sh"
LOGS="$OUT/logs"
mkdir -p "$LOGS"
[ -x "$APPBIN" ] || { echo "no app at $APP: build it first (app/qa/rebuild.sh)"; exit 2; }
[ -x "$TRS" ] || echo "note: no seedwell daemon at $TRS (scripts with a local seeder need: cargo build --release -p trs-cli)"

# The scripts that share the standard scene (scenario.sh, built by the first of them): they run one after the other
# and the scene is torn down after the last.
SCENE="capture_all capture_changes capture_play"
all() { (cd "$HERE" && ls capture_*.sh backlog_test.sh) | sed 's/\.sh$//' | sort; }
name() { local n="${1%.sh}"; n="${n##*/}"; [ -f "$HERE/$n.sh" ] || n="capture_$n"; echo "$n"; }

list=()
if [ $# -eq 0 ] || [ "$1" = all ]; then
  for s in $SCENE; do list+=("$s"); done
  for s in $(all); do case " $SCENE " in *" $s "*) ;; *) list+=("$s");; esac; done
else
  for a in "$@"; do n=$(name "$a"); [ -f "$HERE/$n.sh" ] || { echo "no script $a"; exit 2; }; list+=("$n"); done
fi

scene_down() { QA_ROOT=/private/tmp/claude-501/seedwell-qa bash "$HERE/teardown.sh" >/dev/null 2>&1; }
trap 'scene_down' EXIT
results=(); failed=0; t_all=$SECONDS
for i in "${!list[@]}"; do
  s=${list[$i]}
  next=${list[$((i + 1))]:-}
  export QA_SHOT_LOG="$LOGS/$s.shots"; : > "$QA_SHOT_LOG"
  printf '%-26s ' "$s"
  t0=$SECONDS
  bash "$HERE/$s.sh" > "$LOGS/$s.log" 2>&1; rc=$?
  dt=$((SECONDS - t0))
  # The standard scene stays up for the next script that shares it.
  case " $SCENE " in *" $s "*) case " $SCENE " in *" $next "*) ;; *) scene_down;; esac;; esac
  shots=$(grep -vc '^MISSING ' "$QA_SHOT_LOG" | tr -d ' ')
  missing=$(grep '^MISSING ' "$QA_SHOT_LOG" | sed 's/^MISSING //' | tr '\n' ' ')
  if [ $rc = 0 ] && [ -z "$missing" ]; then verdict=PASS; else verdict=FAIL; failed=$((failed + 1)); fi
  printf '%s %4d s  %3d shots\n' "$verdict" "$dt" "$shots"
  [ "$shots" -gt 0 ] && grep -v '^MISSING ' "$QA_SHOT_LOG" | tr '\n' ' ' | fold -s -w 110 | sed 's/^/    /'
  [ "$shots" -gt 0 ] && echo
  if [ $verdict = FAIL ]; then
    [ -n "$missing" ] && echo "    missing: $missing"
    grep -E 'FAIL|MISSING|qa_cmd:|not running|did not start' "$LOGS/$s.log" | head -8 | sed 's/^/    /'
    echo "    (log: $LOGS/$s.log)"
  fi
  results+=("$verdict $s ${dt}s")
done
echo "---"
echo "$(( ${#list[@]} - failed )) passed, $failed failed, ${#list[@]} scripts in $((SECONDS - t_all)) s; shots in $OUT"
[ $failed = 0 ]
