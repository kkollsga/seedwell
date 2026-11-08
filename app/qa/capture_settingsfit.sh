#!/usr/bin/env bash
# Settings fits the main window at its minimum size (820x600): every tab, Rules' Trackers side included, dark and light,
# with nothing laid out past the window's edges (`ui overflow`). A tab wider than the window pushed the whole window's
# content, sidebar and tab bar included, aside (Settings › Rules until the list went above the editor in a narrow
# window). Six rules, so Rules' list scrolls above the editor. Shots: settingsfit-<820|1180>-<tab>-<dark|light>.png.
# Own QA root (seedwell-qa-settingsfit).
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-settingsfit"
source "$(dirname "$0")/env.sh"
qa_fresh
qa_write_config
{
  printf 'version = 1\n\n[default]\ngoal = { kind = "ratio_or_time", ratio = 1.0, time = "7d" }\n'
  for t in alpha bravo charlie delta echo foxtrot; do
    printf '\n[[rule]]\nid = "%s"\nname = "%s: seed to 2.0"\nmatch = { tracker = "tracker-%s" }\ngoal = { kind = "ratio", ratio = 2.0 }\n' "$t" "$t" "$t"
  done
} > "$STATE/rules.toml"
qa_app_start --window-size 820x600 || qa_end settingsfit
qa_cmd "window open" "settings general"
pages() { for t in general transfers folders rules trackers network folderSync devices agents advanced; do echo "$t"; done; }
show() {
  case "$1" in
    rules) qa_cmd "settings-tab rules" "rules-section rules";;
    trackers) qa_cmd "settings-tab rules" "rules-section trackers";;
    *) qa_cmd "settings-tab $1";;
  esac
}
for look in dark light; do
  qa_cmd "appearance $look"
  for size in 820x600 1180x820; do
    qa_cmd "window size $size"
    for p in $(pages); do
      show "$p"
      out=$(qa_ask "ui overflow" | tail -1)
      check "$p at $size, $look: nothing past the window's edges" "$out" "overflow: none"
      # Every tab at the minimum size; Rules and Trackers at both.
      case "$size:$p" in 820x600:*|*:rules|*:trackers) qa_shot "settingsfit-${size%x*}-$p-$look" settings >/dev/null;; esac
    done
  done
done
qa_end settingsfit
