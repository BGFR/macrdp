# macrdp Camera — building & activating the CoreMediaIO system extension (Phase 3)

This is the operator runbook for camera-redirection **Phase 3**: presenting the
redirected webcam (decoded by Phases 1+2) as a selectable macOS camera ("macrdp
Camera") via a **CoreMediaIO Camera system extension**. It covers the one-time
Apple-portal setup, the build, and activation.

**Phase 3a status:** the extension + activation code + packaging are built and
**verified to assemble/sign locally**. What remains — and what this runbook drives
— is the **activation spike**: proving a *hand-assembled* (no-Xcode) `.systemextension`
activates on a real machine with Developer-ID signing + provisioning profiles. Do
this before building the real frame feed (Phase 3b).

## What activates what

- **`macrdpController.app`** (the menu-bar app, `gui/`) embeds the extension at
  `Contents/Library/SystemExtensions/macrdp-camera.systemextension` and activates it
  via `OSSystemExtensionRequest` (menu → **Enable macrdp Camera…**). It needs the
  `com.apple.developer.system-extension.install` entitlement + its own provisioning
  profile. It is **not** sandboxed.
- **`macrdp.app`** (the Rust server) is unchanged — in Phase 3b it becomes a CoreMediaIO
  *client* that feeds the extension's sink stream. It needs neither the extension nor
  any new entitlement.
- The **extension** (`macrdpcamera`, a `SYSX` bundle) is app-sandboxed, in the shared
  App Group, and presents the virtual camera. In 3a it emits a static test pattern.

## Hard requirements (silent-failure traps — get these exactly right)

| Thing | Value |
|---|---|
| Extension bundle id | `com.clintcan.macrdp.controller.camera` — MUST be a child of the controller app id |
| Controller app id | `com.clintcan.macrdp.controller` |
| App Group | `<TeamID>.com.clintcan.macrdp` (e.g. `QGLA89KHM7.com.clintcan.macrdp`) — on the **extension** only |
| `CMIOExtensionMachServiceName` | **byte-identical to the App Group id** |
| Extension entitlements | `app-sandbox`, `application-groups` (NOT `device.camera`) |
| Controller entitlements | `com.apple.developer.system-extension.install` only (unsandboxed) |
| Install location | **`/Applications`** proper — NOT `~/Applications`, NOT run-from-DMG (else `OSSystemExtensionErrorUnsupportedParentBundleLocation`) |
| Distribution | Developer ID Application + **notarized** (MAS not required; notarization IS, with SIP on) |

## One-time Apple Developer portal setup (self-serviceable — no Apple approval)

Unlike the USB host-controller entitlement (which needed a Feedback-Assistant grant),
everything here is self-serviceable in *Certificates, Identifiers & Profiles*.

1. **App Group** → Identifiers → **App Groups** → register `group`-free id
   `com.clintcan.macrdp` (the portal stores it as `<TeamID>.com.clintcan.macrdp`).
2. **Extension App ID** → Identifiers → App IDs → `com.clintcan.macrdp.controller.camera`
   → enable **App Groups**, assign the group above.
3. **Controller App ID** → `com.clintcan.macrdp.controller` → enable the
   **System Extension** capability (this is what authorizes
   `com.apple.developer.system-extension.install`).
4. **Provisioning profiles** (type: **Developer ID**, since this ships outside the MAS):
   - one for `com.clintcan.macrdp.controller` → download as e.g.
     `macrdp-controller.provisionprofile`
   - one for `com.clintcan.macrdp.controller.camera` → e.g. `macrdp-camera.provisionprofile`
     (the extension's app-group entitlement wants a profile; the controller's restricted
     `system-extension.install` definitely does).

## Build

From the repo root, with the Developer ID identity + notary profile already set up
(same as the entitled USB build — see `reference_developer_id_signing`):

```bash
CODESIGN_IDENTITY="Developer ID Application: Clint Christopher Canada (QGLA89KHM7)" \
NOTARIZE=1 NOTARY_PROFILE=macrdp-notary \
CAMERA_EXTENSION=1 \
PROVISION_PROFILE=/path/to/macrdp-controller.provisionprofile \
CAMERA_PROVISION_PROFILE=/path/to/macrdp-camera.provisionprofile \
APP_DIR=/Applications \
gui/make-tray-app.sh
```

`TEAM_ID` and `APP_GROUP` are derived automatically (`QGLA89KHM7`,
`QGLA89KHM7.com.clintcan.macrdp`); override `APP_GROUP=` if you registered a different
group. This builds the extension (`make-camera-extension.sh`), embeds + signs it,
signs the controller with the entitlement + profile, notarizes the whole app, and
installs to `/Applications/macrdpController.app`.

## Activate

1. During development, enable dev mode so the OS skips the version check between
   rebuilds: `systemextensionsctl developer on` (reboot if it doesn't take effect;
   community reports vary).
2. Launch `/Applications/macrdpController.app` → menu → **Enable macrdp Camera…**.
3. macOS will block it pending approval: **System Settings → Privacy & Security**,
   scroll to *"System software from Clint Christopher Canada was blocked"* → **Allow**
   (the menu offers an "Open Privacy & Security" button). You may need to re-run
   **Enable macrdp Camera…** after approving.
4. Check state: `systemextensionsctl list` → `activated enabled` for
   `com.clintcan.macrdp.controller.camera`.

## Verify (Phases 3a + 3b + 3c GREEN, one run)

1. **3a** — Open **Photo Booth** (or QuickTime → New Movie → camera dropdown, or Zoom
   video settings) → pick **macrdp Camera** → you should see the **sweeping-white-stripe
   test pattern**. That confirms the signing/activation/CMIO-wiring path works.
2. **3b + 3c** — With the extension active, connect an RDP client, redirect a webcam
   (Video capture devices) with `--enable-camera-redirection` on, and the **live webcam
   should replace the test pattern** in Photo Booth. This confirms the sink feed +
   the `420v` format + the producer authentication all work end-to-end.

**The producer must be the signed `macrdp.app`.** Phase 3c authenticates the sink
producer — the extension accepts frames only from a binary signed as
`com.clintcan.macrdp` under Team `QGLA89KHM7`. A plain unsigned `cargo build` macrdp
feeding the sink is **rejected** (you'd see the test pattern, not the webcam, and an
`sink: REJECTED producer` line in the extension log). Run the Developer-ID
`macrdp.app`. Watch the extension log to see which auth path fired (and whether
`SecCode` is available in the extension sandbox — it falls back to the `signingID`
match if not):

```
log stream --predicate 'subsystem == "com.clintcan.macrdp.camera"'
```

## Dev iteration & teardown

- **Replacing a running extension usually needs a reboot** (a running, in-use camera
  extension can't be hot-swapped in one session — Apple's own guidance). Dev mode
  removes the *version-bump* requirement but not necessarily the reboot. Budget for a
  reboot per meaningful reinstall.
- **Uninstall:** deactivate from the app (a `deactivationRequest` — wire a menu item if
  needed) or delete `macrdpController.app` (the extension auto-uninstalls when its host
  app is removed). `systemextensionsctl reset` nukes ALL extensions and typically needs
  SIP disabled — last resort only.
- **Logs:** `log stream --predicate 'subsystem == "com.clintcan.macrdp.camera"'` for the
  extension; `log show --predicate 'process == "sysextd"'` for activation failures.

## Hand-assembly gotchas (RETIRED risk — LIVE-VERIFIED 2026-07-20)

A hand-assembled (no-`xcodebuild`) CMIO camera extension **does activate** — verified
end-to-end on real hardware: signed → notarized → `OSSystemExtensionRequest` →
approved → the test pattern rendered in Photo Booth, activating right alongside OBS's
extension. The Xcode fallback was **not** needed. But two things Xcode injects
silently are load-bearing and cost real debugging (both invisible to
`codesign --verify`, which passes without them):

1. **The bundle FILENAME must equal the `CFBundleIdentifier`** —
   `com.clintcan.macrdp.controller.camera.systemextension`, not an arbitrary name.
   `OSSystemExtensionRequest`'s bundle scan relies on it: a mismatched name fails with
   **"unable to find any matched extension"** (`extensionNotFound`) — the request never
   even reaches `sysextd`. (`make-camera-extension.sh` now names it `<id>.systemextension`.)
2. **`CFBundleSupportedPlatforms = ["MacOSX"]` is required** in the extension
   `Info.plist` — without it the OS doesn't treat the bundle as an installable macOS
   system extension. (Now in `camera-Info.plist`.)

Diagnosis method that worked: compare the failing bundle against a **known-good CMIO
extension already on the machine** (OBS Virtual Camera —
`/Applications/OBS.app/Contents/Library/SystemExtensions/*.systemextension`) — a
`plutil -p` Info.plist diff + a bundle-tree diff surfaced both gaps. The error
progression was the map: `extensionNotFound` (filename) → `code signature invalid`
(not notarized — sysextd validates far stricter than `codesign`; a Developer-ID sysext
MUST be notarized to activate with SIP on) → `needs user approval` (success).
