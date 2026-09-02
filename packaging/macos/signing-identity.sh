#!/usr/bin/env bash
#
# Create (once) a local self-signed code-signing identity for Scirè, so that
# bundle.sh can sign with a *stable* identity instead of ad-hoc.
#
#   packaging/macos/signing-identity.sh          # create if missing
#   packaging/macos/signing-identity.sh --print  # print the identity name only
#   packaging/macos/signing-identity.sh --unlock # unlock the keychain for codesign
#   packaging/macos/signing-identity.sh --remove # delete the keychain again
#
# Why this exists
# ---------------
# `codesign --sign -` (ad-hoc) gives the bundle a designated requirement of
# nothing but its own cdhash:
#
#     designated => cdhash H"9dcc3fb1862f9b4c5fde567d3531ae8069f5b3c7"
#
# The login keychain records that cdhash in the ACL (and the partition list) of
# the stored Navidrome password when you grant access. Every rebuild produces a
# new cdhash, so the grant never matches again and macOS re-asks — twice, once
# for the trusted-application ACL and once for the partition list.
#
# Signing with a real certificate makes the requirement identity-based instead:
#
#     designated => identifier "io.github.lanamirko.scire" and certificate leaf = H"..."
#
# which survives rebuilds, so "Always Allow" is answered once and stays.
#
# The certificate is self-signed: enough for a stable identity locally, not
# enough to distribute (that still needs a Developer ID cert + notarisation).
set -euo pipefail

IDENTITY="${CODESIGN_IDENTITY_NAME:-Scire Local Signing}"
KEYCHAIN_NAME="scire-signing.keychain-db"
KEYCHAIN="$HOME/Library/Keychains/$KEYCHAIN_NAME"
# This password protects nothing but a self-signed local signing key, and it is
# kept out of the login keychain precisely so that no step here has to ask for
# (or be given) your account password.
KEYCHAIN_PASSWORD="scire-signing"
# Must be non-empty: `security import` fails an empty-password PKCS#12 with
# "MAC verification failed during PKCS12 import (wrong password?)" whichever
# algorithms it was exported with.
P12_PASSWORD="scire-signing"

# `find-identity -v` filters to *trusted* identities and a self-signed cert is
# never that (CSSMERR_TP_NOT_TRUSTED) — codesign accepts it anyway, so match on
# the unfiltered list.
have_identity() {
	security find-identity -p codesigning "$KEYCHAIN" 2>/dev/null | grep -q "\"$IDENTITY\""
}

[[ "$(uname -s)" == "Darwin" ]] || { echo "error: macOS only" >&2; exit 1; }

case "${1:-}" in
--print)
	have_identity || { echo "error: identity '$IDENTITY' not found — run $0 first" >&2; exit 1; }
	echo "$IDENTITY"
	exit 0
	;;
--unlock)
	have_identity || { echo "error: identity '$IDENTITY' not found — run $0 first" >&2; exit 1; }
	security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
	exit 0
	;;
--remove)
	security delete-keychain "$KEYCHAIN" 2>/dev/null || true
	echo "removed $KEYCHAIN_NAME"
	exit 0
	;;
"") ;;
*)
	echo "error: unknown option '$1'" >&2
	exit 1
	;;
esac

if have_identity; then
	echo "identity '$IDENTITY' already exists in $KEYCHAIN_NAME — nothing to do"
	exit 0
fi

command -v openssl >/dev/null 2>&1 || { echo "error: openssl not found" >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# A CA:false leaf with the codeSigning EKU is all codesign asks of a cert; it
# does not require the chain to be trusted, only that the private key is
# reachable.
openssl req -x509 -newkey rsa:2048 -nodes -sha256 -days 3650 \
	-keyout "$TMP/key.pem" -out "$TMP/cert.pem" \
	-subj "/CN=$IDENTITY" \
	-addext "basicConstraints=critical,CA:false" \
	-addext "keyUsage=critical,digitalSignature" \
	-addext "extendedKeyUsage=critical,codeSigning" 2>/dev/null

# The PBE algorithms are pinned to the old SHA1/3DES ones: OpenSSL 3 defaults
# to AES-256-CBC with a SHA-256 MAC, and Apple's importer rejects that with
# "MAC verification failed during PKCS12 import (wrong password?)".
openssl pkcs12 -export -out "$TMP/identity.p12" \
	-inkey "$TMP/key.pem" -in "$TMP/cert.pem" -passout "pass:$P12_PASSWORD" \
	-certpbe PBE-SHA1-3DES -keypbe PBE-SHA1-3DES -macalg sha1 2>/dev/null

# A dedicated keychain, not the login one: it lets the partition list below be
# set without prompting, and `--remove` can drop the whole thing cleanly.
security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN_NAME"
# Anything below this point failing would leave a keychain with no usable
# identity in it, which the next run would not recreate.
trap 'rm -rf "$TMP"; security delete-keychain "$KEYCHAIN" 2>/dev/null || true' EXIT
# No -l (lock on sleep) and no -u/-t (inactivity timeout): a keychain holding
# nothing but a local signing key does not need either, and both make codesign
# prompt for the keychain password after a lid close or an idle hour — a prompt
# that is *not* answerable with the account password, which is the confusing
# part. It still locks on logout/reboot, which is why bundle.sh unlocks first.
security set-keychain-settings "$KEYCHAIN"
security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
security import "$TMP/identity.p12" -k "$KEYCHAIN" -P "$P12_PASSWORD" -T /usr/bin/codesign
# Without this codesign is prompted for on every single signing run.
security set-key-partition-list -S apple-tool:,apple:,codesign: \
	-s -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN" >/dev/null

# codesign only searches the user's keychain list, so add ours to it (keeping
# whatever was already there).
EXISTING="$(security list-keychains -d user | sed -e 's/^ *"//' -e 's/"$//')"
# shellcheck disable=SC2086
security list-keychains -d user -s $EXISTING "$KEYCHAIN"

trap 'rm -rf "$TMP"' EXIT
echo "created identity '$IDENTITY' in $KEYCHAIN_NAME"
echo "next: cargo build --release && packaging/macos/bundle.sh"
echo "the first launch after that still asks for the keychain password —"
echo "answer 'Always Allow' and it will not ask again."
