#!/usr/bin/env bash
# First-launch backlog offer (SPEC 7A.1): .torrent files that were already in ~/Downloads (the isolated
# home) are not mass-added; the app offers to add them once (an alert, drawn by `qa_shot … alert`). Own QA root.
set -uo pipefail
export QA_ROOT=/private/tmp/claude-501/seedwell-qa-backlog
source "$(dirname "$0")/env.sh"
qa_fresh
mkdir -p "$HOMEROOT/Downloads" "$STATE"
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/gen" --count 4 --trackers "public=" >/dev/null
cp "$QA_ROOT/gen/torrents"/*.torrent "$HOMEROOT/Downloads/"
# a first launch, as the defaults have it (they watch ~/Downloads), but nothing goes to the Trash: an isolated run's
# engine would use the user's real Trash (its trash_dir is the system's), so the files stay ("leave"). Agent access
# is on for the CLI below.
cat > "$STATE/config.toml" <<TOML
version = 1
[agents]
permission = "full"
[folders]
torrent_after_add = "leave"
[[folders.watch]]
path = "~/Downloads"
mode = "auto"
after_add = "leave"
TOML
qa_app_start || qa_end backlog
check "the offer is up" "$(qa_wait "windows ~ alert" 10)" "alert"
qa_shot "backlog-offer-dark" alert
qa_cmd "alert 1"
qa_until 20 eval '[ "$(qa_count)" -ge 4 ]' || true
echo "torrents in the app:"; "$CLI" --socket "$SOCK" list | head -8
check "all four were added" "$(qa_count)" "4"
check "they stay in Downloads" "$(ls "$HOMEROOT/Downloads" | grep -c torrent)" "4"
check "none went to the Trash" "$(ls "$HOMEROOT/.Trash" 2>/dev/null | grep -c torrent)" "0"
qa_end backlog
