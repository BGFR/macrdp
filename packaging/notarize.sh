#!/bin/bash
# Notarize + staple a signed .app via Apple's notary service.
#
# Prereqs:
#   * The app must already be signed with a Developer ID Application identity
#     AND a secure timestamp + hardened runtime (make-app.sh / make-tray-app.sh
#     do this automatically when CODESIGN_IDENTITY is not "-").
#   * A notarytool keychain profile created once with:
#       xcrun notarytool store-credentials "<profile>" \
#         --apple-id you@example.com --team-id TEAMID --password <app-specific-pw>
#
# Usage:
#   NOTARY_PROFILE="<profile>" packaging/notarize.sh /path/to/Foo.app
set -euo pipefail

APP="${1:?usage: notarize.sh <app-bundle>}"
: "${NOTARY_PROFILE:?set NOTARY_PROFILE (see: xcrun notarytool store-credentials)}"
[ -d "$APP" ] || { echo "not a bundle: $APP" >&2; exit 1; }

WORK="$(mktemp -d)"
ZIP="$WORK/$(basename "$APP").zip"
trap 'rm -rf "$WORK"' EXIT

echo "==> zipping $(basename "$APP") for notarization"
/usr/bin/ditto -c -k --keepParent "$APP" "$ZIP"

echo "==> submitting to Apple notary service (waits for the verdict)"
xcrun notarytool submit "$ZIP" --keychain-profile "$NOTARY_PROFILE" --wait

echo "==> stapling the ticket onto the bundle"
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"

echo "==> Gatekeeper assessment"
spctl -a -vvv --type exec "$APP" || true

echo "notarized + stapled: $APP"
