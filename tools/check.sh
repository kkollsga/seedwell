#!/usr/bin/env bash
# check: the fast local check. Builds and tests only what a change can affect, in one heavy job.
#
#   tools/check.sh                  changes in the working folder (else the last commit)
#   tools/check.sh --base public    everything since that revision
#   tools/check.sh -p seedwell-tracker   these crates (and the crates that depend on them)
#   tools/check.sh --full           the whole workspace, app and Swift tests (what GitHub CI runs)
#   tools/check.sh --quick          a worker's check: the tests of the changed crates only, clippy for them and
#                                   their dependents (no tests there), the app only when app/ or seedwell-ffi changed,
#                                   and a Windows compile check (AGENTS.md, *Catching bugs early*)
#   tools/check.sh -- -E 'test(/accuracy/)'   extra arguments for cargo nextest
#
# Affected = the crates whose files changed plus every workspace crate that depends on them. Clippy
# and the tests (cargo nextest, every test binary in parallel) run for those only; the app build and
# the Swift tests only when app/ or seedwell-ffi is affected (the rendezvous Worker's node tests with seedwell-rendezvous, when
# Node is installed). A change to Cargo.toml, Cargo.lock, the
# toolchain or the nextest config means everything. Documentation-only changes run nothing.
# Two steps catch before CI what only CI would find: tools/test-portability.py reads the affected crates' tests for
# Mac assumptions (Windows CI), and on macOS the timing tests (tools/timing-tests.txt) run a second time at utility
# priority, where the system delays timers as on CI's Mac runner.
set -euo pipefail
cd "$(dirname "$0")/.."
export SEEDWELL_FORBID_REAL_HOME=1

orig=("$@")
base="" full=0 quick=0 app="auto" pkgs=() extra=()
while [ $# -gt 0 ]; do
  case "$1" in
    --base) base="$2"; shift 2 ;;
    --full) full=1; shift ;;
    --quick) quick=1; shift ;;
    --app) app=1; shift ;;
    --no-app) app=0; shift ;;
    -p) pkgs+=("$2"); shift 2 ;;
    --) shift; extra=("$@"); break ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
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
full, wanted, files, quick = sys.argv[1] == "1", set(sys.argv[2].split()), sys.argv[3].split("\n"), sys.argv[4] == "1"
app, scan_all = False, False
if not full and not wanted:
    for f in files:
        if f.startswith("app/"): app = True
        if f == "tools/test-portability.py": scan_all = True
        # The rendezvous Worker is embedded in seedwell-rendezvous (include_str!).
        if f.startswith("tools/rendezvous-worker/") and not f.endswith(".md"): wanted.add("seedwell-rendezvous")
        for name, (d, _) in crates.items():
            if f.startswith(d + "/") and not f.endswith(".md"): wanted.add(name)
if full: wanted = set(crates)
direct = set(wanted)
grow = True
while grow:
    grow = False
    for name, (_, deps) in crates.items():
        if name not in wanted and deps & wanted: wanted.add(name); grow = True
app = app or full or "seedwell-ffi" in (direct if quick else wanted)
# The folders tools/test-portability.py reads: those of the affected crates (all when the scan itself changed).
scan = sorted(d for d, _ in crates.values() if d.startswith("crates/")) if scan_all else sorted(crates[n][0] for n in wanted)
print(" ".join(sorted(wanted)) + "|" + ("1" if app else "0") + "|" + " ".join(sorted(direct)) + "|" + " ".join(scan))
' "$full" "${pkgs[*]:-}" "${files:-}" "$quick")
affected=${plan%%|*}
scan_dirs=${plan##*|}
direct=$(echo "$plan" | cut -d'|' -f3)
if [ "$app" = auto ]; then app=${plan#*|}; app=${app%%|*}; fi

if [ -z "$affected" ] && [ "$app" = 0 ] && [ -z "$scan_dirs" ]; then echo "check: only documentation changed"; exit 0; fi
echo "check: crates: ${affected:-none}; app: $([ "$app" = 1 ] && echo yes || echo no)$([ $quick -eq 1 ] && echo "; tests only for: ${direct:-none} (--quick)")"

# Tests that assume the Mac (a ~/ path, "This Mac", a /-joined path, a Unix-only helper without its cfg), which only
# Windows CI would find. Not heavy: before the heavy job, and only once.
if [ -z "${CHECK_INNER:-}" ] && [ -n "$scan_dirs" ]; then
  s0=$(perl -MTime::HiRes=time -e 'printf "%.1f", time')
  # shellcheck disable=SC2086
  python3 -I tools/test-portability.py $scan_dirs
  echo "check: no test assumes the Mac ($(perl -MTime::HiRes=time -e "printf '%.1f', time - $s0") s, tools/test-portability.py)"
fi
[ -n "$affected" ] || [ "$app" = 1 ] || exit 0

args=() targs=()
if [ $full -eq 1 ]; then args=(--workspace); else for p in $affected; do args+=(-p "$p"); done; fi
# --quick: tests (and the timing tests) only for the crates that changed; clippy still compiles their dependents.
if [ $quick -eq 1 ]; then for p in $direct; do targs+=(-p "$p"); done; else targs=(${args[@]+"${args[@]}"}); fi

# One heavy job for every step: this script runs itself again under heavy.sh.
if [ -z "${CHECK_INNER:-}" ]; then
  # Started under heavy.sh itself, its own heavy job would wait for the lock its parent holds, for ever, and every
  # other build on the machine behind it (2026-11-07): refuse instead.
  p=$PPID
  while [ -n "$p" ] && [ "$p" -gt 1 ]; do
    case "$(ps -o command= -p "$p" 2>/dev/null)" in
      *sysguard/heavy.sh*) echo "check: run tools/check.sh directly, not under heavy.sh: it takes the heavy lock itself" >&2; exit 2 ;;
    esac
    p=$(ps -o ppid= -p "$p" 2>/dev/null | tr -d ' ')
  done
  start=$(date +%s)
  CHECK_INNER=1 CHECK_APP=$app tools/sysguard/heavy.sh "$0" ${orig[@]+"${orig[@]}"}
  echo "check: ok in $(( $(date +%s) - start )) s"
  exit 0
fi
# Build artifacts pile up (every feature set, lock file and toolchain leaves its own): once a day, remove the
# ones no build has touched for 3 days, and the incremental caches likewise (cargo sweep leaves those). A huge
# target/debug/deps is not only disk: with 860 000 files there, a test binary started from it waited 21 s for
# FSEvents to accept a stream, and the file watcher tests timed out.
stamp=target/.check-swept
if [ -z "$(find "$stamp" -mtime -1 2>/dev/null)" ]; then
  if command -v cargo-sweep >/dev/null; then
    cargo sweep --time 3 . >/dev/null
    find target/*/incremental -mindepth 1 -maxdepth 1 -mtime +3 -exec rm -rf {} + 2>/dev/null || true
    # The tests' own temp root (CARGO_TARGET_TMPDIR): what a killed or panicked run left there (AGENTS.md, temp folders).
    find target/tmp -mindepth 1 -maxdepth 1 -mtime +0 -exec rm -rf {} + 2>/dev/null || true
    mkdir -p target && touch "$stamp"
  else
    echo "check: install cargo-sweep (cargo install cargo-sweep) so old build artifacts are removed as you go"
  fi
fi
# FFI names the Windows app's C# bindings cannot compile (Swift does not mind, so only Windows CI would find them).
if [[ " $affected " == *" seedwell-ffi "* ]]; then
  python3 -I tools/ffi-names.py
fi
if [ ${#args[@]} -gt 0 ]; then
  cargo clippy "${args[@]}" --all-targets --locked -q -- -D warnings
  if [ ${#targs[@]} -gt 0 ]; then
    cargo nextest run "${targs[@]}" --locked --status-level fail --final-status-level slow ${extra[@]+"${extra[@]}"}
  fi
  # A worker's check also compiles the changed crates for Windows: what only the PC or Windows CI would find.
  if [ $quick -eq 1 ] && rustup target list --installed 2>/dev/null | grep -qx x86_64-pc-windows-gnu; then
    cargo check "${args[@]}" --all-targets --locked -q --target x86_64-pc-windows-gnu
    echo "check: compiles for Windows (x86_64-pc-windows-gnu)"
  fi
fi
# The timing tests again at utility priority: macOS then delays the timers of the test threads as it does on GitHub's
# macOS runner (f20ed77), so a bound that holds only on a quiet Mac fails here, not on CI. Which tests:
# tools/timing-tests.txt (docs/ENGINEERING.md §5). Skipped when extra nextest arguments narrow the run.
if [ ${#targs[@]} -gt 0 ] && [ ${#extra[@]} -eq 0 ] && [ "$(uname)" = Darwin ]; then
  timing=$(python3 -I -c 'import sys; print(" | ".join("(%s)" % l.strip() for l in open(sys.argv[1]) if l.strip() and not l.lstrip().startswith("#")))' tools/timing-tests.txt)
  t0=$(date +%s)
  taskpolicy -c utility cargo nextest run "${targs[@]}" --locked --no-tests pass --status-level fail \
    --final-status-level slow -E "$timing"
  echo "check: the timing tests pass at utility priority, as on CI's Mac ($(( $(date +%s) - t0 )) s)"
fi
# The rendezvous Worker's own tests (worker.js against a local fake of the Workers runtime), when Node is here.
if [[ " $affected " == *" seedwell-rendezvous "* ]] && [ ${#extra[@]} -eq 0 ]; then
  if command -v node >/dev/null; then
    (cd tools/rendezvous-worker && node --test --test-reporter=dot test.mjs)
  else
    echo "check: skipped the rendezvous Worker's tests (no node)"
  fi
fi
if [ "$CHECK_APP" = 1 ]; then
  SEEDWELL_PROFILE=dev bash app/build.sh >/dev/null
  # Every suite has a time limit for each of its tests: a test that hangs is named, instead of using up CI's 60 minutes.
  if grep -rn '@Suite' app/Tests --include='*.swift' | grep -v '^[^:]*:[0-9]*: *///' | grep -v 'timeLimit'; then
    echo "check: the suites above have no .timeLimit(.minutes(1))" >&2
    exit 1
  fi
  # One thread in Swift's cooperative pool (CI's Mac has three, shared by the tests running at once): a test that
  # blocks a pool thread while it waits for an actor hangs here, not only on CI (the 0.1.18 run that hung for 60 min).
  LIBDISPATCH_COOPERATIVE_POOL_STRICT=1 SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk swift test --package-path app -q \
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
