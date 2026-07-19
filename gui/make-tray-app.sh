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
# MUST match the BUNDLE_PREFIX used by packaging/{make-app,install-launchagent}.sh.
# The controller derives the server's LaunchAgent label by stripping ".controller"
# from its own bundle id at runtime, so this prefix decides which agent it drives.
BUNDLE_PREFIX="${BUNDLE_PREFIX:-com.clintcan}"
CONTROLLER_ID="$BUNDLE_PREFIX.macrdp.controller"

VERSION="$(grep -m1 '^version' "$REPO_ROOT/Cargo.toml" | cut -d'"' -f2)"
[ -n "$VERSION" ] || { echo "could not read version from Cargo.toml" >&2; exit 1; }

echo "==> macrdpController v$VERSION  (id: $CONTROLLER_ID, identity: $IDENTITY, install: $APP_DIR)"

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
    <key>CFBundleIdentifier</key><string>$CONTROLLER_ID</string>
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

# App icon (optional): packaging/macrdpController.png or packaging/icon.png.
ICON_SRC=""
for c in "$REPO_ROOT/packaging/macrdpController.png" "$REPO_ROOT/packaging/icon.png"; do
    [ -f "$c" ] && { ICON_SRC="$c"; break; }
done
if [ -n "$ICON_SRC" ]; then
    "$REPO_ROOT/packaging/make-icns.sh" "$ICON_SRC" "$STAGE/Contents/Resources/AppIcon.icns"
    /usr/libexec/PlistBuddy -c 'Add :CFBundleIconFile string AppIcon' "$STAGE/Contents/Info.plist" \
        2>/dev/null || /usr/libexec/PlistBuddy -c 'Set :CFBundleIconFile AppIcon' "$STAGE/Contents/Info.plist"
    echo "==> app icon: $(basename "$ICON_SRC")"
fi

# Ad-hoc ("-") can't use a secure timestamp; a real Developer ID must
# (notarization requires it).
if [ "$IDENTITY" = "-" ]; then TS="--timestamp=none"; else TS="--timestamp"; fi

# Optional: embed + activate the macrdp Camera CoreMediaIO system extension
# (camera redirection Phase 3). CAMERA_EXTENSION=1 builds the extension via
# packaging/make-camera-extension.sh, embeds it in Contents/Library/
# SystemExtensions/, and signs THIS controller with the system-extension.install
# entitlement + a provisioning profile so OSSystemExtensionRequest can activate it.
# Unset (the normal controller build) → no extension, no entitlements, unchanged.
CTRL_ENT_ARG=""
if [ "${CAMERA_EXTENSION:-0}" = "1" ]; then
    [ "$IDENTITY" != "-" ] || echo "==> WARNING: ad-hoc camera-extension build won't activate (Developer ID + profiles needed)" >&2
    TEAM_ID="${TEAM_ID:-$(printf '%s' "$IDENTITY" | sed -n 's/.*(\([A-Z0-9]\{10\}\)).*/\1/p')}"
    APP_GROUP="${APP_GROUP:-${TEAM_ID:-TEAMIDXXXX}.$BUNDLE_PREFIX.macrdp}"
    # Build + sign the extension bundle (its own entitlements/profile).
    OUT_DIR="$REPO_ROOT/target" TEAM_ID="${TEAM_ID:-}" APP_GROUP="$APP_GROUP" \
        CODESIGN_IDENTITY="$IDENTITY" BUNDLE_PREFIX="$BUNDLE_PREFIX" \
        "$REPO_ROOT/packaging/make-camera-extension.sh"
    EXT_SRC="$REPO_ROOT/target/macrdp-camera.systemextension"
    [ -d "$EXT_SRC" ] || { echo "extension not built at $EXT_SRC" >&2; exit 1; }
    mkdir -p "$STAGE/Contents/Library/SystemExtensions"
    cp -R "$EXT_SRC" "$STAGE/Contents/Library/SystemExtensions/"
    echo "==> embedded macrdp-camera.systemextension"
    # Controller entitlements (system-extension.install + the shared App Group).
    CTRL_ENT="$REPO_ROOT/target/macrdp-controller.entitlements"
    sed -e "s#__APP_GROUP__#$APP_GROUP#g" \
        "$REPO_ROOT/packaging/macrdp-controller.entitlements" > "$CTRL_ENT"
    CTRL_ENT_ARG="--entitlements $CTRL_ENT"
    # Embed the controller's own provisioning profile (system-extension capability).
    if [ -n "${PROVISION_PROFILE:-}" ]; then
        [ -f "$PROVISION_PROFILE" ] || { echo "PROVISION_PROFILE not found: $PROVISION_PROFILE" >&2; exit 1; }
        cp "$PROVISION_PROFILE" "$STAGE/Contents/embedded.provisionprofile"
        echo "==> embedded controller provisioning profile"
    elif [ "$IDENTITY" != "-" ]; then
        echo "==> WARNING: CAMERA_EXTENSION=1 without PROVISION_PROFILE — activation needs the controller profile" >&2
    fi
fi

echo "==> codesign (hardened runtime, ts: $TS${CTRL_ENT_ARG:+, entitlements})"
codesign --force --options runtime $TS $CTRL_ENT_ARG -s "$IDENTITY" "$STAGE/Contents/MacOS/macrdptray"
# NOTE: no --deep on the sign — the embedded .systemextension is already signed
# with its OWN entitlements; a --deep re-sign would strip them. Signing the outer
# bundle seals the pre-signed extension by reference.
codesign --force --options runtime $TS $CTRL_ENT_ARG -s "$IDENTITY" "$STAGE"
codesign --verify --strict "$STAGE"

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
