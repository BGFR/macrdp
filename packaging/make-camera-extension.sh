#!/bin/bash
# Build + assemble the macrdp Camera CoreMediaIO **system extension**
# (camera redirection Phase 3): swift build the macrdpcamera executable, wrap it in
# a hand-assembled `macrdp-camera.systemextension` bundle, substitute the Info.plist
# placeholders, and code-sign it (executable then bundle) with the camera
# entitlements + provisioning profile.
#
# This is the no-Xcode analogue of Xcode's "System Extension" product type — the
# same hand-assembly approach make-app.sh uses for macrdp.app. The resulting
# `.systemextension` is embedded into macrdpController.app/Contents/Library/
# SystemExtensions/ by gui/make-tray-app.sh (CAMERA_EXTENSION=1), which then
# activates it via OSSystemExtensionRequest.
#
# Env:
#   CODESIGN_IDENTITY   Developer ID Application name (REQUIRED for a real build;
#                       "-" ad-hoc only assembles/structurally-signs for local
#                       inspection — it will NOT activate).
#   TEAM_ID             Apple Team ID (e.g. QGLA89KHM7). Required for the App Group
#                       / MachServiceName unless derivable from the identity.
#   APP_GROUP           App Group id (default: <TEAM_ID>.<BUNDLE_PREFIX>.macrdp).
#   CAMERA_PROVISION_PROFILE   .provisionprofile for the extension App ID
#                       (com.clintcan.macrdp.camera) with the App Group. Embedded
#                       into the bundle. Required for a Developer-ID activatable build.
#   BUNDLE_PREFIX       reverse-DNS prefix (default com.clintcan).
#   OUT_DIR             where to stage the bundle (default target/).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG_DIR="$REPO_ROOT/packaging"
GUI_DIR="$REPO_ROOT/gui"
BUNDLE_PREFIX="${BUNDLE_PREFIX:-com.clintcan}"
CAMERA_ID="$BUNDLE_PREFIX.macrdp.camera"
IDENTITY="${CODESIGN_IDENTITY:--}"
OUT_DIR="${OUT_DIR:-$REPO_ROOT/target}"

VERSION="$(grep -m1 '^version' "$REPO_ROOT/Cargo.toml" | cut -d'"' -f2)"
[ -n "$VERSION" ] || { echo "could not read version from Cargo.toml" >&2; exit 1; }

# Team ID: explicit, else parse from the signing identity's cert (…(TEAMID)).
if [ -z "${TEAM_ID:-}" ] && [ "$IDENTITY" != "-" ]; then
    TEAM_ID="$(printf '%s' "$IDENTITY" | sed -n 's/.*(\([A-Z0-9]\{10\}\)).*/\1/p')"
fi
if [ -z "${TEAM_ID:-}" ]; then
    if [ "$IDENTITY" = "-" ]; then
        TEAM_ID="TEAMIDXXXX"   # ad-hoc placeholder; the bundle won't activate anyway
        echo "==> ad-hoc build: using placeholder TEAM_ID=$TEAM_ID (won't activate)"
    else
        echo "TEAM_ID not set and not parseable from identity '$IDENTITY'" >&2; exit 1
    fi
fi
APP_GROUP="${APP_GROUP:-$TEAM_ID.$BUNDLE_PREFIX.macrdp}"
MACH_SVC="$APP_GROUP.camera"

echo "==> macrdp-camera.systemextension v$VERSION"
echo "    id=$CAMERA_ID  team=$TEAM_ID  group=$APP_GROUP  mach=$MACH_SVC  identity=$IDENTITY"

# 1. Build the extension executable.
echo "==> swift build -c release --product macrdpcamera"
( cd "$GUI_DIR" && swift build -c release --product macrdpcamera )
BIN="$GUI_DIR/.build/release/macrdpcamera"
[ -x "$BIN" ] || { echo "build produced no binary at $BIN" >&2; exit 1; }

# 2. Assemble the .systemextension bundle.
EXT="$OUT_DIR/macrdp-camera.systemextension"
echo "==> staging $EXT"
rm -rf "$EXT"
mkdir -p "$EXT/Contents/MacOS"
sed -e "s/__VERSION__/$VERSION/g" \
    -e "s#__CAMERA_ID__#$CAMERA_ID#g" \
    -e "s#__MACH_SVC__#$MACH_SVC#g" \
    "$PKG_DIR/camera-Info.plist" > "$EXT/Contents/Info.plist"
cp "$BIN" "$EXT/Contents/MacOS/macrdpcamera"
chmod +x "$EXT/Contents/MacOS/macrdpcamera"

# Substitute the App Group into the entitlements (temp copy).
ENT="$OUT_DIR/camera-extension.entitlements"
sed -e "s#__APP_GROUP__#$APP_GROUP#g" "$PKG_DIR/camera-extension.entitlements" > "$ENT"

# Embed the extension's provisioning profile (Developer-ID system extensions
# require a profile even outside the App Store).
if [ -n "${CAMERA_PROVISION_PROFILE:-}" ]; then
    [ -f "$CAMERA_PROVISION_PROFILE" ] || { echo "CAMERA_PROVISION_PROFILE not found: $CAMERA_PROVISION_PROFILE" >&2; exit 1; }
    cp "$CAMERA_PROVISION_PROFILE" "$EXT/Contents/embedded.provisionprofile"
    echo "==> embedded extension provisioning profile"
elif [ "$IDENTITY" != "-" ]; then
    echo "==> WARNING: no CAMERA_PROVISION_PROFILE — a Developer-ID system extension needs one to activate" >&2
fi

# 3. Sign: executable (with entitlements + runtime), then the bundle.
if [ "$IDENTITY" = "-" ]; then TS="--timestamp=none"; else TS="--timestamp"; fi
echo "==> codesign (hardened runtime, entitlements)"
codesign --force --options runtime $TS --entitlements "$ENT" -s "$IDENTITY" "$EXT/Contents/MacOS/macrdpcamera"
codesign --force --options runtime $TS --entitlements "$ENT" -s "$IDENTITY" "$EXT"
codesign --verify --strict "$EXT"
echo
echo "Built: $EXT"
codesign -dv --entitlements - "$EXT" 2>&1 | sed 's/^/    /' | head -20
echo
echo "Embed it with:  CAMERA_EXTENSION=1 CODESIGN_IDENTITY=\"$IDENTITY\" gui/make-tray-app.sh"
