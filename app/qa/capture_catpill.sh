#!/usr/bin/env bash
# The category pill of the add popover (SPEC 8.6, 7A.2): the pill in the corner, its menu, and a change TV -> Other
# (the subtitle's destination, the checklist). Own QA root (seedwell-qa-catpill). Shots: app/build/qa/catpill-*.png.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-catpill"
source "$(dirname "$0")/env.sh"
BIN="$QA_ROOT/fake-trash"; DL="$HOMEROOT/Downloads"
state() { qa_ask "starting state" | tail -1; }
shot_popover() { qa_shot "catpill-$1" popover; }
qa_fresh
mkdir -p "$DL" "$BIN"
QA_WATCH="~/Downloads/watch" QA_START_DELAY=120 qa_write_config
sed -i '' 's/^ask_before_adding = .*/ask_before_adding = "multi_file"/' "$STATE/config.toml"
cat >> "$STATE/config.toml" <<TOML
[folders.categories.other]
finished_dir = "~/Downloads/OtherStuff"
TOML
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/lib" --special season --start 1 >/dev/null
cp "$QA_ROOT"/lib/torrents/*Night.Harbor*.torrent "$DL/season.torrent"
QA_APP_ENV="SEEDWELL_QA_TRASH_DIR=$BIN" qa_app_start --window-size 1180x820 --show-window || qa_end catpill
qa_cmd "appearance dark" "open-file $DL/season.torrent"
qa_wait "starting state ~ items 1" 10
shot_popover "compact"
qa_cmd "starting fold"
shot_popover "expanded-tv"
echo "== change TV -> Other"
qa_cmd "starting category Other"; check "the category follows" "$(qa_wait "starting state ~ | Other" 10)" "| Other"
shot_popover "expanded-other"
qa_cmd "starting fold"
shot_popover "compact-other"
qa_cmd "starting category TV"; qa_wait "starting state ~ | TV" 10 >/dev/null; qa_cmd "starting fold"
echo "== the dropdown: the pill's menu under the pill"
out=$(qa_ask "starting pill" | grep 'pill menu' | tail -1); echo "  $out"
check "every category, TV ticked" "$out" '"*TV"'
qa_shot "catpill-menu" menu
qa_cmd "starting pill close" "starting cancel keep"
qa_end catpill
