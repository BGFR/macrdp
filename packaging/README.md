# Packaging — `macrdp.app` (personal use)

Wraps the `macrdp` binary in a stably-signed `.app` bundle and runs it as a
per-user **LaunchAgent**. The point is not double-click UX (macrdp is a
flag-driven server) — it's a **stable signed identity at a fixed path** so the
Screen Recording / Accessibility TCC grants survive rebuilds, plus
non-interactive autostart via the Keychain.

This layout is also GUI-ready: a future menu-bar controller just spawns the
same co-signed helper and edits the same `config.env` — no re-permissioning.

> **vs. `dist/install.sh`:** the repo's other auto-start path installs a *bare
> binary* to `~/.local/bin/macrdp` under the launchd label `com.user.macrdp`.
> This `packaging/` path instead produces a real `.app` bundle under the label
> `com.clintcan.macrdp`. They share the `macrdp` Keychain entry and both bind
> `:3390`, so they're **mutually exclusive** — pick one. Use `dist/install.sh`
> for the lightweight binary; use `packaging/` when you want a stable bundle
> identity and a path a future GUI can build on. The build is staged in
> `target/macrdp.app` (gitignored) before install.

## Files

| File | Role |
|------|------|
| `Info.plist` | Bundle metadata template (`__VERSION__` filled from `Cargo.toml`); `LSUIElement` agent, `NSAppleEventsUsageDescription`. |
| `macrdp-launch` | Wrapper run by launchd: reads `config.env`, translates to flags, `exec`s the signed binary. |
| `config.env.example` | Seed for `~/Library/Application Support/macrdp/config.env`. |
| `com.clintcan.macrdp.plist` | LaunchAgent template (`__APP_DIR__`/`__HOME__` filled at install). |
| `make-app.sh` | Build → assemble bundle → co-sign helper + bundle → install. |
| `install-launchagent.sh` | Seed config, render plist, bootstrap the agent. |

## One-time setup

```bash
# 1. Build + install the bundle (ad-hoc signed) to /Applications.
#    Use APP_DIR=$HOME/Applications to avoid sudo.
packaging/make-app.sh

# 2. Store the macOS account password so launchd can start headless.
security add-generic-password -s macrdp -a "$(id -un)" -w 'YOUR_PASSWORD'

# 3. Install + load the LaunchAgent.
packaging/install-launchagent.sh

# 4. First launch will need TCC grants. Grant macrdp.app under
#    System Settings -> Privacy & Security -> Screen Recording AND Accessibility,
#    then: launchctl kickstart -k gui/$(id -u)/com.clintcan.macrdp
```

## Day to day

```bash
tail -f ~/Library/Logs/macrdp.log                       # logs
launchctl print gui/$(id -u)/com.clintcan.macrdp        # status (state/pid)
$EDITOR "$HOME/Library/Application Support/macrdp/config.env"
launchctl kickstart -k gui/$(id -u)/com.clintcan.macrdp # apply config change
launchctl bootout    gui/$(id -u)/com.clintcan.macrdp   # stop entirely
```

Edit feature toggles (H.264, AAC, HiDPI), bind address, and an `EXTRA_FLAGS`
escape hatch in `config.env`. It's outside the bundle, so edits never disturb
the code signature or the TCC grants.

## Notes & limits

- **Ad-hoc signing is local-only.** `make-app.sh` ad-hoc signs by default
  (`CODESIGN_IDENTITY=-`), which is fine for your own machine but Gatekeeper
  quarantines it on anyone else's. For distribution, sign with a Developer ID
  and notarize:

  ```bash
  # one-time: store notary credentials in the keychain
  xcrun notarytool store-credentials macrdp-notary \
    --apple-id you@example.com --team-id TEAMID --password <app-specific-pw>

  # build signed + notarized + stapled (secure timestamp is automatic for a real ID)
  CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
    NOTARIZE=1 NOTARY_PROFILE=macrdp-notary packaging/make-app.sh
  ```

  `packaging/notarize.sh` (zip → `notarytool submit --wait` → `stapler staple`)
  runs on the staged app so the ticket travels with the install copy.
  **Note:** the Mac App Store is not a viable channel — macrdp uses the private
  `CGVirtualDisplay` API and system-wide `CGEventPost`/ScreenCaptureKit, which
  the MAS sandbox forbids. Ship a notarized direct download (DMG/zip).
- **TCC is keyed to the binary, not the wrapper.** The grants attach to
  `Contents/MacOS/macrdp` (the process that calls ScreenCaptureKit / CGEventPost
  after `exec`). Keep the install path stable and the grants persist.
- **Re-signing on rebuild** keeps the same identity as long as the bundle ID,
  install path, and signing identity are unchanged — so re-running
  `make-app.sh` for an update does not reset permissions.
- Login-window / lock-screen / secure-input contexts still can't receive
  synthetic input — an OS limitation, unchanged by packaging.
