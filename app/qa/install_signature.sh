#!/usr/bin/env bash
# install.sh's signature rule, without the network or the real app: a release "on GitHub" (here a file:// copy laid
# out like GitHub's download URLs, which install.sh treats as the real thing) installs only when it is signed by
# Seedwell's Team ID and notarized. Checked with a tiny stand-in app:
#   1. signed ad hoc, the latest release:                       refused, nothing installed
#   2. signed ad hoc, pinned with --version to one before FIRST_SIGNED: installed (the releases before signing)
#   3. signed ad hoc, pinned to FIRST_SIGNED or later:          refused
#   4. signed with a self-signed certificate (no team):         refused, nothing installed (skipped when the
#      temporary keychain cannot be made)
#   5. the same ad-hoc app with --zip (a test build):           installed
# A signed and notarized release (accepted) needs the real Developer ID: release-zip.sh --signed checks that with
# spctl on every release. Everything happens in a temporary folder; no app is opened (--no-launch).
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
TMP="$(mktemp -d)"
KC="$TMP/test.keychain-db"
# create-keychain adds the new keychain to the user's search list: it is put back as it was right after.
SEARCH=()
while IFS= read -r k; do k="${k#"${k%%[![:space:]]*}"}"; k="${k%\"}"; SEARCH+=("${k#\"}"); done < <(security list-keychains -d user)
restore_search() { [ "${#SEARCH[@]}" = 0 ] || security list-keychains -d user -s "${SEARCH[@]}"; }
cleanup() { restore_search; security delete-keychain "$KC" 2>/dev/null || true; rm -rf "$TMP"; }
trap cleanup EXIT
FIRST_SIGNED="$(sed -n 's/^FIRST_SIGNED=//p' "$REPO/install.sh")"
fails=0
pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1"; fails=$((fails + 1)); }

# stand_in <dir> <version> [identity]: a minimal Seedwell.app, signed ad hoc or with the identity.
stand_in() {
  local app="$1/Seedwell.app"
  mkdir -p "$app/Contents/MacOS"
  printf '#!/bin/sh\nexit 0\n' > "$app/Contents/MacOS/SeedwellApp"; chmod +x "$app/Contents/MacOS/SeedwellApp"
  cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>io.github.kkollsga.Seedwell.signature-test</string>
<key>CFBundleExecutable</key><string>SeedwellApp</string>
<key>CFBundleShortVersionString</key><string>$2</string>
</dict></plist>
PLIST
  if [ -n "${3:-}" ]; then codesign --force --keychain "$KC" --sign "$3" "$app" 2>/dev/null
  else codesign --force --sign - "$app" 2>/dev/null; fi
}

# release <version> [identity]: the stand-in as a release at $TMP/gh/<version>, laid out as GitHub serves one.
release() {
  local v="$1" d="$TMP/gh/$1/releases/download/v$1"
  rm -rf "$TMP/gh/$v"; mkdir -p "$d" "$TMP/gh/$v/releases/latest/download" "$TMP/build-$v"
  stand_in "$TMP/build-$v" "$v" "${2:-}"
  ditto -c -k --keepParent "$TMP/build-$v/Seedwell.app" "$d/Seedwell-$v.zip"
  (cd "$d" && shasum -a 256 "Seedwell-$v.zip" > SHA256SUMS)
  cp "$d/"* "$TMP/gh/$v/releases/latest/download/"
}

# install <case> <version> <args...>: install.sh from that release into a fresh folder; prints its output.
install() {
  local v="$1"; shift
  rm -rf "$TMP/inst"; mkdir -p "$TMP/inst"
  SEEDWELL_REPO_URL="file://$TMP/gh/$v" SEEDWELL_INSTALL_DIR="$TMP/inst" bash "$REPO/install.sh" --no-launch "$@" 2>&1
}
installed() { [ -d "$TMP/inst/Seedwell.app" ]; }

# 1. ad hoc, latest
release 9.9.9
out="$(install 9.9.9 || true)"
if ! installed && grep -q "not signed by its developer" <<< "$out"; then pass "an ad-hoc latest release is refused"
else fail "an ad-hoc latest release is refused: $out"; fi

# 2. ad hoc, pinned before FIRST_SIGNED
old="0.1.9"
release "$old"
out="$(install "$old" --version "$old" || true)"
if installed && grep -q "from before releases were signed" <<< "$out"; then pass "a pinned release from before signing ($old) installs"
else fail "a pinned release from before signing ($old) installs: $out"; fi

# 3. ad hoc, pinned at FIRST_SIGNED
release "$FIRST_SIGNED"
out="$(install "$FIRST_SIGNED" --version "$FIRST_SIGNED" || true)"
if ! installed && grep -q "not signed by its developer" <<< "$out"; then pass "an ad-hoc release pinned at $FIRST_SIGNED is refused"
else fail "an ad-hoc release pinned at $FIRST_SIGNED is refused: $out"; fi

# 4. self-signed certificate (a signature with no team, or someone else's)
if security create-keychain -p test "$KC" 2>/dev/null && restore_search && security unlock-keychain -p test "$KC" \
  && openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=Seedwell Signature Test/OU=XXXXXXXXXX" \
    -addext "keyUsage=critical,digitalSignature" -addext "extendedKeyUsage=critical,codeSigning" \
    -keyout "$TMP/key.pem" -out "$TMP/cert.pem" 2>/dev/null \
  && openssl pkcs12 -export -legacy -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -out "$TMP/id.p12" -passout pass:test 2>/dev/null \
  && security import "$TMP/id.p12" -k "$KC" -P test -T /usr/bin/codesign >/dev/null 2>&1 \
  && security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k test "$KC" >/dev/null 2>&1; then
  release 9.9.8 "Seedwell Signature Test"
  out="$(install 9.9.8 || true)"
  if ! installed && grep -q "not signed by its developer" <<< "$out"; then pass "a release signed by another certificate is refused ($(grep -o "it is signed by team [^;)]*" <<< "$out" || echo "no team"))"
  else fail "a release signed by another certificate is refused: $out"; fi
else
  echo "SKIP  a release signed by another certificate (no temporary keychain or certificate here)"
fi

# 5. --zip test build
out="$(install 9.9.9 --zip "$TMP/gh/9.9.9/releases/download/v9.9.9/Seedwell-9.9.9.zip" || true)"
if installed; then pass "an ad-hoc --zip test build installs"; else fail "an ad-hoc --zip test build installs: $out"; fi

[ "$fails" = 0 ] && echo "install_signature: PASS" || { echo "install_signature: $fails FAILED"; exit 1; }
