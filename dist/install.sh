#!/usr/bin/env bash
# Install macrdp as a per-user launchd agent that auto-starts at login.
#
# What this does:
#   1. Builds the release binary if needed.
#   2. Ad-hoc signs it so TCC grants persist across rebuilds.
#   3. Copies it to ~/.local/bin (override with $MACRDP_BIN_DIR).
#   4. Stores the Mac password in the macOS Keychain under service "macrdp".
#   5. Writes ~/Library/LaunchAgents/com.user.macrdp.plist with the
#      resolved binary path.
#   6. Loads the agent with launchctl.
#
# Re-run after `cargo build --release` to refresh the installed binary.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="${MACRDP_BIN_DIR:-$HOME/.local/bin}"
BIN_PATH="$BIN_DIR/macrdp"
PLIST_PATH="$HOME/Library/LaunchAgents/com.user.macrdp.plist"
LABEL="com.user.macrdp"
KEYCHAIN_SERVICE="macrdp"

echo "==> Building release binary"
(cd "$REPO_ROOT" && cargo build --release)

echo "==> Ad-hoc signing"
codesign -s - --force "$REPO_ROOT/target/release/macrdp"

echo "==> Installing to $BIN_PATH"
mkdir -p "$BIN_DIR"
cp "$REPO_ROOT/target/release/macrdp" "$BIN_PATH"

# Keychain entry. add-generic-password -U updates if it exists.
if ! security find-generic-password -s "$KEYCHAIN_SERVICE" -a "$USER" >/dev/null 2>&1; then
    echo "==> Storing Mac password in Keychain (service=$KEYCHAIN_SERVICE, account=$USER)"
    echo -n "Mac password for $USER: "
    read -rs PW
    echo
    security add-generic-password -U -s "$KEYCHAIN_SERVICE" -a "$USER" -w "$PW"
    unset PW
else
    echo "==> Keychain entry already exists; leaving it alone"
fi

echo "==> Writing $PLIST_PATH"
sed "s|BINARY_PATH|$BIN_PATH|g" "$REPO_ROOT/dist/com.user.macrdp.plist.template" > "$PLIST_PATH"

# Unload first in case it was already loaded.
launchctl bootout "gui/$UID/$LABEL" 2>/dev/null || true
echo "==> Loading agent"
launchctl bootstrap "gui/$UID" "$PLIST_PATH"

cat <<EOF

Installed. The agent will start automatically at login.

Verify:   launchctl print gui/$UID/$LABEL | head
Logs:     /tmp/macrdp.out.log  /tmp/macrdp.err.log
Stop:     launchctl bootout gui/$UID/$LABEL
Restart:  launchctl kickstart -k gui/$UID/$LABEL

First-run TCC prompts (Screen Recording + Accessibility) will fire
against $BIN_PATH; grant them in System Settings → Privacy & Security
and run \`launchctl kickstart\` again.
EOF
