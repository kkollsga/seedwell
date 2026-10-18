#!/usr/bin/env bash
# Seedwell installer: downloads a release from GitHub, checks its SHA-256, installs Seedwell.app.
#
#   curl -fsSL https://github.com/kkollsga/seedwell/releases/latest/download/install.sh | bash
#   … | bash -s -- --cli              also link the `seedwell` command (/usr/local/bin, else ~/.local/bin)
#   … | bash -s -- --version 0.1.0    a specific release instead of the latest
#   … | bash -s -- --no-launch        don't open the app afterwards
#   … | bash -s -- --uninstall        remove the app and the command link (asks before deleting your data)
#
# What it touches: /Applications/Seedwell.app (or ~/Applications if /Applications is not writable), the
# `seedwell` link if you ask for it, and a temporary folder. Downloaded files are never touched.
set -euo pipefail

REPO_URL="${SEEDWELL_REPO_URL:-https://github.com/kkollsga/seedwell}"
APP_NAME="Seedwell"
DATA_DIR="$HOME/Library/Application Support/Seedwell"
EXE_NAME="SeedwellApp"
# SEEDWELL_INSTALL_DIR: install somewhere else (tests); default /Applications, else ~/Applications.
INSTALL_DIR="${SEEDWELL_INSTALL_DIR:-}"

version=""; cli=0; launch=1; uninstall=0
while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="${2:?--version needs a value}"; shift ;;
    --cli) cli=1 ;;
    --no-launch) launch=0 ;;
    --uninstall) uninstall=1 ;;
    -h|--help) sed -n '2,11p' "$0" 2>/dev/null || true; exit 0 ;;
    *) echo "seedwell-install: unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

say() { printf '==> %s\n' "$*"; }
die() { printf 'seedwell-install: %s\n' "$*" >&2; exit 1; }

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
  pid=$(pgrep -f "^$app/Contents/MacOS/$EXE_NAME" | head -1 || true)
  [ -n "$pid" ] || return 0
  say "Quitting the running $APP_NAME (trackers are told 'stopped')"
  kill -TERM "$pid" 2>/dev/null || true
  for _ in $(seq 1 30); do kill -0 "$pid" 2>/dev/null || return 0; sleep 1; done
  die "$APP_NAME did not quit within 30 s; quit it from the menu bar and run this again"
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
say "Downloading $APP_NAME${version:+ $version}"
curl -fsSL --retry 3 -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "could not download $base/SHA256SUMS"
zip=$(awk '$2 ~ /^Seedwell-.*\.zip$/ {print $2; exit}' "$tmp/SHA256SUMS")
[ -n "$zip" ] || die "SHA256SUMS lists no Seedwell zip"
curl -fSL --retry 3 --progress-bar -o "$tmp/$zip" "$base/$zip" || die "could not download $base/$zip"

say "Checking the SHA-256 checksum"
(cd "$tmp" && grep " $zip\$" SHA256SUMS | shasum -a 256 -c - >/dev/null) || die "checksum mismatch: the download is damaged or not the published file; nothing was installed"

ditto -x -k "$tmp/$zip" "$tmp/unpacked"
new="$tmp/unpacked/$APP_NAME.app"
[ -d "$new" ] || die "the zip does not contain $APP_NAME.app"
codesign --verify --deep --strict "$new" 2>/dev/null || die "the app's code signature does not verify; nothing was installed"

dest_dir="${INSTALL_DIR:-/Applications}"
[ -n "$INSTALL_DIR" ] || [ -w "$dest_dir" ] || dest_dir="$HOME/Applications"
mkdir -p "$dest_dir"
dest="$dest_dir/$APP_NAME.app"

quit_running "$dest"
if [ -d "$dest" ]; then
  old=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$dest/Contents/Info.plist" 2>/dev/null || echo "?")
  say "Replacing $APP_NAME $old"
  rm -rf "$dest"
fi
ditto "$new" "$dest"
# Fetched with curl, so it carries no quarantine mark; clear one anyway in case this script itself was
# downloaded in a browser and something inherited the mark.
xattr -dr com.apple.quarantine "$dest" 2>/dev/null || true
v=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$dest/Contents/Info.plist" 2>/dev/null || echo "")
say "Installed $APP_NAME $v in $dest_dir"

if [ "$cli" = 1 ]; then
  target="$dest/Contents/MacOS/seedwell"
  if [ -w /usr/local/bin ]; then link_dir=/usr/local/bin; else link_dir="$HOME/.local/bin"; mkdir -p "$link_dir"; fi
  ln -sf "$target" "$link_dir/seedwell"
  say "Linked $link_dir/seedwell"
  case ":$PATH:" in *":$link_dir:"*) ;; *) say "Add $link_dir to your PATH to use \`seedwell\` (e.g. in ~/.zshrc: export PATH=\"$link_dir:\$PATH\")" ;; esac
fi

if [ "$launch" = 1 ]; then open "$dest"; say "Done. Seedwell is in your menu bar."; else say "Done."; fi
