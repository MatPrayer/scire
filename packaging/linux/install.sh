#!/usr/bin/env bash
#
# Install Scirè from an extracted release tarball.
#
#   ./install.sh                # per-user install into ~/.local
#   sudo ./install.sh --system  # system-wide install into /usr/local + /usr/share
#   ./install.sh --uninstall    # remove whatever the matching mode installed
#
# This script lives *inside the tarball*, beside the binary it installs — it
# does not build anything and needs nothing but coreutils. The repository copy
# is packaging/linux/install.sh; make-tarball.sh stages it at the top level.
#
# The icon is installed as the SVG alone (hicolor/scalable), unlike the
# from-source install-icon.sh which also renders PNGs: rendering needs librsvg,
# and requiring a build dependency of a *binary* release is the wrong trade for
# the few launchers that cannot read an SVG.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

MODE=user
ACTION=install
for arg in "$@"; do
	case "$arg" in
	--system) MODE=system ;;
	--uninstall) ACTION=uninstall ;;
	*)
		echo "usage: $0 [--system] [--uninstall]" >&2
		exit 1
		;;
	esac
done

if [[ "$MODE" == system ]]; then
	BINDIR=/usr/local/bin
	DATADIR=/usr/share
else
	BINDIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
	DATADIR="${XDG_DATA_HOME:-$HOME/.local/share}"
fi
DESKTOP_DIR="$DATADIR/applications"
ICON_DIR="$DATADIR/icons/hicolor/scalable/apps"

if [[ "$ACTION" == uninstall ]]; then
	rm -f "$BINDIR/scire" "$DESKTOP_DIR/scire.desktop" "$ICON_DIR/scire.svg"
	echo "removed Scirè from $BINDIR and $DATADIR"
	exit 0
fi

for f in scire scire.desktop scire.svg; do
	[[ -e "$HERE/$f" ]] || { echo "error: $f missing — is this an extracted release tarball?" >&2; exit 1; }
done

mkdir -p "$BINDIR" "$DESKTOP_DIR" "$ICON_DIR"
install -m 755 "$HERE/scire" "$BINDIR/scire"
# The desktop entry ships `Exec=scire`, which only resolves for a launcher that
# searches PATH — and a per-user $BINDIR is routinely absent from the PATH a
# graphical session starts with. Write the absolute path instead.
sed "s|^Exec=scire$|Exec=$BINDIR/scire|" "$HERE/scire.desktop" > "$DESKTOP_DIR/scire.desktop"
install -m 644 "$HERE/scire.svg" "$ICON_DIR/scire.svg"

if command -v update-desktop-database >/dev/null 2>&1; then
	update-desktop-database -q "$DESKTOP_DIR" || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
	gtk-update-icon-cache -q -f "$DATADIR/icons/hicolor" || true
fi

echo "installed $BINDIR/scire"
case ":$PATH:" in
*":$BINDIR:"*) ;;
*) echo "note: $BINDIR is not on your PATH — the launcher entry works either way" ;;
esac
