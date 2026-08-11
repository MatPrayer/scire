#!/usr/bin/env bash
#
# Wrap the already-built release binary in a macOS .app bundle.
#
#   cargo build --release
#   packaging/macos/bundle.sh
#
# Output: target/macos/Scirè.app  (override with OUT_DIR)
#
# The binary is not rebuilt — this script only packages what is in
# target/release. The bundle is ad-hoc signed, which is enough to launch it
# locally on Apple silicon but not enough to distribute it: another machine
# gets Gatekeeper's "damaged" dialog unless the app is signed with a Developer
# ID certificate and notarised.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HERE="$ROOT/packaging/macos"
OUT_DIR="${OUT_DIR:-$ROOT/target/macos}"
APP="$OUT_DIR/Scirè.app"
BIN="$ROOT/target/release/scire"
BUNDLE_ID="${BUNDLE_ID:-io.github.lanamirko.scire}"

[[ "$(uname -s)" == "Darwin" ]] || { echo "error: macOS only" >&2; exit 1; }
if [[ ! -x "$BIN" ]]; then
	echo "error: $BIN not found — run 'cargo build --release' first" >&2
	exit 1
fi

# Version comes from the workspace manifest so the bundle can't drift from it.
VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/p' "$ROOT/Cargo.toml" \
	| sed -n 's/^version *= *"\(.*\)"/\1/p' | head -1)"
[[ -n "$VERSION" ]] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$BIN" "$APP/Contents/MacOS/scire"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@BUNDLE_ID@/$BUNDLE_ID/g" \
	"$HERE/Info.plist.in" > "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

# Icon: rendered from the SVG when librsvg is around, otherwise fall back to a
# checked-in .icns so packaging still works on a machine without it.
ICNS="$OUT_DIR/AppIcon.icns"
if command -v rsvg-convert >/dev/null 2>&1; then
	SET="$OUT_DIR/AppIcon.iconset"
	rm -rf "$SET"; mkdir -p "$SET"
	for size in 16 32 128 256 512; do
		rsvg-convert -w $size -h $size "$HERE/AppIcon.svg" -o "$SET/icon_${size}x${size}.png"
		rsvg-convert -w $((size * 2)) -h $((size * 2)) "$HERE/AppIcon.svg" \
			-o "$SET/icon_${size}x${size}@2x.png"
	done
	iconutil -c icns "$SET" -o "$ICNS"
	rm -rf "$SET"
elif [[ -f "$HERE/AppIcon.icns" ]]; then
	cp "$HERE/AppIcon.icns" "$ICNS"
else
	echo "warning: no rsvg-convert and no prebuilt AppIcon.icns — bundling without an icon" >&2
fi
[[ -f "$ICNS" ]] && cp "$ICNS" "$APP/Contents/Resources/AppIcon.icns"

# arm64 refuses to run an unsigned binary, and copying the binary invalidated
# the linker's ad-hoc signature — re-sign the whole bundle.
#
# Which identity matters beyond Gatekeeper: an ad-hoc signature's designated
# requirement is a bare cdhash, which changes on every rebuild, so the login
# keychain's grant for the stored Navidrome password never matches the new
# binary and macOS asks for the keychain password again (twice — trusted-app
# ACL, then partition list). A real certificate makes the requirement
# identity-based and the grant sticks. See signing-identity.sh.
SIGN_ID="${CODESIGN_ID:-}"
if [[ -z "$SIGN_ID" ]]; then
	SIGN_ID="$("$HERE/signing-identity.sh" --print 2>/dev/null || true)"
fi
if [[ -z "$SIGN_ID" ]]; then
	SIGN_ID="-"
	echo "warning: no code-signing identity — signing ad-hoc." >&2
	echo "         macOS will ask for your keychain password on every rebuild;" >&2
	echo "         run packaging/macos/signing-identity.sh once to stop that." >&2
fi
codesign --force --sign "$SIGN_ID" --timestamp=none "$APP"
codesign --verify --strict "$APP"

echo "built $APP ($VERSION, $(lipo -archs "$APP/Contents/MacOS/scire"))"
