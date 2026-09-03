#!/usr/bin/env bash
#
# Install the Scirè desktop entry and icon on Linux.
#
#   packaging/linux/install-icon.sh              # user install
#   sudo packaging/linux/install-icon.sh --system # system-wide install
#
# Defaults to a per-user install (~/.local/share) — works with Flatpak,
# GNOME, KDE and any XDG-compliant launcher. Pass --system (root) to install
# into /usr/share instead.
#
# The icon is installed as the SVG source (hicolor/scalable) plus PNGs at the
# standard hicolor sizes, so it shows up sharp at any scale and in any theme.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HERE="$ROOT/packaging/macos"
DESKTOP="$(dirname "${BASH_SOURCE[0]}")/scire.desktop"

SYSTEM=0
if [[ "${1:-}" == "--system" ]]; then
	SYSTEM=1
elif [[ $# -gt 0 ]]; then
	echo "usage: $0 [--system]" >&2
	exit 1
fi

if [[ "$SYSTEM" -eq 1 ]]; then
	BASEDIR=/usr/share
	DESKTOP_DIR=/usr/share/applications
else
	BASEDIR="${XDG_DATA_HOME:-$HOME/.local/share}"
	DESKTOP_DIR="$BASEDIR/applications"
fi
ICON_DIR="$BASEDIR/icons/hicolor"

command -v rsvg-convert >/dev/null 2>&1 \
	|| { echo "error: rsvg-convert not found (install librsvg) — needed to render PNG sizes" >&2; exit 1; }
[[ -f "$HERE/scire.svg" ]] || { echo "error: scire.svg not found at $HERE" >&2; exit 1; }

# --- desktop entry -----------------------------------------------------------
mkdir -p "$DESKTOP_DIR"
cp "$DESKTOP" "$DESKTOP_DIR/scire.desktop"

# --- icon (SVG + per-size PNGs into the hicolor icon theme) ------------------
ICON_NAME=scire

mkdir -p "$ICON_DIR/scalable/apps"
cp "$HERE/scire.svg" "$ICON_DIR/scalable/apps/$ICON_NAME.svg"

for size in 16 32 48 64 128 256 512; do
	DIR="$ICON_DIR/${size}x${size}/apps"
	mkdir -p "$DIR"
	rsvg-convert -w "$size" -h "$size" "$HERE/scire.svg" -o "$DIR/$ICON_NAME.png"
done

# Refresh the icon cache so launchers pick up the new icon immediately.
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
	gtk-update-icon-cache -q -f "$ICON_DIR" || true
elif command -v update-desktop-database >/dev/null 2>&1; then
	update-desktop-database -q "$DESKTOP_DIR" || true
fi

echo "installed Scirè icon to $ICON_DIR and desktop entry to $DESKTOP_DIR"
