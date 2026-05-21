# macrdp

A native RDP server for macOS, written in Rust on top of [IronRDP]. Connect from `mstsc`, Microsoft Remote Desktop, or FreeRDP to drive your Mac desktop with keyboard, mouse, real-cursor-shape forwarding, text + image clipboard sync, Mac↔Windows file copy, system audio forwarding, and optional H.264 video (EGFX/AVC420, hardware-encoded). NLA/CredSSP is supported. Authenticates against your local Mac account via PAM.

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
--enable-h264             Stream the display as H.264 over EGFX (AVC420),
                          hardware-encoded via VideoToolbox, instead of legacy
                          bitmaps. Falls back to legacy automatically for
                          clients that don't negotiate H.264. See "Video".
--bitrate N               Target H.264 bitrate in Mbps (default 6; only with
                          --enable-h264). Raise it (8–12) for sharper detail if
                          you have bandwidth headroom.
--keyframe-interval SECS  H.264 periodic keyframe (IDR) interval in seconds
                          (default 5; only with --enable-h264). Safety net for
                          transient decode glitches; fractional values OK.
--no-keyframe-on-change   Disable on-change H.264 keyframes (ON by default): an
                          IDR is otherwise forced on large changes (window-to-
                          front, scroll, app launch) and briefly after a click,
                          so big updates render at once. See "Video".
--cert-dir PATH           Persisted TLS cert (default ~/Library/Application Support/macrdp)
--virtual-display         Serve a headless virtual display at --width × --height
                          instead of mirroring the primary panel — local screen
                          stays untouched. Requires --width and --height.
--make-primary            Promote the virtual display to system primary (the one
                          with the menu bar). Only valid with --virtual-display.
--detach-primary          While a client is connected, disable every physical
                          display (backlights off, no menu bar). Restored on
                          disconnect / exit. Only with --virtual-display.
--capture-primary         Alternative to --detach-primary: exclusive
                          CGDisplayCapture of every physical display, then
                          gamma-clamp to black. Panels stay backlit but render
                          solid black. Use when --detach-primary doesn't
                          actually blank the panel on your hardware. Mutually
                          exclusive with --detach-primary. Only with
                          --virtual-display.
```

`RUST_LOG=debug` for verbose logging.

## Headless mode

`--virtual-display --width W --height H` allocates a headless display via undocumented `CGVirtualDisplay*` private API and serves it over RDP instead of mirroring the Mac's panel. Behaves like plugging in an external monitor — the remote session gets its own desktop at the requested resolution, and you keep using the Mac locally as normal. Add `--make-primary` to give the virtual display the menu bar so new app windows open there.

To go *fully* headless while a client is connected, pick one:

- **`--detach-primary`** — turns the backlight off on every built-in / external panel via `CGSConfigureDisplayEnabled`. Cleanest visually. On some macOS versions / displays the disable transaction succeeds but the panel keeps showing the desktop; if you hit that, switch to:
- **`--capture-primary`** — takes exclusive `CGDisplayCapture` of every physical display and forces the gamma LUT to map every input to black. Backlight stays on but panels render solid black. Works everywhere capture is allowed; uses only public CG symbols.

Both restore the original layout when the last client disconnects, and both auto-revert on `SIGKILL` / panic (no logout required). Pick `--detach-primary` first; fall back to `--capture-primary` if your hardware doesn't honor the disable.

## Examples

```bash
# Default — loopback only, mirror primary panel, prompt for password.
./macrdp

# Accept LAN connections, force a non-$USER account.
./macrdp --bind 0.0.0.0:3390 --username clint

# Higher frame rate, custom cert dir.
./macrdp --fps 30 --cert-dir ~/.macrdp-certs

# H.264 video over EGFX (much lower bandwidth than legacy bitmaps).
./macrdp --enable-h264

# Verbose logs (DEBUG level).
./macrdp -v

# Headless virtual display at 1440p — local Mac screen stays available.
./macrdp --virtual-display --width 2560 --height 1440

# Same, but the virtual display owns the menu bar (drive it as your main desktop).
./macrdp --virtual-display --width 2560 --height 1440 --make-primary

# Fully headless on connect: physical panels go dark, revived on disconnect.
./macrdp --virtual-display --width 2560 --height 1440 --detach-primary

# Same idea, for hardware where --detach-primary doesn't actually blank the panel.
./macrdp --virtual-display --width 2560 --height 1440 --capture-primary

# Non-interactive launch (used by dist/install.sh): password from Keychain.
./macrdp --keychain

# Quick dev test on loopback — skips PAM, accepts --password verbatim.
./macrdp --skip-auth --password test
```

## Video (H.264)

By default the display is sent as legacy bitmaps (RemoteFx/QOI to mstsc, NSCodec/raw to others) — works everywhere, but bandwidth-heavy. Pass **`--enable-h264`** to stream the desktop as **H.264 over the EGFX virtual channel** (MS-RDPEGFX, AVC420), hardware-encoded with VideoToolbox. Far less bandwidth, especially for video/scrolling/photos.

How it behaves:

- **Automatic fallback.** Clients that don't advertise H.264 (AVC420) decode — e.g. a FreeRDP build without an H.264 decoder, or Microsoft Remote Desktop on macOS (NSCodec only) — transparently fall back to legacy bitmaps. No need to match the flag to the client.
- **Wire format.** The AVC420 payload is Annex-B framed (what Microsoft's decoder expects). The bitstream is verified rendering on `mstsc` and on FreeRDP built with H.264 (e.g. the [Thincast client]).
- **Bitrate.** `--bitrate N` sets the target encoder bitrate in megabits/sec (default `6`, only meaningful with `--enable-h264`). Raising it sharpens detail but grows each frame, so the big per-frame writes are more likely to fill the socket buffer and delay audio on a constrained link — `6` is a good balance; try `8`–`12` if you have headroom.
- **Color.** The stream is encoded as full-range BT.709. This matters for `mstsc`, which reads AVC420 luma as full-range regardless of the bitstream flag — video-range output otherwise renders washed-out / lighter there. FreeRDP honors the flag and is correct either way.
- **Keyframes.** A keyframe (IDR) is forced on the first frame, then periodically every `--keyframe-interval` seconds (default `5`) as a safety net — some clients (mstsc) only fully recover a transient decode glitch on the next IDR, so a long interval leaves garbled regions (notably text) lingering. Lower it for faster recovery at the cost of bandwidth/quality; raise it for smoother typing. On top of that, an IDR is forced **on demand** whenever a large area changes at once (window-to-front, scroll, app launch) and briefly after a mouse click, so big updates land immediately instead of waiting for the periodic interval (rising-edge detection keeps sustained churn like video from forcing an IDR every frame). Pass **`--no-keyframe-on-change`** to disable that and rely on the periodic interval alone.

### Known limitations

- **Reconnecting `mstsc` to a still-running macrdp can show a black screen** (with a live cursor). This is an mstsc-specific quirk: it retains EGFX surfaces for the lifetime of its process and mis-composites on reconnect. It is *not* a server bug — FreeRDP reconnects cleanly over the same stream. **Reliable workaround:** quit macrdp and relaunch it before reconnecting — a fresh macrdp run hands mstsc a never-cached surface id, so the desktop renders every time, with no Windows reboot needed. (Reopening the mstsc window also works; fresh connections always render fine.)
- **The new Windows App client may fail to connect** (it sends a GCC block the underlying IronRDP parser rejects, before any video negotiation — so this affects all modes, not just H.264). Use `mstsc`, FreeRDP, or a FreeRDP-based client (e.g. [Thincast]) instead.
- H.264 is **macOS-only** (VideoToolbox) and still maturing — bitrate and keyframe behavior are tunable (above), but dirty-region *encoding* is not yet done: every frame is a full encode (dirty rects are used only to time keyframes under `--keyframe-on-change`, not to encode sub-regions). H.264's own inter-prediction keeps unchanged regions cheap regardless.

## Audio

System audio rides over the RDPSND virtual channel as 16-bit stereo PCM at **44.1 kHz**. ScreenCaptureKit only supports 8 / 16 / 24 / 48 kHz, so the capture loop captures at 48 kHz and resamples to 44.1 with [`rubato`](https://github.com/HEnquist/rubato) before sending. 44.1 matches the native rate of most Windows audio endpoints, which avoids the client-side resampling drift that otherwise accumulates into multi-second audio backlogs. A generation counter on the audio factory keeps a client reconnect from leaving a second capture loop feeding the channel. The vendored `ironrdp-server` carries a single patch that makes `dispatch_server_events` keep the *newest* queued waves on per-batch overflow instead of the oldest — without it, a one-off video-encode stall would bake a permanent audio-latency offset into the session.

## Reason why this was made
This was done to scratch an itch.  There are practically no active open source RDP servers for MacOS.  The closest project that does this functionality is xrdp; however this program only runs on Linux/Unix machines, and has no homebrew equivalent on Macs. Done in a few hours with the help of Claude and runs pretty well.

Multi-monitor support is on the list when I'm bored or need a distraction from real life. File copy is now bidirectional: Mac→Windows streams real bytes via the standard MS-RDPECLIP path; Windows→Mac eagerly downloads to `/tmp` the moment Windows announces a copy, publishes the file URLs to NSPasteboard, plays a Glass chime when ready, and automatically fires `Cmd-V` if Finder is the frontmost app so the paste completes without a second keystroke.

### Windows → Mac file copy: known limitation

`Ctrl-C` on a *folder* in Windows Explorer doesn't reach the Mac side. Explorer puts only the Shell IDList format on the clipboard and delay-renders `FileGroupDescriptorW`, which `mstsc` doesn't request — so nothing is forwarded over the RDP clipboard channel and you'll hear a beep on `Cmd-V`. This is a Windows + mstsc behavior we can't work around server-side.

**Workaround:** open the folder in Explorer, `Ctrl-A` to select its contents (files and any subfolders), `Ctrl-C`, then `Cmd-V` in Finder. That path produces a real file group descriptor and folder structure is preserved via `relative_path`. For copying entire arbitrary folder trees rooted at a folder you don't want to enter, RDP drive redirection is a more appropriate feature (not currently implemented).

## License

MIT OR Apache-2.0.

[IronRDP]: https://github.com/Devolutions/IronRDP
[Thincast client]: https://thincast.com/en/products/client
[Thincast]: https://thincast.com/en/products/client
