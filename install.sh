#!/usr/bin/env bash
# Seedwell installer: downloads a release from GitHub, checks its SHA-256, installs Seedwell.app.
#
#   curl -fsSL https://github.com/kkollsga/seedwell/releases/latest/download/install.sh | bash
#   … | bash -s -- --cli              also link the `seedwell` command (/usr/local/bin, else ~/.local/bin)
#   … | bash -s -- --version 0.1.0    a specific release instead of the latest
#   … | bash -s -- --no-launch        don't open the app afterwards
#   … | bash -s -- --uninstall        remove the app and the command link (asks before deleting your data)
#   install.sh --zip Seedwell-X.zip [--sha256 HEX]   install a zip you already have (a test build), checked
#                                     against HEX when given; same quit, swap and relaunch as an update
#   install.sh --rollback --state-dir DIR   put back the app kept in DIR/previous (see below)
#
# With a state folder (--state-dir, or SEEDWELL_INSTALL_STATE_DIR from the app's updater) an install keeps the app it
# replaces in DIR/previous (a zip, its own installer, its version), when DIR/launch-ok says that app started fine;
# then it removes launch-ok, reopens the new app and waits up to two minutes for it to write launch-ok with its
# version. If it does not, the new app is quit and the kept one put back and reopened. SEEDWELL_RELAUNCH_ARGS (one
# argument a line) are the arguments to reopen the app with; none: it opens as from the Dock.
#
# A release from GitHub must be signed with Seedwell's Developer ID (TEAM_ID below) and notarized, or nothing is
# installed: an update can never put an unsigned or someone else's app in place of Seedwell. (Releases before
# FIRST_SIGNED were signed ad hoc; one of those installs only when asked for by --version. A --zip test build and a
# release served by a local test server are not checked.)
#
# What it touches: /Applications/Seedwell.app (or ~/Applications if /Applications is not writable), the
# `seedwell` link if you ask for it, and a temporary folder. Downloaded files are never touched.
set -euo pipefail

REPO_URL="${SEEDWELL_REPO_URL:-https://github.com/kkollsga/seedwell}"
# The Apple Developer team every release is signed by (app/release-zip.sh and release.yml read it from here).
TEAM_ID=6GND8Z2C7Z
# The first release signed with that Developer ID; the ones before were signed ad hoc.
FIRST_SIGNED=0.1.11
APP_NAME="Seedwell"
DATA_DIR="$HOME/Library/Application Support/Seedwell"
EXE_NAME="SeedwellApp"
# SEEDWELL_INSTALL_DIR: install somewhere else (tests); default /Applications, else ~/Applications.
INSTALL_DIR="${SEEDWELL_INSTALL_DIR:-}"

version=""; cli=0; launch=1; uninstall=0; local_zip=""; want_sha=; rollback=0
STATE_DIR="${SEEDWELL_INSTALL_STATE_DIR:-}"
WATCHDOG_SECS="${SEEDWELL_WATCHDOG_SECS:-120}"
relaunch_args=()
if [ -n "${SEEDWELL_RELAUNCH_ARGS:-}" ]; then
  while IFS= read -r a; do relaunch_args+=("$a"); done <<< "$SEEDWELL_RELAUNCH_ARGS"
fi
while [ $# -gt 0 ]; do
  case "$1" in
    --state-dir) STATE_DIR="${2:?--state-dir needs a folder}"; shift ;;
    --rollback) rollback=1 ;;
    --version) version="${2:?--version needs a value}"; shift ;;
    --cli) cli=1 ;;
    --no-launch) launch=0 ;;
    --uninstall) uninstall=1 ;;
    --zip) local_zip="${2:?--zip needs a file}"; shift ;;
    --sha256) want_sha="${2:?--sha256 needs a value}"; shift ;;
    -h|--help) sed -n '2,21p' "$0" 2>/dev/null || true; exit 0 ;;
    *) echo "seedwell-install: unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

say() { printf '==> %s\n' "$*"; }
die() { printf 'seedwell-install: %s\n' "$*" >&2; exit 1; }
# version_lt A B: A is an older version than B (numbers compared part by part; a missing part is 0).
version_lt() {
  local IFS=. i a b
  read -r -a a <<< "$1"; read -r -a b <<< "$2"
  for i in 0 1 2 3; do
    [ "${a[i]:-0}" -lt "${b[i]:-0}" ] 2>/dev/null && return 0
    [ "${a[i]:-0}" -gt "${b[i]:-0}" ] 2>/dev/null && return 1
  done
  return 1
}

# Questions go to the terminal: stdin is the script itself when piped from curl.
ask() {
  local answer=""
  # No terminal (a script, CI): the answer is no.
  if (: < /dev/tty) 2>/dev/null; then printf '%s [y/N] ' "$1" > /dev/tty; read -r answer < /dev/tty || true; fi
  case "$answer" in y|Y|yes|YES) return 0 ;; *) return 1 ;; esac
}

installed_app() {
  for d in ${INSTALL_DIR:+"$INSTALL_DIR"} /Applications "$HOME/Applications"; do
    [ -n "$INSTALL_DIR" ] && [ "$d" != "$INSTALL_DIR" ] && continue
    [ -d "$d/$APP_NAME.app" ] && { echo "$d/$APP_NAME.app"; return; }
  done
}

# A proper quit: Seedwell handles SIGTERM like ⌘Q (engine shutdown, trackers told `stopped`).
quit_running() {
  local app="$1" pid
  [ -n "$app" ] || return 0
  # -a: include our ancestors. When the app itself runs this script (its updater), the app is our parent,
  # and pgrep leaves out ancestors by default.
  pid=$(pgrep -a -f "^$app/Contents/MacOS/$EXE_NAME" | head -1 || true)
  [ -n "$pid" ] || return 0
  say "Quitting the running $APP_NAME (trackers are told 'stopped', up to two minutes)"
  kill -TERM "$pid" 2>/dev/null || true
  for _ in $(seq 1 130); do kill -0 "$pid" 2>/dev/null || return 0; sleep 1; done
  die "$APP_NAME did not quit within 130 s; quit it from the menu bar and run this again"
}

# Only a link into the app being removed (never someone else's `seedwell`).
remove_cli_link() {
  local app="$1"
  [ -n "$app" ] || return 0
  for d in /usr/local/bin "$HOME/.local/bin"; do
    local l="$d/seedwell"
    if [ -L "$l" ] && [ "$(readlink "$l")" = "$app/Contents/MacOS/seedwell" ]; then rm -f "$l" && say "Removed $l"; fi
  done
}

# The version an app says it is: a development build's X.Y.Z.N, else X.Y.Z.
app_version() {
  /usr/libexec/PlistBuddy -c "Print :SeedwellDevVersion" "$1/Contents/Info.plist" 2>/dev/null \
    || /usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$1/Contents/Info.plist" 2>/dev/null || echo "?"
}

# Open the app: with the updater's arguments (an isolated run's own --state-dir among them: started directly, as
# LaunchServices would pass nothing of the environment), else as from the Dock.
open_app() {
  local app="$1"
  if [ ${#relaunch_args[@]} -eq 0 ]; then open "$app"
  elif printf '%s\n' "${relaunch_args[@]}" | grep -qx -- '--state-dir'; then
    nohup "$app/Contents/MacOS/$EXE_NAME" "${relaunch_args[@]}" >/dev/null 2>&1 &
  else open "$app" --args "${relaunch_args[@]}"; fi
}

# Keep the installed app in STATE_DIR/previous to go back to, if it started fine (launch-ok names its version) and
# carries the installer that can put it back.
keep_previous() {
  local app="$1" v ok
  [ -n "$STATE_DIR" ] && [ -d "$app" ] || return 0
  v=$(app_version "$app")
  ok=$(head -1 "$STATE_DIR/launch-ok" 2>/dev/null || true)
  if [ "$ok" != "$v" ]; then say "Not keeping $APP_NAME $v to go back to: it never started fine"; return 0; fi
  [ -f "$app/Contents/Resources/install.sh" ] || { say "Not keeping $APP_NAME $v: it has no installer to go back with"; return 0; }
  local d="$STATE_DIR/previous" t="$STATE_DIR/previous.new"
  rm -rf "$t"; mkdir -p "$t"
  ditto -c -k --sequesterRsrc --keepParent "$app" "$t/Seedwell.zip" || { rm -rf "$t"; say "Could not keep $APP_NAME $v"; return 0; }
  cp "$app/Contents/Resources/install.sh" "$t/install.sh"
  printf '{"version":"%s"}\n' "$v" > "$t/previous.json"
  rm -rf "$d"; mv "$t" "$d"
  say "Kept $APP_NAME $v to go back to"
}

# Put the kept app in place of `dest` (quitting whatever runs there) and open it.
restore_previous() {
  local dest="$1" d="$STATE_DIR/previous"
  [ -f "$d/Seedwell.zip" ] || die "no app kept in $d"
  local t; t=$(mktemp -d)
  ditto -x -k "$d/Seedwell.zip" "$t" || die "the kept app could not be unpacked"
  [ -d "$t/$APP_NAME.app" ] || die "the kept zip has no $APP_NAME.app"
  quit_running "$dest"
  rm -rf "$dest"; ditto "$t/$APP_NAME.app" "$dest"; rm -rf "$t"
  xattr -dr com.apple.quarantine "$dest" 2>/dev/null || true
  say "Put back $APP_NAME $(app_version "$dest")"
  open_app "$dest"
}

# Wait for the new app to say it started fine; if it does not, go back to the kept app.
watchdog() {
  local dest="$1" want; want=$(app_version "$dest")
  say "Waiting up to ${WATCHDOG_SECS} s for $APP_NAME $want to start"
  for _ in $(seq 1 "$WATCHDOG_SECS"); do
    [ "$(head -1 "$STATE_DIR/launch-ok" 2>/dev/null || true)" = "$want" ] && { say "$APP_NAME $want started fine"; return 0; }
    sleep 1
  done
  say "$APP_NAME $want did not start within ${WATCHDOG_SECS} s"
  if [ -f "$STATE_DIR/previous/Seedwell.zip" ]; then
    restore_previous "$dest"
    die "$APP_NAME $want did not start; the previous app was put back"
  fi
  die "$APP_NAME $want did not start, and no earlier app was kept to go back to"
}

if [ "$rollback" = 1 ]; then
  [ -n "$STATE_DIR" ] || die "--rollback needs --state-dir"
  app=$(installed_app || true)
  [ -n "$app" ] || app="${INSTALL_DIR:-/Applications}/$APP_NAME.app"
  restore_previous "$app"
  say "Done."
  exit 0
fi

if [ "$uninstall" = 1 ]; then
  app=$(installed_app || true)
  quit_running "$app"
  remove_cli_link "$app"
  if [ -n "$app" ]; then rm -rf "$app" && say "Removed $app"; else say "$APP_NAME.app is not installed"; fi
  if [ -d "$DATA_DIR" ]; then
    if ask "Also delete your torrent list, resume data and settings ($DATA_DIR)?"; then
      rm -rf "$DATA_DIR" && say "Removed $DATA_DIR"
    else
      say "Kept $DATA_DIR"
    fi
  fi
  say "Done. Your downloaded files were not touched."
  exit 0
fi

# Requirements: Apple silicon, macOS 15 or later.
[ "$(uname -s)" = "Darwin" ] || die "Seedwell runs on macOS only"
[ "$(uname -m)" = "arm64" ] || die "Seedwell needs a Mac with Apple silicon"
major=$(sw_vers -productVersion | cut -d. -f1)
[ "$major" -ge 15 ] || die "Seedwell needs macOS 15 or later (this Mac has $(sw_vers -productVersion))"

if [ -n "$version" ]; then base="$REPO_URL/releases/download/v${version#v}"; else base="$REPO_URL/releases/latest/download"; fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ -n "$local_zip" ]; then
  [ -f "$local_zip" ] || die "no such file: $local_zip"
  zip=$(basename "$local_zip")
  cp "$local_zip" "$tmp/$zip"
  have=$(shasum -a 256 "$tmp/$zip" | cut -d' ' -f1)
  if [ -n "$want_sha" ]; then
    [ "$have" = "$(printf '%s' "$want_sha" | tr 'A-F' 'a-f')" ] || die "checksum mismatch: $zip is $have, not $want_sha; nothing was installed"
    say "Checked the SHA-256 checksum of $zip"
  else
    say "Installing $zip (SHA-256 $have, not checked: no --sha256 given)"
  fi
else
  say "Downloading $APP_NAME${version:+ $version}"
  curl -fsSL --retry 3 -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "could not download $base/SHA256SUMS"
  # The Mac's zip (Seedwell-X.Y.Z.zip); the release also lists the Windows one (Seedwell-X.Y.Z-windows-x64.zip).
  zip=$(awk '$2 ~ /^Seedwell-[0-9]+\.[0-9]+\.[0-9]+\.zip$/ {print $2; exit}' "$tmp/SHA256SUMS")
  [ -n "$zip" ] || die "SHA256SUMS lists no Seedwell zip"
  curl -fSL --retry 3 --progress-bar -o "$tmp/$zip" "$base/$zip" || die "could not download $base/$zip"

  say "Checking the SHA-256 checksum"
  (cd "$tmp" && grep " $zip\$" SHA256SUMS | shasum -a 256 -c - >/dev/null) || die "checksum mismatch: the download is damaged or not the published file; nothing was installed"
fi

ditto -x -k "$tmp/$zip" "$tmp/unpacked"
new="$tmp/unpacked/$APP_NAME.app"
[ -d "$new" ] || die "the zip does not contain $APP_NAME.app"
codesign --verify --deep --strict "$new" 2>/dev/null || die "the app's code signature does not verify; nothing was installed"

# A release from GitHub is Seedwell's own, signed and notarized (a --zip test build is the caller's to vouch for).
if [ -z "$local_zip" ]; then
  new_version=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$new/Contents/Info.plist" 2>/dev/null || echo "")
  team=$(codesign -dv --verbose=2 "$new" 2>&1 | sed -n 's/^TeamIdentifier=//p' | grep -v '^not set$' || true)
  case "$REPO_URL" in
    http://127.0.0.1:* | http://localhost:*)
      say "A release from a local test server ($REPO_URL): its signature is not checked against team $TEAM_ID" ;;
    *)
      if [ "$team" = "$TEAM_ID" ]; then
        spctl --assess --type execute "$new" 2>/dev/null \
          || die "macOS's Gatekeeper does not accept this $APP_NAME (not notarized?); nothing was installed"
        say "Checked the Developer ID signature (team $TEAM_ID) and notarization"
      elif [ -n "$version" ] && [ "$new_version" = "${version#v}" ] && version_lt "$new_version" "$FIRST_SIGNED"; then
        say "$APP_NAME $new_version is from before releases were signed with a Developer ID: installing it as it is"
      else
        die "this $APP_NAME is not signed by its developer (team $TEAM_ID${team:+; it is signed by team $team}); nothing was installed"
      fi ;;
  esac
fi

dest_dir="${INSTALL_DIR:-/Applications}"
[ -n "$INSTALL_DIR" ] || [ -w "$dest_dir" ] || dest_dir="$HOME/Applications"
mkdir -p "$dest_dir"
dest="$dest_dir/$APP_NAME.app"

# A --zip build over a Seedwell signed with a Developer ID must be signed by the same team. To macOS another signature
# (an ad-hoc test build) is another app: the permissions given to Seedwell (notifications, the login item, folders it
# was allowed into) would be asked for again, and the next signed release would be a stranger to them once more. Such
# a build starts fine, so the watchdog would not catch it. (Folder Sync and chat keys are in the state folder for every
# build so far; the data protection Keychain needs an entitlement these builds do not have.)
if [ -n "$local_zip" ] && [ -d "$dest" ]; then
  team_of() { codesign -dv --verbose=2 "$1" 2>&1 | sed -n 's/^TeamIdentifier=//p' | grep -v '^not set$' || true; }
  have_team=$(team_of "$dest"); new_team=$(team_of "$new")
  if [ -n "$have_team" ] && [ "$new_team" != "$have_team" ]; then
    die "this build is ${new_team:+signed by team $new_team}${new_team:-signed ad hoc}, the installed $APP_NAME by team $have_team: to macOS it would be another app (its permissions asked for again). Sign it with that team (SEEDWELL_SIGN_IDENTITY); nothing was installed"
  fi
fi

quit_running "$dest"
if [ -d "$dest" ]; then
  keep_previous "$dest"
  say "Replacing $APP_NAME $(app_version "$dest")"
  rm -rf "$dest"
fi
ditto "$new" "$dest"
# Fetched with curl, so it carries no quarantine mark; clear one anyway in case this script itself, or a --zip
# file, was downloaded in a browser or synced by a cloud drive and carries the mark.
xattr -dr com.apple.quarantine "$dest" 2>/dev/null || true
say "Installed $APP_NAME $(app_version "$dest") in $dest_dir"

if [ "$cli" = 1 ]; then
  target="$dest/Contents/MacOS/seedwell"
  if [ -w /usr/local/bin ]; then link_dir=/usr/local/bin; else link_dir="$HOME/.local/bin"; mkdir -p "$link_dir"; fi
  ln -sf "$target" "$link_dir/seedwell"
  say "Linked $link_dir/seedwell"
  case ":$PATH:" in *":$link_dir:"*) ;; *) say "Add $link_dir to your PATH to use \`seedwell\` (e.g. in ~/.zshrc: export PATH=\"$link_dir:\$PATH\")" ;; esac
fi

if [ "$launch" = 1 ]; then
  [ -n "$STATE_DIR" ] && rm -f "$STATE_DIR/launch-ok"
  open_app "$dest"
  [ -n "$STATE_DIR" ] && watchdog "$dest"
  say "Done. Seedwell is in your menu bar."
else say "Done."; fi
