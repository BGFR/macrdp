# macrdp

A native RDP server for macOS, written in Rust on top of [IronRDP]. Connect from `mstsc`, Microsoft Remote Desktop, or FreeRDP to drive your Mac desktop with keyboard, mouse, real-cursor-shape forwarding, text + image clipboard sync, Mac→Windows file copy, and system audio forwarding. NLA/CredSSP is supported. Authenticates against your local Mac account via PAM.

This is the macOS equivalent of `xrdp`. Not a client, not a VNC bridge.

## Status

v0 — daily-driver usable on a trusted LAN. See [CLAUDE.md](CLAUDE.md) for what's wired up, what isn't, and known quirks.

## Quick start

```bash
cargo build --release
codesign -s - --force target/release/macrdp   # ad-hoc sign so TCC grants persist
./target/release/macrdp
```

First run will prompt for:
1. **Screen Recording permission** (System Settings → Privacy & Security → Screen Recording → enable `macrdp` → restart it).
2. **Accessibility permission** (same path, "Accessibility" — required to forward keyboard and mouse).
3. Your Mac password at the terminal — validated against your local account via PAM `checkpw`, then used as the RDP credential.

Then connect from a client to `<your-mac-ip>:3390` with your Mac username and password. `mstsc` will prompt for credentials in its own NLA dialog — no need to pre-type the username.

## Auto-start at login (launchd)

```bash
dist/install.sh
```

Builds + signs + installs to `~/.local/bin/macrdp`, stores your Mac password in the macOS Keychain under service `macrdp`, drops a launchd plist at `~/Library/LaunchAgents/com.user.macrdp.plist`, and loads it. macrdp will start on every login and restart if it crashes. Re-run the script after `cargo build --release` to refresh the installed binary.

```bash
launchctl print gui/$UID/com.user.macrdp | head    # status
launchctl kickstart -k gui/$UID/com.user.macrdp    # restart
launchctl bootout gui/$UID/com.user.macrdp         # stop / uninstall
```

## CLI

```
--bind 0.0.0.0:3390       Listen address (3390 by default; 3389 needs root)
--username NAME           Defaults to $USER
--password PASS           Skip the interactive prompt
--skip-auth               Bypass PAM (testing only)
--keychain                Read password from macOS Keychain (service=macrdp)
-v, --verbose             Show all the noisy logs the default filter hides
--allow-sleep             Let the Mac sleep / auto-lock normally (default
                          is to spawn `caffeinate` so an idle Mac doesn't
                          drop the connection mid-session)
--width / --height        Override autodetected display size
--fps N                   Frame rate cap (default 15)
--cert-dir PATH           Persisted TLS cert (default ~/Library/Application Support/macrdp)
```

`RUST_LOG=debug` for verbose logging.

## Audio

System audio rides over the RDPSND virtual channel as 16-bit stereo PCM at **44.1 kHz**. ScreenCaptureKit only supports 8 / 16 / 24 / 48 kHz, so the capture loop captures at 48 kHz and resamples to 44.1 with [`rubato`](https://github.com/HEnquist/rubato) before sending. 44.1 matches the native rate of most Windows audio endpoints, which avoids the client-side resampling drift that otherwise accumulates into multi-second audio backlogs. A generation counter on the audio factory keeps a client reconnect from leaving a second capture loop feeding the channel. The vendored `ironrdp-server` carries a single patch that makes `dispatch_server_events` keep the *newest* queued waves on per-batch overflow instead of the oldest — without it, a one-off video-encode stall would bake a permanent audio-latency offset into the session.

## Reason why this was made
This was done to scratch an itch.  There are practically no active open source RDP servers for MacOS.  The closest project that does this functionality is xrdp; however this program only runs on Linux/Unix machines, and has no homebrew equivalent on Macs. Done in a few hours with the help of Claude and runs pretty well.

Multi-monitor support is on the list when I'm bored or need a distraction from real life. As of v0.5.15, the Mac→Windows file clipboard streams real bytes end-to-end: copying a file in Finder and pasting on the Windows side produces an actual file in Explorer. Windows→Mac direction and recursive directory copy aren't wired yet.

## License

MIT OR Apache-2.0.

[IronRDP]: https://github.com/Devolutions/IronRDP
