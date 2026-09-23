#!/usr/bin/env bash
#
# Package the built release binary as a Debian/Ubuntu .deb.
#
#   cargo build --release
#   packaging/linux/make-deb.sh
#
# Output: target/linux/scire_<version>_<arch>.deb
#
# Beside the tarball rather than instead of it: the tarball is what every
# distro can read and needs nothing but coreutils, and this is for the share of
# desktop Linux where "install a program" means apt. Both are built from
# target/release without rebuilding it, the same contract bundle.sh has.
#
# Two things about a .deb are worth stating, because they are its cost:
#
#   * It is built against *this machine's* libraries. A package built on Ubuntu
#     24.04 will not install on an older release, glibc being the floor. That is
#     a property of a distro package and the reason the tarball stays.
#   * Its dependencies must name real packages of the distro it is for. They are
#     therefore *derived* (`derive_depends`) by resolving the binary's own
#     shared libraries to their owning packages with dpkg-query, rather than
#     hand-listed — a hand-written list is correct on the day it is written and
#     silently wrong after any dependency change. Where dpkg-query is absent
#     (building on a non-Debian box) the fallback list is used and the script
#     says so, because a .deb built on Arch is for testing this script, not for
#     shipping.
#
# No maintainer scripts: the desktop entry and the icon are picked up by dpkg
# triggers from desktop-file-utils and hicolor-icon-theme, so a postinst that
# ran update-desktop-database itself would be doing their job twice.
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

VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$ROOT/Cargo.toml" \
	| sed -n 's/^version *= *"\(.*\)"/\1/p' | head -1)"
[[ -n "$VERSION" ]] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }

# Debian's architecture names are its own: amd64, not x86_64.
if command -v dpkg >/dev/null 2>&1; then
	ARCH="$(dpkg --print-architecture)"
else
	case "$(uname -m)" in
		x86_64) ARCH=amd64 ;;
		aarch64) ARCH=arm64 ;;
		*) echo "error: unknown architecture $(uname -m)" >&2; exit 1 ;;
	esac
fi

# Resolve the binary's shared libraries to the packages that own them. Each
# library is followed to its real path first: dpkg owns the versioned file
# (libssl.so.3), while the loader may have reported a symlink to it.
FALLBACK_DEPENDS="libc6, libasound2t64 | libasound2, libdbus-1-3, libfontconfig1, libfreetype6, libwayland-client0, libxkbcommon0, libx11-6, libxcb1, libvulkan1, libssl3t64 | libssl3"

derive_depends() {
	command -v dpkg-query >/dev/null 2>&1 || return 1
	local pkgs
	pkgs="$(ldd "$BIN" \
		| sed -n 's/.*=> \(\/[^ ]*\).*/\1/p' \
		| while read -r so; do
			readlink -f "$so"
		done \
		| sort -u \
		| xargs -r dpkg-query -S 2>/dev/null \
		| cut -d: -f1 \
		| sort -u \
		| paste -sd, - \
		| sed 's/,/, /g')"
	[[ -n "$pkgs" ]] || return 1
	printf '%s' "$pkgs"
}

if DEPENDS="$(derive_depends)"; then
	echo "depends (derived): $DEPENDS"
else
	DEPENDS="$FALLBACK_DEPENDS"
	echo "warning: could not derive dependencies with dpkg-query — using the" >&2
	echo "         fallback list, which is only as fresh as the last time it was" >&2
	echo "         checked. Do not ship a .deb built this way." >&2
fi

STAGE="$OUT_DIR/deb/scire_${VERSION}_${ARCH}"
DEB="$OUT_DIR/scire_${VERSION}_${ARCH}.deb"

rm -rf "$STAGE"
mkdir -p "$STAGE/DEBIAN" \
	"$STAGE/usr/bin" \
	"$STAGE/usr/share/applications" \
	"$STAGE/usr/share/icons/hicolor/scalable/apps" \
	"$STAGE/usr/share/doc/scire"

install -m 755 "$BIN" "$STAGE/usr/bin/scire"
# The entry's Exec=scire is left alone: /usr/bin is on every PATH, which is the
# whole reason install.sh has to rewrite it and this does not.
install -m 644 "$HERE/scire.desktop" "$STAGE/usr/share/applications/scire.desktop"
install -m 644 "$ROOT/packaging/macos/scire.svg" \
	"$STAGE/usr/share/icons/hicolor/scalable/apps/scire.svg"
install -m 644 "$ROOT/README.md" "$STAGE/usr/share/doc/scire/README.md"

# /usr/share/doc/<pkg>/copyright is where a Debian package's licence goes, and
# policy requires it.
{
	echo "Upstream-Name: scire"
	echo "Source: https://github.com/MatPrayer/scire"
	echo
	sed 's/^/ /; s/^ $//' "$ROOT/LICENSE"
} > "$STAGE/usr/share/doc/scire/copyright"
chmod 644 "$STAGE/usr/share/doc/scire/copyright"

gzip -9nc "$ROOT/CHANGELOG.md" > "$STAGE/usr/share/doc/scire/changelog.gz"
chmod 644 "$STAGE/usr/share/doc/scire/changelog.gz"

# Installed-Size is in KiB and is what apt reports before installing.
INSTALLED_KB="$(du -sk --exclude=DEBIAN "$STAGE" | awk '{print $1}')"

cat > "$STAGE/DEBIAN/control" <<CONTROL
Package: scire
Version: $VERSION
Architecture: $ARCH
Maintainer: the Scirè authors <https://github.com/MatPrayer/scire/issues>
Installed-Size: $INSTALLED_KB
Depends: $DEPENDS
Section: sound
Priority: optional
Homepage: https://github.com/MatPrayer/scire
Description: Desktop music client for Navidrome and local files
 Scirè is a native desktop music client for Navidrome servers, speaking
 Subsonic v1.16.1 and OpenSubsonic, and for local music files. It has gapless
 playback, a SQLite library cache that works with no server reachable, a
 waveform seek bar, synced lyrics and a real-time audio visualizer.
CONTROL
chmod 644 "$STAGE/DEBIAN/control"

# Anything under DEBIAN/ must not be group- or world-writable, and the payload
# must be owned by root once installed. dpkg-deb --root-owner-group does the
# latter; the hand-rolled path below passes --owner/--group to tar.
if command -v dpkg-deb >/dev/null 2>&1; then
	dpkg-deb --build --root-owner-group "$STAGE" "$DEB" >/dev/null
else
	# A .deb is an ar archive of exactly three members in exactly this order:
	# debian-binary, control.tar.gz, data.tar.gz. Hand-rolling it means this
	# script runs on a machine without dpkg — which is where it is developed.
	command -v ar >/dev/null 2>&1 || { echo "error: neither dpkg-deb nor ar found" >&2; exit 1; }
	WORK="$(mktemp -d)"
	trap 'rm -rf "$WORK"' EXIT
	echo "2.0" > "$WORK/debian-binary"
	tar --sort=name --owner=0 --group=0 --numeric-owner \
		--mtime="@${SOURCE_DATE_EPOCH:-0}" \
		-czf "$WORK/control.tar.gz" -C "$STAGE/DEBIAN" .
	tar --sort=name --owner=0 --group=0 --numeric-owner \
		--mtime="@${SOURCE_DATE_EPOCH:-0}" --exclude=./DEBIAN \
		-czf "$WORK/data.tar.gz" -C "$STAGE" .
	rm -f "$DEB"
	# -c create, -q append in order (no symbol table, which is for object
	# archives and which dpkg does not expect to find here).
	ar -qc "$DEB" "$WORK/debian-binary" "$WORK/control.tar.gz" "$WORK/data.tar.gz"
fi

echo "built $DEB"
if command -v dpkg-deb >/dev/null 2>&1; then
	dpkg-deb --info "$DEB" >/dev/null
	echo "verified: $DEB"
fi
