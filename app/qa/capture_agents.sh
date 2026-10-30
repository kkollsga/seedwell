#!/usr/bin/env bash
# Settings > Agents (SPEC 9A.4): agent access, the Command line group (Copy CLI Path, Install
# Command-Line Tool..., Install Skill for Claude Code) and the activity list, dark and light. Own QA
# root (seedwell-qa-agents); the app runs with an isolated home, so nothing is written to the real
# ~/.claude or the real PATH folders (the shots only look; no button is pressed). Shots: app/build/qa/agents-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-agents"
source "$(dirname "$0")/env.sh"
qa_fresh
qa_write_config
qa_app_start --window-size 1180x820 || qa_end agents
# A few agent actions so the activity list has rows (the CLI of the app bundle, QA socket only).
"$CLI" --socket "$SOCK" --client claude-code status >/dev/null 2>&1
"$CLI" --socket "$SOCK" --client claude-code limits --down 5MB/s >/dev/null 2>&1
"$CLI" --socket "$SOCK" --client claude-code limits --down unlimited >/dev/null 2>&1
for mode in dark light; do
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "settings agents"
  qa_shot "agents-$mode" settings
done
qa_cmd "settings close"
qa_end agents
