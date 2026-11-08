#!/usr/bin/env bash
# The build key and the signed build.json of a folder the app watches for development builds (SPEC 12A). A device
# installs a build from a watched folder only when that folder's build.json is signed with a build key its Seedwell
# trusts (compiled in: crates/buildsig/src/lib.rs TRUSTED_KEYS) and the zip is the one signed.
#
#   tools/build-sign.sh key new        make the build key, once, on the Mac that publishes builds: the secret key is
#                                      kept in this Mac's login Keychain (service "Seedwell build key"); prints the
#                                      public key to put in TRUSTED_KEYS (trusted from the release that carries it)
#   tools/build-sign.sh key show       the public key of the build key in the Keychain
#   tools/build-sign.sh add <folder> <zip> --sha256 HEX [--commit C] [--minimum V]
#                                      sign <folder>/build.json with <zip> (a file in <folder>, which must have that
#                                      SHA-256) as its system's build; the other system's build stays
#   tools/build-sign.sh verify <folder>   check <folder>/build.json against the trusted keys, and the zips against it
#
# Run it through tools/sysguard/heavy.sh: it builds the signer (seedwell-buildsig) when it is missing or older than its
# sources. tools/dev-build.sh --to calls `add` for the Mac's build. A Windows build (app-windows/dev-build.ps1 on the
# PC) arrives in the folder unsigned; its SHA-256, as the PC's agent posted it in the agent channel, is what `add`
# signs: never sign a zip whose checksum did not come from its builder.
#
# The secret key goes from the Keychain to the signer's standard input: never an argument, a file or a log.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
SERVICE="Seedwell build key"
ACCOUNT="build-key"
SIGNER="$REPO/target/release/seedwell-build-sign"

die() { echo "build-sign: $*" >&2; exit 1; }

signer() {
  if [ ! -x "$SIGNER" ] || [ -n "$(find "$REPO/crates/buildsig" -newer "$SIGNER" -type f 2>/dev/null | head -1)" ]; then
    (cd "$REPO" && cargo build --release --locked --quiet -p seedwell-buildsig --bin seedwell-build-sign) >&2
  fi
  "$SIGNER" "$@"
}

secret() { security find-generic-password -s "$SERVICE" -a "$ACCOUNT" -w 2>/dev/null; }

case "${1:-}" in
  key)
    case "${2:-}" in
      new)
        if secret >/dev/null; then die "this Mac's Keychain already has a build key (tools/build-sign.sh key show)"; fi
        seed="$(signer keygen 2>/dev/null)"
        [ ${#seed} -eq 64 ] || die "the signer made no key"
        # Through security's own command line (standard input), so the secret is never a process argument.
        printf 'add-generic-password -s "%s" -a "%s" -l "%s" -D "Seedwell development-build signing key" -T /usr/bin/security -w "%s"\n' \
          "$SERVICE" "$ACCOUNT" "$SERVICE" "$seed" | security -i >/dev/null
        unset seed
        pk="$(secret | signer public)" || die "the key was not kept in the Keychain"
        echo "Made the build key; its secret half is in this Mac's login Keychain (\"$SERVICE\")."
        echo "Public key (add it to TRUSTED_KEYS in crates/buildsig/src/lib.rs; devices trust it from the release that carries it):"
        echo "$pk"
        ;;
      show)
        secret >/dev/null || die "no build key in this Mac's Keychain (tools/build-sign.sh key new)"
        secret | signer public
        ;;
      *) die "usage: tools/build-sign.sh key new|show" ;;
    esac
    ;;
  add)
    shift
    secret >/dev/null || die "no build key in this Mac's Keychain (tools/build-sign.sh key new): nothing signed"
    secret | signer add "$@"
    ;;
  verify)
    shift
    signer verify "$@"
    ;;
  -h|--help|"") sed -n '2,20p' "$0" ;;
  *) die "unknown command $1 (tools/build-sign.sh --help)" ;;
esac
