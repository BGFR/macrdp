#!/bin/bash
# Build macrdp.app — a stably-signed bundle with the binary as a co-signed
# helper at a fixed path, so the Screen Recording / Accessibility TCC grants
# survive rebuilds. Designed for personal use, but the bundle layout is also
# the foundation a future menu-bar GUI controller would spawn.
#
# Usage:
#   packaging/make-app.sh                 # build + bundle + ad-hoc sign + install
#
# Env overrides:
#   APP_DIR=/Applications                 # where to install (default /Applications)
#   CODESIGN_IDENTITY="-"                 # "-" = ad-hoc; or a Developer ID name
#   SKIP_BUILD=1                          # reuse an existing release binary
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG_DIR="$REPO_ROOT/packaging"
APP_DIR="${APP_DIR:-/Applications}"
IDENTITY="${CODESIGN_IDENTITY:--}"

VERSION="$(grep -m1 '^version' "$REPO_ROOT/Cargo.toml" | cut -d'"' -f2)"
[ -n "$VERSION" ] || { echo "could not read version from Cargo.toml" >&2; exit 1; }

echo "==> macrdp.app v$VERSION  (identity: $IDENTITY, install: $APP_DIR)"

# 1. Build the release binary (native target).
if [ "${SKIP_BUILD:-0}" != "1" ]; then
    echo "==> cargo build --release"
    ( cd "$REPO_ROOT" && cargo build --release )
fi
BIN="$REPO_ROOT/target/release/macrdp"
[ -x "$BIN" ] || { echo "missing release binary at $BIN (unset SKIP_BUILD?)" >&2; exit 1; }

# 2. Assemble the bundle in a staging dir under target/ (already gitignored;
#    dist/ holds the tracked install scripts, not build output).
STAGE="$REPO_ROOT/target/macrdp.app"
echo "==> staging bundle at $STAGE"
rm -rf "$STAGE"
mkdir -p "$STAGE/Contents/MacOS" "$STAGE/Contents/Resources"

sed "s/__VERSION__/$VERSION/g" "$PKG_DIR/Info.plist" > "$STAGE/Contents/Info.plist"
cp "$BIN" "$STAGE/Contents/MacOS/macrdp"
# The wrapper goes in Resources/ (sealed as a resource by the bundle signature),
# NOT MacOS/ — a script in MacOS/ is treated as nested code that needs its own
# signature and breaks bundle signing.
cp "$PKG_DIR/macrdp-launch" "$STAGE/Contents/Resources/macrdp-launch"
chmod +x "$STAGE/Contents/MacOS/macrdp" "$STAGE/Contents/Resources/macrdp-launch"

# 3. Sign the Mach-O executable, then the bundle (which seals Info.plist + the
#    Resources, including the wrapper script). Ad-hoc ("-") can't use a secure
#    timestamp; a real Developer ID must (notarization requires it).
if [ "$IDENTITY" = "-" ]; then TS="--timestamp=none"; else TS="--timestamp"; fi
echo "==> codesign (hardened runtime, ts: $TS)"
codesign --force --options runtime $TS -s "$IDENTITY" "$STAGE/Contents/MacOS/macrdp"
codesign --force --options runtime $TS -s "$IDENTITY" "$STAGE"
codesign --verify --deep --strict "$STAGE"

# 3b. Optional notarization (NOTARIZE=1, real Developer ID + NOTARY_PROFILE).
#     Done on the staged app so the stapled ticket travels with the install copy.
if [ "${NOTARIZE:-0}" = "1" ]; then
    [ "$IDENTITY" != "-" ] || { echo "NOTARIZE=1 needs a real CODESIGN_IDENTITY (not ad-hoc)" >&2; exit 1; }
    "$PKG_DIR/notarize.sh" "$STAGE"
fi

# 4. Install to the stable path. cp -R preserves the signature.
echo "==> installing to $APP_DIR/macrdp.app"
if ! mkdir -p "$APP_DIR" 2>/dev/null || [ ! -w "$APP_DIR" ]; then
    echo "    $APP_DIR is not writable — re-run with sudo, or set APP_DIR=\$HOME/Applications" >&2
    exit 1
fi
rm -rf "$APP_DIR/macrdp.app"
cp -R "$STAGE" "$APP_DIR/macrdp.app"
codesign --verify --strict "$APP_DIR/macrdp.app"

echo
echo "Done. Installed: $APP_DIR/macrdp.app"
codesign -dv "$APP_DIR/macrdp.app" 2>&1 | sed 's/^/    /'
echo
echo "Next:"
echo "  1. Store the password once:"
echo "       security add-generic-password -s macrdp -a \"\$(id -un)\" -w 'YOUR_PASSWORD'"
echo "  2. Install + load the LaunchAgent:"
echo "       APP_DIR=\"$APP_DIR\" packaging/install-launchagent.sh"
echo "  3. Grant Screen Recording + Accessibility to macrdp.app when prompted"
echo "     (System Settings -> Privacy & Security)."
