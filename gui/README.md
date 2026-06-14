# macrdp Controller (menu-bar app)

A small AppKit menu-bar (tray) app that **controls** the macrdp LaunchAgent and
its `config.env` — start/stop the server, flip feature toggles, jump to the
permission panes, and open the log. It's a separate controller process, not the
server: quitting it leaves macrdp running under launchd.

## Why a separate process (not UI inside the Rust binary)

macrdp's tokio runtime owns the main thread, but AppKit's menu bar **must** run
on the main thread — so an in-process UI would mean restructuring the carefully
tuned threading/QoS model. The controller sidesteps that entirely: it drives
the server through `launchctl` and the shared `config.env`. TCC is unaffected —
the Screen Recording / Accessibility grants belong to the macrdp *binary* (the
API caller), and this controller needs none of them.

## Build & install

Prereq: set up the server bundle + LaunchAgent first (see `../packaging/`):

```bash
../packaging/make-app.sh
../packaging/install-launchagent.sh
```

Then build the controller:

```bash
./make-tray-app.sh                                  # -> /Applications/macrdpController.app
APP_DIR="$HOME/Applications" ./make-tray-app.sh     # or install without sudo
open "/Applications/macrdpController.app"            # display icon appears in the menu bar
```

Built with plain SwiftPM (no `.xcodeproj`): `swift build -c release` produces
the executable, `make-tray-app.sh` wraps it into a signed `LSUIElement` bundle
in `target/` and installs it.

## Menu

- **Status header** — running (with pid) / stopped / not installed.
- **Start · Stop · Restart** — bootstrap + `kickstart -k` / `bootout` the agent.
- **Options** — H.264 / AAC / HiDPI checkmarks (write `config.env` and live
  `kickstart` if running); shows the current bind address.
- **Edit config…** — opens `~/Library/Application Support/macrdp/config.env`.
- **Open Logs** — opens `~/Library/Logs/macrdp.log`.
- **Permissions** — deep-links to the Screen Recording / Accessibility panes.
- **Quit Controller** — quits the menu-bar app only; the server keeps running.

## Distribution (paid product)

Ad-hoc signing is local-only. For a shippable build, sign with a Developer ID
and notarize (same env contract as `packaging/`):

```bash
xcrun notarytool store-credentials macrdp-notary \
  --apple-id you@example.com --team-id TEAMID --password <app-specific-pw>

CODESIGN_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
  NOTARIZE=1 NOTARY_PROFILE=macrdp-notary ./make-tray-app.sh
```

This is MIT-licensed; selling a productized, notarized build + support is fully
compatible with that (you're selling the product, not a license exemption).
The Mac App Store is not viable for the server it controls (private
`CGVirtualDisplay` API + system-wide input/capture) — ship a direct download.

## Notes
- To auto-launch the controller at login: System Settings → General →
  Login Items → add `macrdpController.app` (the server itself already
  autostarts via its own LaunchAgent).
