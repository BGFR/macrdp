#!/bin/bash
# Install + (re)load the macrdp LaunchAgent for the current user.
#
# Seeds ~/Library/Application Support/macrdp/config.env from the example on
# first run, renders the LaunchAgent plist from the template, and bootstraps it.
#
# Env overrides:
#   APP_DIR=/Applications     # where macrdp.app was installed (default /Applications)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG_DIR="$REPO_ROOT/packaging"
APP_DIR="${APP_DIR:-/Applications}"
LABEL="com.clintcan.macrdp"
UID_NUM="$(id -u)"

APP="$APP_DIR/macrdp.app"
[ -d "$APP" ] || { echo "macrdp.app not found at $APP — run packaging/make-app.sh first" >&2; exit 1; }

# 1. Seed config.env if absent.
SUPPORT="$HOME/Library/Application Support/macrdp"
mkdir -p "$SUPPORT" "$HOME/Library/Logs" "$HOME/Library/LaunchAgents"
CONFIG="$SUPPORT/config.env"
if [ ! -f "$CONFIG" ]; then
    cp "$PKG_DIR/config.env.example" "$CONFIG"
    echo "==> seeded $CONFIG (edit to taste)"
else
    echo "==> keeping existing $CONFIG"
fi

# 2. Render the LaunchAgent plist from the template.
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
sed -e "s#__APP_DIR__#$APP_DIR#g" -e "s#__HOME__#$HOME#g" \
    "$PKG_DIR/$LABEL.plist" > "$PLIST"
echo "==> wrote $PLIST"

# 3. (Re)bootstrap the agent.
launchctl bootout "gui/$UID_NUM/$LABEL" 2>/dev/null || true
launchctl bootstrap "gui/$UID_NUM" "$PLIST"
launchctl enable "gui/$UID_NUM/$LABEL"
launchctl kickstart -k "gui/$UID_NUM/$LABEL"

echo
echo "Loaded $LABEL."
echo "  status:  launchctl print gui/$UID_NUM/$LABEL | grep -E 'state|pid'"
echo "  logs:    tail -f ~/Library/Logs/macrdp.log"
echo "  apply config change:  launchctl kickstart -k gui/$UID_NUM/$LABEL"
echo "  stop:    launchctl bootout gui/$UID_NUM/$LABEL"
