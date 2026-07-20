# macrdp Camera — building & activating the CoreMediaIO system extension (Phase 3)

This is the operator runbook for camera-redirection **Phase 3**: presenting the
redirected webcam (decoded by Phases 1+2) as a selectable macOS camera ("macrdp
Camera") via a **CoreMediaIO Camera system extension**. It covers the one-time
Apple-portal setup, the build, and activation.

**Status: Phase 3 is COMPLETE and LIVE-VERIFIED (2026-07-20).** A hand-assembled
(no-Xcode) `.systemextension` activates on real hardware, and a client webcam
redirected over MS-RDPECAM presents as a live macOS camera in Photo Booth. Before
changing anything here, read **"The four silent failure modes"** at the bottom — each
one fails with no error and cost real debugging time.

## What activates what

- **`macrdpController.app`** (the menu-bar app, `gui/`) embeds the extension at
  `Contents/Library/SystemExtensions/com.clintcan.macrdp.controller.camera.systemextension`
  (the filename MUST equal the bundle id) and activates it
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

## Verify (Phases 3a + 3b + 3c — ALL LIVE-VERIFIED GREEN 2026-07-20)

1. **3a** — Open **Photo Booth** (or QuickTime → New Movie → camera dropdown, or Zoom
   video settings) → pick **macrdp Camera** → you should see the **sweeping-white-stripe
   test pattern**. That confirms the signing/activation/CMIO-wiring path works.
2. **3b + 3c** — With the extension active, connect an RDP client, redirect a webcam
   (Video capture devices) with `--enable-camera-redirection` on, and the **live webcam
   should replace the test pattern** in Photo Booth. This confirms the sink feed +
   the `420v` format + the producer authentication all work end-to-end.

**Producer authentication is NOT enforced** — the extension accepts any client that
starts the sink. This is a deliberate, documented limitation: CMIO exposes no
trustworthy client identity (`signingID` is literally `"unknown"`, and `SecCode` can't
evaluate an external process from the extension's sandbox), so Apple's own sample and
SinkCam leave the hook an unconditional `return true` too. See failure mode #2 below —
an auth check here silently breaks the feed.

**Watching the extension requires `sudo`** (it runs as the `_cmiodalassistants` role
account; without `sudo` you see nothing at all, which looks like "it isn't logging"):

```
sudo log show --last 5m --info --debug --predicate 'subsystem == "com.clintcan.macrdp.camera"' --style compact
```

The quickest health check is actually on the macrdp side — it logs the sink feed every
~90 frames: `dropped_full=0` with `enqueued` climbing means the extension is draining
(working); `enqueued` frozen at the queue capacity means it is **not**.

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

## The four silent failure modes (all LIVE-DEBUGGED 2026-07-20 — read before touching this)

Every one of these fails **silently** — no error, no log, and `codesign`/`CMIODeviceStartStream`
both report success. Together they cost most of a day; none is guessable from the docs.

1. **The `.systemextension` bundle filename must equal its `CFBundleIdentifier`.**
   Otherwise `OSSystemExtensionRequest` fails with *"unable to find any matched
   extension"* — the request never even reaches `sysextd`. (Also needs
   `CFBundleSupportedPlatforms`; see the next section.)
2. **`CMIOExtensionClient.signingID` is the literal string `"unknown"`** for every
   client — it is NOT the code-signing identifier. Any auth check comparing it
   rejects the *legitimate* producer. Worse, a rejecting `authorizedToStartStream`
   surfaces to the producer as a bogus **`CMIODeviceStartStream` OSStatus `-4`**,
   which reads like a wrong-stream error and sends you down the wrong path. (A
   Team-ID-pinned `SecCode` check is also useless here — it can't evaluate an
   external process from inside the extension sandbox.) There is currently **no
   trustworthy way to authenticate the sink producer**; Apple's sample and SinkCam
   both leave the hook an unconditional `return true`, and so do we.
3. **`kCMIOStreamPropertyDirection` is INVERTED from its documented meaning.**
   Measured against our own extension (which registers source-then-sink), the
   **SOURCE reports `direction=1`** and the **SINK reports `direction=0`** — the
   opposite of the header's "0 = output / 1 = input". Picking the sink by direction
   makes the producer start the *source*, and **`CMIODeviceStartStream` returns
   SUCCESS on the wrong stream** while the extension's sink handlers never fire — so
   nothing drains the queue and every frame is dropped as "queue full". Select the
   sink by **stream name** (`kCMIOObjectPropertyName` → `*Sink*`) instead.
4. **macOS will not replace a same-`CFBundleVersion` system extension.** Re-activation
   silently keeps the old one running. Worse, a half-swapped state **splits the source
   and sink across two extension processes** (the old one keeps serving Photo Booth
   while the producer feeds the new one) and nothing works until a reboot.
   `make-camera-extension.sh` therefore stamps a **monotonic build number**.

**Debugging aids that made this tractable:**
- A CMIO extension's `os_log` is **invisible to a normal user** — it runs as the
  `_cmiodalassistants` role account. You must use
  `sudo log show --info --debug --predicate 'subsystem == "com.clintcan.macrdp.camera"'`.
  Without `sudo` you get *nothing*, which looks like "the extension isn't logging".
- Since the extension is otherwise opaque, macrdp logs **`enqueued`/`dropped_full`**
  every ~90 frames. `dropped_full` climbing with `enqueued` frozen at the queue
  capacity = **the extension is not draining the sink**; `dropped_full=0` with
  `enqueued` climbing = working.
- **Every extension code change costs a reboot to test cleanly.** Batch them.
- Diff against a known-good CMIO extension already on the machine (OBS Virtual
  Camera) — a `plutil -p` Info.plist diff found gotcha #1.

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
