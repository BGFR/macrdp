#!/bin/bash
# Notarize + staple a signed artifact (.app, .dmg, or .pkg) via Apple's notary
# service.
#
# Prereqs:
#   * Signed with a Developer ID identity + secure timestamp + hardened runtime
#     (make-app.sh / make-tray-app.sh / make-dmg.sh do this when CODESIGN_IDENTITY
#     is not "-").
#   * A notarytool keychain profile created once with:
#       xcrun notarytool store-credentials "<profile>" \
#         --apple-id you@example.com --team-id TEAMID --password <app-specific-pw>
#
# Usage:
#   NOTARY_PROFILE="<profile>" packaging/notarize.sh /path/to/Foo.{app,dmg,pkg}
set -euo pipefail

TARGET="${1:?usage: notarize.sh <app|dmg|pkg>}"
: "${NOTARY_PROFILE:?set NOTARY_PROFILE (see: xcrun notarytool store-credentials)}"
[ -e "$TARGET" ] || { echo "no such artifact: $TARGET" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# notarytool wants a single file. A .app is a directory, so zip it; .dmg/.pkg
# submit directly.
case "$TARGET" in
    *.app)
        SUBMIT="$WORK/$(basename "$TARGET").zip"
        echo "==> zipping $(basename "$TARGET") for notarization"
        /usr/bin/ditto -c -k --keepParent "$TARGET" "$SUBMIT"
        SPCTL_TYPE="exec"
        ;;
    *)
        SUBMIT="$TARGET"
        SPCTL_TYPE="open"
        ;;
esac

echo "==> submitting to Apple notary service (waits for the verdict)"
xcrun notarytool submit "$SUBMIT" --keychain-profile "$NOTARY_PROFILE" --wait

echo "==> stapling the ticket onto $(basename "$TARGET")"
xcrun stapler staple "$TARGET"
xcrun stapler validate "$TARGET"

echo "==> Gatekeeper assessment"
if [ "$SPCTL_TYPE" = "open" ]; then
    # A DMG/pkg needs an explicit assessment context, or spctl reports
    # "Insufficient Context" even when notarization + stapling succeeded.
    spctl -a -vvv --type open --context context:primary-signature "$TARGET" || true
else
    spctl -a -vvv --type exec "$TARGET" || true
fi

echo "notarized + stapled: $TARGET"
