#!/usr/bin/env bash
# check: the fast local check. Builds and tests only what a change can affect, in one heavy job.
#
#   tools/check.sh                  changes in the working folder (else the last commit)
#   tools/check.sh --base public    everything since that revision
#   tools/check.sh -p trs-tracker   these crates (and the crates that depend on them)
#   tools/check.sh --full           the whole workspace, app and Swift tests (what GitHub CI runs)
#   tools/check.sh -- -E 'test(/accuracy/)'   extra arguments for cargo nextest
#
# Affected = the crates whose files changed plus every workspace crate that depends on them. Clippy
# and the tests (cargo nextest, every test binary in parallel) run for those only; the app build and
# the Swift tests only when app/ or trs-ffi is affected. A change to Cargo.toml, Cargo.lock, the
# toolchain or the nextest config means everything. Documentation-only changes run nothing.
set -euo pipefail
cd "$(dirname "$0")/.."
export TRS_FORBID_REAL_HOME=1

orig=("$@")
base="" full=0 app="auto" pkgs=() extra=()
while [ $# -gt 0 ]; do
  case "$1" in
    --base) base="$2"; shift 2 ;;
    --full) full=1; shift ;;
    --app) app=1; shift ;;
    --no-app) app=0; shift ;;
    -p) pkgs+=("$2"); shift 2 ;;
    --) shift; extra=("$@"); break ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "check: unknown option $1" >&2; exit 2 ;;
  esac
done

if [ $full -eq 0 ] && [ ${#pkgs[@]} -eq 0 ]; then
  if [ -n "$base" ]; then
    files=$(git diff --name-only "$base"; git ls-files --others --exclude-standard)
  else
    files=$(git diff --name-only HEAD; git ls-files --others --exclude-standard)
    [ -n "$files" ] || files=$(git diff --name-only HEAD~1 HEAD)
  fi
  [ -n "$files" ] || { echo "check: nothing changed"; exit 0; }
  if echo "$files" | grep -qE '^(Cargo\.(toml|lock)|rust-toolchain\.toml|\.config/|\.cargo/)'; then full=1; fi
fi

# Workspace crates, their folders and reverse dependencies, from cargo metadata (no build).
plan=$(cargo metadata --format-version 1 --no-deps --locked | python3 -I -c '
import json, os, sys
meta = json.load(sys.stdin)
root = meta["workspace_root"]
crates = {p["name"]: (os.path.relpath(os.path.dirname(p["manifest_path"]), root),
                      {d["name"] for d in p["dependencies"]}) for p in meta["packages"]}
full, wanted, files = sys.argv[1] == "1", set(sys.argv[2].split()), sys.argv[3].split("\n")
app = False
if not full and not wanted:
    for f in files:
        if f.startswith("app/"): app = True
        for name, (d, _) in crates.items():
            if f.startswith(d + "/") and not f.endswith(".md"): wanted.add(name)
if full: wanted = set(crates)
grow = True
while grow:
    grow = False
    for name, (_, deps) in crates.items():
        if name not in wanted and deps & wanted: wanted.add(name); grow = True
app = app or full or "trs-ffi" in wanted
print(" ".join(sorted(wanted)) + "|" + ("1" if app else "0"))
' "$full" "${pkgs[*]:-}" "${files:-}")
affected=${plan%|*}
[ "$app" = auto ] && app=${plan#*|}

if [ -z "$affected" ] && [ "$app" = 0 ]; then echo "check: only documentation changed"; exit 0; fi
echo "check: crates: ${affected:-none}; app: $([ "$app" = 1 ] && echo yes || echo no)"

args=()
if [ $full -eq 1 ]; then args=(--workspace); else for p in $affected; do args+=(-p "$p"); done; fi

# One heavy job for every step: this script runs itself again under heavy.sh.
if [ -z "${CHECK_INNER:-}" ]; then
  start=$(date +%s)
  CHECK_INNER=1 CHECK_APP=$app tools/sysguard/heavy.sh "$0" ${orig[@]+"${orig[@]}"}
  echo "check: ok in $(( $(date +%s) - start )) s"
  exit 0
fi
if [ ${#args[@]} -gt 0 ]; then
  cargo clippy "${args[@]}" --all-targets --locked -q -- -D warnings
  cargo nextest run "${args[@]}" --locked --status-level fail --final-status-level slow ${extra[@]+"${extra[@]}"}
fi
if [ "$CHECK_APP" = 1 ]; then
  SEEDWELL_PROFILE=dev bash app/build.sh >/dev/null
  SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk swift test --package-path app -q \
    -Xswiftc -plugin-path -Xswiftc /Library/Developer/CommandLineTools/usr/lib/swift/host/plugins/testing
fi
# CI builds the app with Xcode's Swift, which is stricter than the Command Line Tools' in places (it refused a
# closure that used a name declared after it). Compile the app with it too when Xcode is installed and its licence
# accepted; its own build folder, so the builds above are not disturbed. DEVELOPER_DIR picks Xcode for this command
# only (never `xcode-select -s`, which would change the whole Mac).
if [ "$CHECK_APP" = 1 ]; then
  XC=/Applications/Xcode.app/Contents/Developer
  if [ -d "$XC" ] && DEVELOPER_DIR="$XC" xcrun swift --version >/dev/null 2>&1; then
    DEVELOPER_DIR="$XC" swift build --package-path app --scratch-path app/.build-xcode -q
    echo "check: the app also builds with $(DEVELOPER_DIR="$XC" xcrun swift --version 2>&1 | head -1 | sed 's/.*\(Swift version [0-9.]*\).*/\1/') (Xcode, as CI)"
  elif [ -d "$XC" ]; then
    echo "check: skipped the Xcode build (accept Xcode's licence: sudo $XC/usr/bin/xcodebuild -license accept)"
  fi
fi
