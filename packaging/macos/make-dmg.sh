#!/usr/bin/env bash
#
# Build a drag-and-drop macOS installer (a .dmg) for Scirè.
#
#   cargo dmg                     # recommended: builds release + packages
#   # or manually:
#   cargo build --release
#   packaging/macos/make-dmg.sh
#
# Output: target/macos/Scirè-<version>.dmg
#
# Steps:
#   1. Reuse bundle.sh to produce Scirè.app (runs it if missing).
#   2. Stage a writable image with the .app, an Applications symlink, and a
#      themed background, then arrange the icons via AppleScript.
#   3. Compress the staged image into the final .dmg.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HERE="$ROOT/packaging/macos"
OUT_DIR="${OUT_DIR:-$ROOT/target/macos}"
APP="$OUT_DIR/Scirè.app"

[[ "$(uname -s)" == "Darwin" ]] || { echo "error: macOS only" >&2; exit 1; }
command -v hdiutil >/dev/null 2>&1 || { echo "error: hdiutil not found" >&2; exit 1; }
command -v osascript >/dev/null 2>&1 || { echo "error: osascript not found" >&2; exit 1; }
command -v rsvg-convert >/dev/null 2>&1 || { echo "error: rsvg-convert not found (brew install librsvg)" >&2; exit 1; }

# Build the .app if needed, or reuse the one bundle.sh produced — but only
# while it is not older than the binary it was made from. Reusing it
# unconditionally meant that every run after the first packaged whatever was
# bundled the first time: `cargo build --release` would update
# target/release/scire, the .app would keep the previous copy, and the .dmg
# shipped a stale binary with no warning at all. That is the kind of thing you
# only notice by testing a fix and finding it absent.
BIN="$ROOT/target/release/scire"
if [[ ! -d "$APP" ]]; then
	echo "Scirè.app not found — running bundle.sh first"
	"$HERE/bundle.sh"
elif [[ -f "$BIN" && "$BIN" -nt "$APP/Contents/MacOS/scire" ]]; then
	echo "Scirè.app is older than target/release/scire — re-running bundle.sh"
	rm -rf "$APP"
	"$HERE/bundle.sh"
fi

VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$ROOT/Cargo.toml" \
	| sed -n 's/^version *= *"\(.*\)"/\1/p' | head -1)"
[[ -n "$VERSION" ]] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }

DMG="$OUT_DIR/Scirè-$VERSION.dmg"
WORK="$(mktemp -d -t scire-dmg)"
MOUNT=""
trap '[[ -n "$MOUNT" ]] && hdiutil detach "$MOUNT" >/dev/null 2>&1 || true; rm -rf "$WORK"' EXIT

STAGE="$WORK/stage"
STAGEDMG="$WORK/staged.dmg"
VOLNAME="Scirè Installer"

# Background dimensions (must match dmg-background.svg's viewBox).
# Was 1000x650; shrunk ~36% to a more compact installer window.
W=640
H=416

# ---- 1. Themed background ---------------------------------------------------
BG_PNG="$WORK/background.png"
rsvg-convert -w "$W" -h "$H" "$HERE/dmg-background.svg" -o "$BG_PNG"

# ---- 2. Stage & mount a writable image --------------------------------------
mkdir -p "$STAGE/.background"
cp "$BG_PNG" "$STAGE/.background/background.png"
cp -R "$APP" "$STAGE/Scirè.app"
ln -s /Applications "$STAGE/Applications"

# Size the staging image to fit the staged content plus headroom. The HFS+
# filesystem, the .background/, and Finder's .DS_Store all take space on top
# of the payload, so a fixed 20m ceiling breaks the moment the .app grows
# past it ("create failed - No space left on device"). Floor at 40m, pad 25%.
STAGE_KB="$(du -sk "$STAGE" | awk '{print $1}')"
SIZE_M="$(( STAGE_KB * 125 / 100 / 1024 + 1 ))"
[[ "$SIZE_M" -lt 40 ]] && SIZE_M=40

hdiutil create -volname "$VOLNAME" -srcfolder "$STAGE" \
	-ov -fs HFS+ -format UDRW -size "${SIZE_M}m" "$STAGEDMG" >/dev/null

MOUNT="$(hdiutil attach "$STAGEDMG" -nobrowse -readwrite 2>/dev/null \
	| awk -F '\t' '/\/Volumes\//{print $NF}' | head -1)"
[[ -n "$MOUNT" ]] || { echo "error: failed to mount staging image" >&2; exit 1; }

# ---- 3. Arrange icons & set the background via Finder ------------------------
# Finder's icon coordinates are relative to the visible content area of the
# window (below the title bar). We size the window to the background then
# place the app bottom-left and Applications bottom-right. AppleScript blocks
# until the window is drawn so the layout is captured into .DS_Store.
BG_ON_VOL="$MOUNT/.background/background.png"
osascript <<EOF
tell application "Finder"
	set win to make new Finder window
	set target of win to POSIX file "$MOUNT"
	tell win
		set toolbar visible to false
		set statusbar visible to false
		set current view to icon view
		set bounds to {200, 120, $((200 + W)), $((120 + H))}
		delay 0.3
		tell its icon view options
			set icon size to 110
			set text size to 14
			set arrangement to not arranged
			set shows item info to false
			set background picture to POSIX file "$BG_ON_VOL"
		end tell
		delay 0.3
	end tell
	set b to bounds of win
	set winW to (item 3 of b) - (item 1 of b)
	set winH to (item 4 of b) - (item 2 of b)
	-- approximate icon grid inset; tune these to taste
	set position of item "Scirè.app" of win to {90, (winH - 150)}
	set position of item "Applications" of win to {((winW - 150)), (winH - 150)}
	delay 0.5
	close win
end tell
EOF

# ---- 4. Flush, detach, compress ---------------------------------------------
sleep 1
hdiutil detach "$MOUNT" >/dev/null
MOUNT=""
hdiutil convert "$STAGEDMG" -format UDZO -imagekey zlib-level=9 -o "$DMG" >/dev/null

echo "built $DMG"
hdiutil verify "$DMG" >/dev/null
echo "verified: $DMG"
