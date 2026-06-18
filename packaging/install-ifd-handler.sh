#!/bin/bash
# Install (or uninstall) macrdp's PC/SC IFD handler into the system smart-card
# driver directory. Smart-card redirection (--enable-smartcard-redirection)
# needs this: it's the macOS-side virtual reader that macrdp bridges to the
# client's redirected smart card.
#
# The driver dir is root-owned, so this needs admin rights — you'll get a single
# GUI password prompt (no manual sudo). The handler bundle ships embedded in
# macrdp.app (Contents/Resources/ifd-macrdp.bundle); this finds it there, or
# builds one from the repo if run from a checkout.
#
# Usage:
#   packaging/install-ifd-handler.sh                       # install
#   packaging/install-ifd-handler.sh --uninstall           # remove
#   IFD_VID=0x2174 IFD_PID=0x2100 packaging/install-ifd-handler.sh   # set USB trigger
#
# Env:
#   APP_DIR=/Applications   where macrdp.app is installed (to find the bundle)
#   IFD_VID / IFD_PID       VID/PID of the USB device that triggers the driver
#                           load (macOS loads IFD drivers only on a matching USB
#                           hotplug). Defaults to the bundle's baked-in values.
set -euo pipefail

DRIVERS="/usr/local/libexec/SmartCardServices/drivers"
DEST="$DRIVERS/ifd-macrdp.bundle"
SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SELF_DIR/.." && pwd)"
APP_DIR="${APP_DIR:-/Applications}"

# Run a shell command as root via one GUI auth prompt. Paths are single-quoted
# by callers; AppleScript string is double-quoted, so no escaping clash.
run_admin() {
    /usr/bin/osascript -e "do shell script \"$1\" with administrator privileges" >/dev/null
}

# Restart the PC/SC reader daemon so it drops any cached driver dylib (slotd
# keeps loaded IFD drivers in memory for its whole lifetime, so just replacing
# the file on disk isn't enough — it must be restarted). launchd respawns it.
# MUST be SIGKILL (-9): slotd ignores SIGTERM, so a plain `pkill`/`killall`
# leaves it running with the stale dylib. `-f` matches the full path, since the
# process `comm` is truncated past 15 chars so `killall com.apple.ifdreader`
# never matches. Best-effort (`|| true`) so it doesn't abort the install chain,
# but it does NOT mask a failed copy — that's a separate `&&`-guarded step.
RESTART_SLOTD="{ pkill -9 -f com.apple.ifdreader 2>/dev/null || true; }"

if [ "${1:-}" = "--uninstall" ]; then
    echo "==> Removing $DEST (admin required)"
    run_admin "rm -rf '$DEST'; $RESTART_SLOTD"
    echo "Removed. (slotd restarted; the reader is gone on its next load.)"
    exit 0
fi

# 1. Locate the handler bundle: embedded next to this script (DMG/app install),
#    then the installed app, then a staged build, else build one from the repo.
SRC=""
for c in "$SELF_DIR/ifd-macrdp.bundle" \
         "$APP_DIR/macrdp.app/Contents/Resources/ifd-macrdp.bundle" \
         "$REPO_ROOT/target/macrdp.app/Contents/Resources/ifd-macrdp.bundle"; do
    [ -d "$c" ] && { SRC="$c"; break; }
done
if [ -z "$SRC" ]; then
    [ -f "$REPO_ROOT/ifd-handler/Cargo.toml" ] || {
        echo "No embedded ifd-macrdp.bundle found and no repo to build from." >&2
        echo "Install macrdp.app first (it embeds the bundle), or run from a checkout." >&2
        exit 1
    }
    echo "==> No embedded bundle found; building from ifd-handler/"
    ( cd "$REPO_ROOT" && cargo build --release --manifest-path ifd-handler/Cargo.toml )
    DY="$REPO_ROOT/ifd-handler/target/release/libifd_macrdp.dylib"
    [ -f "$DY" ] || { echo "build failed: $DY missing" >&2; exit 1; }
    VERSION="$(grep -m1 '^version' "$REPO_ROOT/Cargo.toml" | cut -d'"' -f2)"
    SRC="$REPO_ROOT/target/ifd-macrdp.bundle"
    rm -rf "$SRC"; mkdir -p "$SRC/Contents/MacOS"
    sed -e "s/__VERSION__/$VERSION/g" -e "s#__BUNDLE_ID__#com.clintcan.macrdp.ifd#g" \
        "$REPO_ROOT/packaging/ifd-Info.plist" > "$SRC/Contents/Info.plist"
    cp "$DY" "$SRC/Contents/MacOS/libifd_macrdp.dylib"
    codesign -s - --force "$SRC/Contents/MacOS/libifd_macrdp.dylib" "$SRC"
fi
echo "==> Source bundle: $SRC"

# 2. Optionally rebind the USB trigger VID/PID (patch a temp copy + re-sign, so
#    we don't mutate the signed, read-only app resource).
if [ -n "${IFD_VID:-}" ] || [ -n "${IFD_PID:-}" ]; then
    TMP_PARENT="$(mktemp -d)"
    SRC_PATCHED="$TMP_PARENT/ifd-macrdp.bundle"
    cp -R "$SRC" "$SRC_PATCHED"
    PL="$SRC_PATCHED/Contents/Info.plist"
    [ -n "${IFD_VID:-}" ] && /usr/libexec/PlistBuddy -c "Set :ifdVendorID:0 ${IFD_VID}" "$PL"
    [ -n "${IFD_PID:-}" ] && /usr/libexec/PlistBuddy -c "Set :ifdProductID:0 ${IFD_PID}" "$PL"
    codesign -s - --force "$SRC_PATCHED/Contents/MacOS/libifd_macrdp.dylib" "$SRC_PATCHED"
    SRC="$SRC_PATCHED"
    echo "==> USB trigger rebound: VID=${IFD_VID:-(unchanged)} PID=${IFD_PID:-(unchanged)}"
fi

# 3. Hint: which USB devices are attached (the trigger must match one of these).
echo "==> Attached USB devices (the driver loads on a hotplug matching its VID/PID):"
ioreg -r -c IOUSBHostDevice 2>/dev/null \
    | grep -E '"(USB Vendor Name|idVendor|USB Product Name|idProduct)"' \
    | sed 's/^[[:space:]]*/      /' | head -40 || true

# 4. Stage the bundle into /tmp first. The repo / app may live under ~/Documents
#    (or another TCC-protected location), and the root `cp` spawned by osascript
#    can't read those — it would silently create an empty bundle. /tmp is always
#    readable by root with no TCC gate.
STAGE_DIR="$(mktemp -d)"
STAGED="$STAGE_DIR/ifd-macrdp.bundle"
cp -R "$SRC" "$STAGED"

# 5. Install (privileged): copy in, fix ownership, restart slotd. The copy/chown
#    are &&-guarded (a failure aborts and surfaces via osascript's nonzero exit —
#    NOT masked); only the slotd restart is best-effort.
echo "==> Installing to $DEST (admin required)"
run_admin "mkdir -p '$DRIVERS' && rm -rf '$DEST' && cp -R '$STAGED' '$DEST' && chown -R root:wheel '$DEST' && $RESTART_SLOTD"
rm -rf "$STAGE_DIR"

# Verify the copy actually landed (osascript can mask some failures).
[ -f "$DEST/Contents/MacOS/libifd_macrdp.dylib" ] || {
    echo "ERROR: install did not land $DEST/Contents/MacOS/libifd_macrdp.dylib" >&2
    exit 1
}

cat <<EOF

Installed: $DEST

To use smart-card redirection:
  1. Run macrdp with --enable-smartcard-redirection (or set
     ENABLE_SMARTCARD_REDIRECTION=1 in config.env) and have the client redirect
     its reader (mstsc: Local Resources -> More -> Smart cards; FreeRDP: /smartcard).
  2. Plug in the trigger USB device (VID/PID in the bundle's Info.plist) — macOS
     only loads IFD drivers on a USB hotplug. Unplug/replug it now so slotd loads
     the freshly-installed driver.

Verify the reader registered:  system_profiler SPSmartCardsDataType
Uninstall:                     packaging/install-ifd-handler.sh --uninstall
EOF
