#!/usr/bin/env bash
# Settings > Network (encryption and uTP now do something; no LSD / IPv6 toggles): dark and light
# into app/build/qa/settings-network-*.png. Own QA root (seedwell-qa-network), nothing is added.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-network"
source "$(dirname "$0")/env.sh"
qa_fresh
qa_write_config
qa_app_start --window-size 1180x820 || qa_end network
for mode in dark light; do
  qa_cmd "appearance $mode" "window open" "window size 1180x820" "settings network"
  qa_shot "settings-network-$mode" settings
done
qa_end network
