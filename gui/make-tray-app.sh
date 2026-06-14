#!/bin/bash
# Build the macrdp Controller menu-bar app: `swift build` the SwiftPM
# executable, wrap it in macrdpController.app (LSUIElement, signed), install it.
#
# Env overrides:
#   APP_DIR=/Applications              # install location (default /Applications)
#   CODESIGN_IDENTITY="-"             # "-" = ad-hoc; or a Developer ID name
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GUI_DIR="$REPO_ROOT/gui"
APP_DIR="${APP_DIR:-/Applications}"
IDENTITY="${CODESIGN_IDENTITY:--}"
APP_NAME="macrdpController.app"

VERSION="$(grep -m1 '^version' "$REPO_ROOT/Cargo.toml" | cut -d'"' -f2)"
[ -n "$VERSION" ] || { echo "could not read version from Cargo.toml" >&2; exit 1; }

echo "==> macrdpController v$VERSION  (identity: $IDENTITY, install: $APP_DIR)"

echo "==> swift build -c release"
( cd "$GUI_DIR" && swift build -c release )
BIN="$GUI_DIR/.build/release/macrdptray"
[ -x "$BIN" ] || { echo "build produced no binary at $BIN" >&2; exit 1; }

STAGE="$REPO_ROOT/target/$APP_NAME"   # target/ is gitignored
echo "==> staging $STAGE"
rm -rf "$STAGE"
mkdir -p "$STAGE/Contents/MacOS" "$STAGE/Contents/Resources"

cat > "$STAGE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>macrdp Controller</string>
    <key>CFBundleDisplayName</key><string>macrdp Controller</string>
    <key>CFBundleIdentifier</key><string>com.clintcan.macrdp.controller</string>
    <key>CFBundleExecutable</key><string>macrdptray</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSUIElement</key><true/>
</dict>
</plist>
PLIST

cp "$BIN" "$STAGE/Contents/MacOS/macrdptray"
chmod +x "$STAGE/Contents/MacOS/macrdptray"

# Ad-hoc ("-") can't use a secure timestamp; a real Developer ID must
# (notarization requires it).
if [ "$IDENTITY" = "-" ]; then TS="--timestamp=none"; else TS="--timestamp"; fi
echo "==> codesign (hardened runtime, ts: $TS)"
codesign --force --options runtime $TS -s "$IDENTITY" "$STAGE/Contents/MacOS/macrdptray"
codesign --force --options runtime $TS -s "$IDENTITY" "$STAGE"
codesign --verify --deep --strict "$STAGE"

# Optional notarization (NOTARIZE=1, real Developer ID + NOTARY_PROFILE).
if [ "${NOTARIZE:-0}" = "1" ]; then
    [ "$IDENTITY" != "-" ] || { echo "NOTARIZE=1 needs a real CODESIGN_IDENTITY (not ad-hoc)" >&2; exit 1; }
    "$REPO_ROOT/packaging/notarize.sh" "$STAGE"
fi

echo "==> installing to $APP_DIR/$APP_NAME"
if ! mkdir -p "$APP_DIR" 2>/dev/null || [ ! -w "$APP_DIR" ]; then
    echo "    $APP_DIR not writable — re-run with sudo or set APP_DIR=\$HOME/Applications" >&2
    exit 1
fi
rm -rf "$APP_DIR/$APP_NAME"
cp -R "$STAGE" "$APP_DIR/$APP_NAME"
codesign --verify --strict "$APP_DIR/$APP_NAME"

echo
echo "Done. Installed: $APP_DIR/$APP_NAME"
echo "Launch it:  open \"$APP_DIR/$APP_NAME\"   (a display icon appears in the menu bar)"
echo "Note: it controls the LaunchAgent from packaging/ — run make-app.sh +"
echo "      install-launchagent.sh first if you haven't."
