#!/usr/bin/env bash
# A big folder added as the user added theirs, measured for "every part of the app stays interactive": about 400
# films of ~2.25 GB (sparse: they take no disk space) and a few thousand small real files in nested folders, added
# through the Add Folder window From this computer, On all my devices, with Files on demand for the other devices.
#
# A second device: a second QA run (own root, quiet like this one) paired with this one through Settings › Devices'
# calls (`devices pair|join|confirm`), so On all my devices brings the folder to it. BIGFOLDER_PEER=0 runs alone.
#
# Baseline (no add), then the add: both phases start the latency probes (`ui latency`: main-thread beats every 10 ms,
# the core's reads every 50 ms), cycle through the menu and its Folder Sync submenu, the window's sections, every
# Settings tab, a chat channel, Get Info, `ui ping` (and in the add, the new folder's page, Edit and Share windows), and
# make CLI round trips through the control socket (and `sync list` on the second device, which joins the folder
# meanwhile); bigfolder.py times every command and prints baseline against add.
# Verdict during the add (bigfolder.py; never loosened to pass): no beat later than 100 ms outside QA's own shots,
# every surface command within 500 ms, every CLI round trip within 1 s, the core's reads under 500 ms, the folder
# listed by `sync list` and in the menu. Shots: app/build/qa/bigfolder-*.png. Own QA roots (seedwell-qa-bigfolder,
# seedwell-qa-bigfolder-peer); only their processes are signalled. BIGFOLDER_RUNS=<file> appends each run's numbers.
set -uo pipefail
export QA_ROOT="/private/tmp/claude-501/seedwell-qa-bigfolder"
PEER_ROOT="/private/tmp/claude-501/seedwell-qa-bigfolder-peer"
source "$(dirname "$0")/env.sh"
HERE="$REPO/app/qa"
PEER="${BIGFOLDER_PEER:-1}"
BASE_SECS="${BIGFOLDER_BASE_SECS:-10}"
ADD_SECS="${BIGFOLDER_ADD_SECS:-15}"
FOLDER_NAME="Movies"
cli() { "$CLI" --socket "$SOCK" --client claude-code "$@"; }
# The peer's own QA helpers: env.sh read again with its root, in a subshell.
peer() { (export QA_ROOT="$PEER_ROOT"; source "$HERE/env.sh"; "$@"); }
peer_ask() { peer qa_ask "$@"; }
# Both runs end, and both roots go (the sparse folder too: its real bytes are few, but it is 900 GB to any tool that
# adds up sizes).
end() {
  [ "$PEER" = 1 ] && peer qa_teardown >/dev/null 2>&1
  rm -rf "$PEER_ROOT"
  qa_teardown
  rm -rf "$QA_ROOT"
  if [ "$FAIL" = 0 ]; then echo "bigfolder: PASS"; exit 0; else echo "bigfolder: FAIL"; exit 1; fi
}

qa_fresh
[ "$PEER" = 1 ] && peer qa_fresh

# The folder. Sparse files need a file system that keeps holes (APFS does): checked on the first one, and the real
# bytes are kept small (a few MB), so only a little free space is needed.
free_kb=$(df -k "$QA_ROOT" | awk 'NR==2 {print $4}')
[ "${free_kb:-0}" -gt $((5 * 1024 * 1024)) ] || { echo "  FAIL: less than 5 GB free under $QA_ROOT"; FAIL=1; end; }
MOVIES="$HOMEROOT/$FOLDER_NAME"
python3 -I - "$MOVIES" <<'PY' || { echo "  FAIL: could not make the folder"; FAIL=1; end; }
import os, random, sys
root = sys.argv[1]
rnd = random.Random(7)
words = "Night Harbor Glass River Paper Moon Silent Valley Iron Garden Last Summer Red Forest Cold Light Blue Station".split()
total = 0
for i in range(400):
    title = f"{rnd.choice(words)} {rnd.choice(words)} {i:03d} ({1960 + i % 60})"
    d = os.path.join(root, title)
    os.makedirs(d, exist_ok=True)
    size = 2_250_000_000 + rnd.randrange(0, 50_000_000)
    with open(os.path.join(d, title + ".mkv"), "wb") as f:
        f.truncate(size)  # sparse: no blocks
    total += size
    with open(os.path.join(d, title + ".srt"), "wb") as f:
        f.write(os.urandom(rnd.randrange(8_000, 24_000)))
    with open(os.path.join(d, "movie.nfo"), "wb") as f:
        f.write(os.urandom(rnd.randrange(500, 2_500)))
    if i == 0:
        st = os.stat(os.path.join(d, title + ".mkv"))
        if st.st_blocks * 512 > 1 << 20:
            sys.exit("sparse files are not sparse here")
# A few thousand small real files in nested folders (extras, artwork, notes).
n = 0
for a in range(12):
    for b in range(10):
        d = os.path.join(root, "Extras", f"Collection {a:02d}", f"Disc {b}", "Artwork")
        os.makedirs(d, exist_ok=True)
        for c in range(20):
            with open(os.path.join(d, f"still-{c:03d}.jpg"), "wb") as f:
                f.write(os.urandom(rnd.randrange(1_000, 6_000)))
            n += 1
print(f"{400 * 3 + n} files, {total / 1e9:.0f} GB apparent")
PY
echo "== the folder: $(du -sh "$MOVIES" | cut -f1) on disk, $(find "$MOVIES" -type f | wc -l | tr -d ' ') files"

# This Mac's app: one finished torrent (Get Info) and a chat channel.
qa_write_config
python3 -I "$REPO/app/qa/make_torrents.py" --out "$QA_ROOT/seed" --count 1 --start 2000 >/dev/null
qa_app_start --window-size 1180x820 || end
qa_add_all "$QA_ROOT/seed/torrents" "$QA_ROOT/seed/data"
qa_until 30 eval '[ "$(qa_count finished)" -ge 1 ]' || echo "  (the torrent is not sharing yet)"
TORRENT=$(cli list 2>/dev/null | tail -n +2 | awk 'NR==1 {print $NF}')
cli chat new "QA Big Folder" >/dev/null 2>&1 || { echo "  FAIL: chat new"; FAIL=1; }
qa_until 20 cli sync list || { echo "  FAIL: folder sync did not start"; FAIL=1; end; }

# The second device, paired through Settings › Devices.
if [ "$PEER" = 1 ]; then
  peer qa_write_config
  peer qa_app_start || { echo "  FAIL: the second device did not start"; FAIL=1; end; }
  peer qa_until 20 bash -c '"$0" --socket "$1" sync list' "$CLI" "$PEER_ROOT/state/control.sock"
  qa_cmd "devices pair"
  link=$(qa_wait "devices state !~ link -" 20 | sed -n 's/.* link //p')
  case "$link" in seedwell://*) echo "== pair link made";; *) echo "  FAIL: no pair link ($link)"; FAIL=1; end;; esac
  peer qa_cmd "devices join $link"
  check "the first device is asked" "$(qa_wait "devices state ~ requests 1" 30)" "requests 1"
  qa_cmd "devices confirm"
  check "paired here" "$(qa_wait "devices state ~ devices 2" 30)" "devices 2"
  check "paired there" "$(peer qa_wait "devices state ~ devices 2" 30)" "devices 2"
fi

CH="QA Big Folder"
PHASE=(python3 -I "$HERE/bigfolder.py" phase --ctl "$CTL" --cli "$CLI" --sock "$SOCK" --channel "$CH" ${TORRENT:+--torrent "$TORRENT"})
# The second device's `sync list` too: it joins the folder while this one adds it.
[ "$PEER" = 1 ] && PHASE+=(--peer-sock "$PEER_ROOT/state/control.sock")

# Baseline: every surface, no add.
qa_cmd "appearance dark" "window open" "window size 1180x820"
"${PHASE[@]}" --name baseline --secs "$BASE_SECS" --out "$QA_ROOT/baseline.json" || { echo "  FAIL: the baseline phase"; FAIL=1; end; }

# The Add Folder window, set as the user set it.
echo "== Add Folder: $(qa_ask "sync-add open" | tail -1)"
qa_ask "sync-add choose $MOVIES" | tail -1
# Start is pressed only when the window is set as the user set it: with Files on demand off, the second device would
# download the folder in full (900 GB of zeros onto this disk).
need() { case "$2" in *"$3"*) echo "  ok: $1";; *) echo "  FAIL: $1: wanted '$3' in: $2 (Start not pressed)"; FAIL=1; end;; esac; }
need "On all my devices" "$(qa_ask "sync-add devices all" | tail -1)" "devices all (shown)"
need "Files on demand for the others" "$(qa_ask "sync-add ondemand on" | tail -1)" "ondemand on (shown)"
need "set as the user set it, Start on" "$(qa_ask "sync-add state" | tail -1)" "devices all (shown) ondemand on (shown) start on"
qa_shot bigfolder-add-window "window sync-add"
"${PHASE[@]}" --name add --secs "$ADD_SECS" --out "$QA_ROOT/add.json" --add --folder "$FOLDER_NAME" --shots "$OUT" \
  || { echo "  FAIL: the add phase"; FAIL=1; end; }

echo "== baseline against add"
python3 -I "$HERE/bigfolder.py" verdict "$QA_ROOT/baseline.json" "$QA_ROOT/add.json" \
  ${BIGFOLDER_RUNS:+--runs-file "$BIGFOLDER_RUNS"} --label "${BIGFOLDER_LABEL:-run}" || FAIL=1
check "the folder in sync list" "$(cli sync list 2>&1)" "$FOLDER_NAME"
qa_cmd "menu open"
check "the folder in the menu" "$(qa_wait "menu-sync ~ $FOLDER_NAME" 10)" "$FOLDER_NAME"
qa_cmd "menu close"
check "Add Folder closed after Start" "$(qa_ask "sync-add state" | tail -1)" "sync-add: closed"
echo "== this Mac: $(cli sync status "$FOLDER_NAME" 2>&1 | head -3 | tr '\n' ' ')"
if [ "$PEER" = 1 ]; then
  # Not part of the verdict: the other device gets the folder by itself (listed, nothing downloaded).
  if peer qa_until 60 bash -c '"$0" --socket "$1" sync list | grep -q Movies' "$CLI" "$PEER_ROOT/state/control.sock"; then
    echo "== the second device: $("$CLI" --socket "$PEER_ROOT/state/control.sock" sync status Movies 2>&1 | head -3 | tr '\n' ' ')"
  else
    echo "  note: the second device has not listed the folder within 60 s"
  fi
fi
echo "== disk: this root $(du -sh "$QA_ROOT" | cut -f1), peer $(du -sh "$PEER_ROOT" 2>/dev/null | cut -f1)"
end
