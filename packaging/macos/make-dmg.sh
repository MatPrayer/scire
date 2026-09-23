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
#   2. Convert dmg-icon.png to an .icns volume icon and stage a writable
#      image with the .app, an Applications symlink, and a themed background,
#      then set the volume icon and arrange the Finder icons via AppleScript.
#   3. Compress the staged image into the final .dmg.
#
# Step 2's *presentation* — the background picture and the icon positions — is
# optional, and everything that makes the image an installer is not. A CI
# runner has no usable Finder: arranging the window means driving Finder over
# Apple Events, which needs a logged-in GUI session and an Automation consent
# no unattended machine can give, so the call is denied or simply never
# returns. Rather than ship a .zip from CI and a .dmg from a desk — two
# different downloads for the same release — the themed pass is skipped where
# it cannot run and the .dmg is built regardless: the .app, the Applications
# symlink to drag it onto, and the volume icon. What is lost is the wallpaper
# behind the two icons. Set DMG_PLAIN=1 to skip it deliberately.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HERE="$ROOT/packaging/macos"
OUT_DIR="${OUT_DIR:-$ROOT/target/macos}"
APP="$OUT_DIR/Scirè.app"

[[ "$(uname -s)" == "Darwin" ]] || { echo "error: macOS only" >&2; exit 1; }
command -v hdiutil >/dev/null 2>&1 || { echo "error: hdiutil not found" >&2; exit 1; }

# Does Finder answer an Apple Event? Asked with a deadline, because the failure
# this guards against is not always an error: a denied or unattended Automation
# request can leave osascript waiting indefinitely, and a packaging script that
# hangs is worse than one that drops the wallpaper.
FINDER_PROBE_TIMEOUT=10
finder_available() {
	command -v osascript >/dev/null 2>&1 || return 1
	osascript -e 'tell application "Finder" to get name' >/dev/null 2>&1 &
	local probe=$! waited=0
	while kill -0 "$probe" 2>/dev/null; do
		if (( waited >= FINDER_PROBE_TIMEOUT * 10 )); then
			kill -9 "$probe" 2>/dev/null || true
			wait "$probe" 2>/dev/null || true
			return 1
		fi
		sleep 0.1
		waited=$(( waited + 1 ))
	done
	wait "$probe"
}

THEMED=1
if [[ "${DMG_PLAIN:-0}" == 1 ]]; then
	THEMED=0
	echo "DMG_PLAIN=1 — building a plain image (no background, no icon layout)"
elif ! command -v rsvg-convert >/dev/null 2>&1; then
	THEMED=0
	echo "note: rsvg-convert not found (brew install librsvg) — no themed background"
elif ! finder_available; then
	THEMED=0
	echo "note: Finder did not answer within ${FINDER_PROBE_TIMEOUT}s — no themed background"
fi

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
VOLNAME="Install Scirè"

# Background dimensions (must match dmg-background.svg's viewBox).
# Was 1000x650; shrunk ~36% to a more compact installer window.
W=640
H=416

# ---- 1. Themed background ---------------------------------------------------
BG_PNG="$WORK/background.png"
if (( THEMED )); then
	rsvg-convert -w "$W" -h "$H" "$HERE/dmg-background.svg" -o "$BG_PNG"
fi

# ---- 1b. Volume icon (PNG → .icns) ------------------------------------------
# Independent of the themed pass: the icon is a file at the volume root plus a
# flag, with no Finder scripting involved. sips, iconutil and SetFile ship with
# the Command Line Tools, so a machine that can build the app has them — but a
# missing one costs the icon, not the installer.
ICON_ICNS=""
if command -v sips >/dev/null 2>&1 && command -v iconutil >/dev/null 2>&1 \
	&& command -v SetFile >/dev/null 2>&1; then
	ICONSET="$WORK/icon.iconset"
	mkdir -p "$ICONSET"
	ICON_SRC="$HERE/dmg-icon.png"
	for size in 16 32 128 256 512; do
		sips -z "$size" "$size" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
		two=$(( size * 2 ))
		sips -z "$two" "$two" "$ICON_SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
	done
	ICON_ICNS="$WORK/volume-icon.icns"
	iconutil -c icns "$ICONSET" -o "$ICON_ICNS"
else
	echo "note: sips/iconutil/SetFile not all present — no volume icon"
fi

# ---- 2. Stage & mount a writable image --------------------------------------
if (( THEMED )); then
	mkdir -p "$STAGE/.background"
	cp "$BG_PNG" "$STAGE/.background/background.png"
else
	mkdir -p "$STAGE"
fi
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

# Volume icon: Finder picks up .VolumeIcon.icns at the volume root once the
# custom-icon flag is set. hdiutil has no direct "set icon" option.
if [[ -n "$ICON_ICNS" ]]; then
	cp "$ICON_ICNS" "$MOUNT/.VolumeIcon.icns"
	SetFile -a C "$MOUNT"
fi

# ---- 3. Arrange icons & set the background via Finder ------------------------
# Finder's icon coordinates are relative to the visible content area of the
# window (below the title bar). We size the window to the background then
# place the app bottom-left and Applications bottom-right. AppleScript blocks
# until the window is drawn so the layout is captured into .DS_Store.
if (( THEMED )); then
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
	set position of item "Scirè.app" of win to {160, 207}
	set position of item "Applications" of win to {480, 207}
	delay 0.5
	close win
end tell
EOF
fi

# ---- 4. Flush, detach, compress ---------------------------------------------
sleep 1
hdiutil detach "$MOUNT" >/dev/null
MOUNT=""
hdiutil convert "$STAGEDMG" -format UDZO -imagekey zlib-level=9 -o "$DMG" >/dev/null

echo "built $DMG"
hdiutil verify "$DMG" >/dev/null
echo "verified: $DMG"
