#!/usr/bin/env bash
#
# Package the built release binary as a Linux tarball.
#
#   cargo build --release
#   packaging/linux/make-tarball.sh
#
# Output: target/linux/scire-<version>-linux-<arch>.tar.gz
#
# The tarball holds the binary, the desktop entry, the icon, an install.sh that
# needs nothing but coreutils, and the licence/changelog. It is the format with
# no distro in it: make-deb.sh sits beside it for apt, and packaging/aur covers
# Arch by building from source, but neither can serve a distro that is neither
# — which this does, needing nothing a POSIX system does not already have.
#
# The binary is not rebuilt — this packages what is in target/release, the same
# contract the macOS bundle.sh has.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HERE="$ROOT/packaging/linux"
OUT_DIR="${OUT_DIR:-$ROOT/target/linux}"
BIN="${BIN:-$ROOT/target/release/scire}"

[[ "$(uname -s)" == "Linux" ]] || { echo "error: Linux only" >&2; exit 1; }
if [[ ! -x "$BIN" ]]; then
	echo "error: $BIN not found — run 'cargo build --release' first" >&2
	exit 1
fi

# Version comes from the workspace manifest so the tarball cannot drift from it.
VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$ROOT/Cargo.toml" \
	| sed -n 's/^version *= *"\(.*\)"/\1/p' | head -1)"
[[ -n "$VERSION" ]] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }

ARCH="$(uname -m)"
NAME="scire-$VERSION-linux-$ARCH"
STAGE="$OUT_DIR/$NAME"

rm -rf "$STAGE"
mkdir -p "$STAGE"

install -m 755 "$BIN" "$STAGE/scire"
install -m 755 "$HERE/install.sh" "$STAGE/install.sh"
install -m 644 "$HERE/scire.desktop" "$STAGE/scire.desktop"
install -m 644 "$ROOT/packaging/macos/scire.svg" "$STAGE/scire.svg"
install -m 644 "$ROOT/LICENSE" "$STAGE/LICENSE"
install -m 644 "$ROOT/CHANGELOG.md" "$STAGE/CHANGELOG.md"
install -m 644 "$ROOT/README.md" "$STAGE/README.md"

# --sort=name and a fixed mtime so two builds of the same commit produce the
# same bytes; --owner/--group so the archive does not carry the build user.
tar --sort=name --owner=0 --group=0 --numeric-owner \
	--mtime="@${SOURCE_DATE_EPOCH:-0}" \
	-czf "$OUT_DIR/$NAME.tar.gz" -C "$OUT_DIR" "$NAME"

echo "built $OUT_DIR/$NAME.tar.gz"
