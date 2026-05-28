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
--fps N                   Frame rate cap (default 15, or 60 with --enable-h264
                          — see "Video" for why H.264 wants the higher rate)
--enable-h264             Stream the display as H.264 over EGFX (AVC420),
                          hardware-encoded via VideoToolbox, instead of legacy
                          bitmaps. Falls back to legacy automatically for
                          clients that don't negotiate H.264. See "Video".
--bitrate N               Target H.264 bitrate in Mbps (default 6; only with
                          --enable-h264). Raise it (8–12) for sharper detail if
                          you have bandwidth headroom.
--keyframe-interval SECS  H.264 periodic keyframe (IDR) interval in seconds
                          (default 2; only with --enable-h264). Safety net for
                          transient decode glitches; fractional values OK.
--no-keyframe-on-change   Disable on-change H.264 keyframes (ON by default): an
                          IDR is otherwise forced on large changes (window-to-
                          front, scroll, app launch) and briefly after a click,
                          so big updates render at once. See "Video".
--flush-frames N          Trailing frames re-sent after each change to drain
                          mstsc's presentation buffer (default 4; only with
                          --enable-h264). Stops the last keystroke before a pause
                          lagging until the next keyframe. 0 disables. See "Video".
--no-lazy-paste           Opt out of lazy Windows→Mac file paste (default ON).
                          With lazy, temp files are pre-sized but empty when the
                          copy lands and stream bytes only on Cmd-V, with macOS's
                          native "Preparing to paste" progress dialog. Pass this
                          to fall back to the eager path (downloads everything
                          on copy, auto-fires Cmd-V into Finder when done).
                          See "Windows → Mac file copy" below.
--no-mute-on-minimize     Opt out of muting audio while the client window is
                          minimized (default ON). When the client sends the
                          standard `SuppressOutput` PDU on minimize, the server
                          stops emitting Wave PDUs so the client's audio queue
                          drains naturally; on refocus, audio resumes in sync
                          with the freshly IDR'd video. Pass this to keep audio
                          flowing through a minimize (preserves "minimized
                          YouTube keeps playing on the Mac speakers") at the
                          cost of accepting that drift on refocus. See "Audio"
                          below.
--qoi-force-rgb           Force QOI BitmapUpdates to emit `Channels::Rgb` instead
                          of the natural `Channels::Rgba` mapping from a *A32
                          capture. Default OFF (matches upstream ironrdp-server).
                          Only matters if you connect with an IronRDP-based viewer
                          built against ironrdp-session WITHOUT the RGBA decode
                          patch (currently every published release, until
                          Devolutions/IronRDP#1341 lands) — without this flag those
                          viewers will render blank with one "Unsupported RGBA QOI
                          data" warning per frame. mstsc / Microsoft Remote Desktop
                          / Windows App / FreeRDP don't advertise QOI and are
                          unaffected either way.
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

# Use the eager Windows→Mac file paste path (default is lazy / on-demand).
./macrdp --no-lazy-paste
```

## Video (H.264)

By default the display is sent as legacy bitmaps (RemoteFx/QOI to mstsc, NSCodec/raw to others) — works everywhere, but bandwidth-heavy. Pass **`--enable-h264`** to stream the desktop as **H.264 over the EGFX virtual channel** (MS-RDPEGFX, AVC420), hardware-encoded with VideoToolbox. Far less bandwidth, especially for video/scrolling/photos.

How it behaves:

- **Automatic fallback.** Clients that don't advertise H.264 (AVC420) decode — e.g. a FreeRDP build without an H.264 decoder — transparently fall back to legacy bitmaps. No need to match the flag to the client. mstsc, FreeRDP-with-H.264, and the macOS **Windows App** / Microsoft Remote Desktop client all decode the H.264 stream.
- **Wire format.** The AVC420 payload is Annex-B framed (what Microsoft's decoder expects). The bitstream is verified rendering on `mstsc` and on FreeRDP built with H.264 (e.g. the [Thincast client]).
- **Bitrate.** `--bitrate N` sets the target encoder bitrate in megabits/sec (default `6`, only meaningful with `--enable-h264`). Raising it sharpens detail but grows each frame, so the big per-frame writes are more likely to fill the socket buffer and delay audio on a constrained link — `6` is a good balance; try `8`–`12` if you have headroom.
- **Color.** The stream is encoded as full-range BT.709. This matters for `mstsc`, which reads AVC420 luma as full-range regardless of the bitstream flag — video-range output otherwise renders washed-out / lighter there. FreeRDP honors the flag and is correct either way. To get full range we convert each captured BGRA frame to full-range NV12 ourselves (VideoToolbox would otherwise emit video-range from a BGRA source); that conversion is **vImage**-accelerated — see [Color conversion: scalar vs vImage](#color-conversion-scalar-vs-vimage).
- **Frame rate.** `--enable-h264` defaults to **60fps** (vs 15 for legacy). mstsc holds a fixed ~2-frame presentation buffer for the H.264 stream, so at 30fps typing lags ~2 keystrokes (~66ms) while at 60fps that buffer is ~33ms and feels immediate. FreeRDP-based clients don't buffer this way and are snappy at any rate. Set `--fps` explicitly to override (lower it to save CPU/bandwidth if your client/link doesn't need 60).
- **Keyframes.** A keyframe (IDR) is forced on the first frame, then periodically every `--keyframe-interval` seconds (default `2`) as a safety net — some clients (mstsc) only fully recover a transient decode glitch on the next IDR, so a long interval leaves garbled regions (notably text) lingering. Lower it for faster recovery at the cost of bandwidth/quality; raise it for smoother typing. On top of that, an IDR is forced **on demand** whenever a large area changes at once (window-to-front, scroll, app launch) and briefly after a mouse click, so big updates land immediately instead of waiting for the periodic interval (rising-edge detection keeps sustained churn like video from forcing an IDR every frame). Pass **`--no-keyframe-on-change`** to disable that and rely on the periodic interval alone. The trigger thresholds are tunable: `--keyframe-change-pct` (default 20, the dirty-area % that fires an IDR), `--keyframe-click-pct` (default 5, the lowered threshold after a click), and `--keyframe-click-window-ms` (default 400, how long that lowered threshold lasts).
- **Flush frames (`--flush-frames`, default `4`).** ScreenCaptureKit only delivers a frame when the screen changes, so after the last keystroke before a pause there are no further frames to push it through mstsc's ~2-frame AVC420 presentation buffer — it would strand there until the next change or periodic keyframe (the classic "typing follows the keyframe" lag). After each change the server re-submits the last frame this many times as cheap skip-P-frames, draining the buffer so the change appears within a couple of frame intervals (~33 ms at 60fps), then goes quiet. mstsc needs ≥2; raise if a slight trailing lag remains, or set `0` to disable.

### Known limitations

- **Reconnecting `mstsc` to a still-running macrdp can show a black screen** (with a live cursor). This is an mstsc-specific quirk: it retains EGFX surfaces for the lifetime of its process and mis-composites on reconnect. It is *not* a server bug — FreeRDP reconnects cleanly over the same stream. **Reliable workaround:** fully close the mstsc window and open a new connection — quitting the client clears its surface cache, so the desktop renders every time, with no Windows reboot needed. (A server-side fresh-surface-id workaround was tried but only mitigated this unreliably on mstsc, so it was dropped in favor of this documented recovery.)
- H.264 is **macOS-only** (VideoToolbox) and still maturing — bitrate and keyframe behavior are tunable (above), but dirty-region *encoding* is not yet done: every frame is a full encode (dirty rects are used only to time on-demand keyframes, not to encode sub-regions). H.264's own inter-prediction keeps unchanged regions cheap regardless.

### Color conversion: scalar vs vImage

*(Implementation detail — skip unless you're profiling CPU or porting the encoder.)*

VideoToolbox, given a BGRA source, emits **video-range** YUV (luma 16–235). `mstsc` reads AVC420 luma as **full-range**, so that looks washed out (see **Color** above). The fix is to hand VideoToolbox a YUV buffer that's already full-range, which means doing the BGRA → full-range BT.709 NV12 (`420f`) color conversion ourselves, once per captured frame, on the capture thread.

That conversion is a real per-frame cost, so it's done with **vImage** (Apple's Accelerate framework), which runs the RGB→Y'CbCr math on the CPU's vector units (NEON on Apple Silicon). A scalar reference implementation (a plain Rust loop) is kept as well: it's the fallback for any frame vImage declines (e.g. odd dimensions), the oracle the vImage path is unit-tested against, and the baseline below. Both produce identical output (within ±1 rounding).

Single-thread cost per frame, Apple M3 (`cargo test --release bench_nv12_full_range -- --ignored --nocapture`):

| Resolution | scalar | vImage | speedup |
|---|---:|---:|---:|
| 1470×956 | 3.36 ms | 0.12 ms | ~29× |
| 1920×1080 | 4.98 ms | 0.16 ms | ~32× |
| 2560×1440 | 8.88 ms | 0.33 ms | ~27× |
| 3840×2160 (4K) | 20.0 ms | 0.84 ms | ~24× |

At 60fps the frame budget is 16.67 ms. The scalar path is fine at 1080p (~30% of one core) but **exceeds the budget at 4K**, where it would cap the achievable frame rate before the encoder even runs; vImage keeps the conversion at ~1% of budget across the board, so it's never the bottleneck. The implementation lives in `src/videotoolbox.rs` (`bgra_to_nv12_full_range_vimage`, with `bgra_to_nv12_full_range` as the scalar reference).

## Audio

System audio rides over the RDPSND virtual channel as 16-bit stereo PCM at **44.1 kHz**. ScreenCaptureKit only supports 8 / 16 / 24 / 48 kHz, so the capture loop captures at 48 kHz and resamples to 44.1 with [`rubato`](https://github.com/HEnquist/rubato) before sending. 44.1 matches the native rate of most Windows audio endpoints, which avoids the client-side resampling drift that otherwise accumulates into multi-second audio backlogs. A generation counter on the audio factory keeps a client reconnect from leaving a second capture loop feeding the channel. The vendored `ironrdp-server` carries a single patch that makes `dispatch_server_events` keep the *newest* queued waves on per-batch overflow instead of the oldest — without it, a one-off video-encode stall would bake a permanent audio-latency offset into the session.

**Mute on minimize** (default-on, opt out with `--no-mute-on-minimize`). When the client minimizes its window it sends the standard `SuppressOutput { None }` PDU; the server stops emitting both EGFX video frames and RDPSND waves until the client refocuses (`RefreshRectangle` / `SuppressOutput { Some(rect) }`). Without this, mstsc accumulates a backlog of video frames + audio waves during a long minimize that has to chew through on refocus, producing several seconds of input lockout, audio drift, and a video catch-up storm. With it, you get a brief audio gap on refocus and audio + video resume in sync. Both gates are debounced (1 s) so transient `SuppressOutput` flaps mstsc emits under wire pressure (e.g., during a heavy local `cargo build`) don't oscillate the mute and cause stutter. Pass `--no-mute-on-minimize` if you specifically want audio to keep playing while the client window is minimized — accepting that audio will drift by however long was spent minimized.

## File copy

Bidirectional via MS-RDPECLIP. Both directions support single files and folder trees.

**Mac → Windows.** `Cmd-C` a file or folder in Finder, `Ctrl-V` in Windows Explorer. The pasteboard walk recurses into directories (skipping symlinks, capped at 10 000 descriptors per copy) and emits the right `relative_path` so Explorer reconstructs the tree. Bytes stream on demand via `FileContentsRequest` chunks (4 MiB per chunk). Windows shows its native "Copying…" progress dialog.

**Windows → Mac (lazy, default).** `Ctrl-C` in Explorer, `Cmd-V` in Finder. The server pre-allocates an empty temp file per leaf at its declared size and registers each one with `NSFileCoordinator` via `NSFilePresenter`. Bytes only start streaming when Finder asks for them on `Cmd-V`, and macOS shows its **native "Preparing to paste" progress dialog** during the wait. Folder trees and multi-file selections both work. Lower chunk parallelism is used than the eager path so the RDP session stays responsive (mouse / keyboard / video) while a multi-hundred-MB paste is in flight. If you'd rather have files downloaded eagerly the moment Windows announces a copy (and `Cmd-V` auto-fired into Finder when ready, with an audible Glass-chime cue), pass `--no-lazy-paste`.

### Known limitations

- **`Ctrl-C` on a *folder* in Windows Explorer doesn't reach the Mac.** Explorer puts only the Shell IDList format on the clipboard and delay-renders `FileGroupDescriptorW`, which `mstsc` doesn't request — so nothing is forwarded over the RDP clipboard channel and you'll hear a beep on `Cmd-V`. Windows + mstsc behavior, not fixable server-side. **Workaround:** open the folder in Explorer, `Ctrl-A` to select its contents, then `Ctrl-C` — that path uses `FileGroupDescriptorW` directly and folder structure is preserved.
- **Some Windows shell extensions silently swallow specific files from the clipboard.** Archive tools (7-Zip, WinRAR, built-in Compressed Folders) commonly hook extensions like `.zip`, `.gz`, `.7z`, `.bz`, `.bz2`, `.rar`, `.tar` and intercept Explorer's clipboard so `Ctrl-C` either sends no `FileGroupDescriptorW` to mstsc or sends none at all. The Mac side detects the clipboard transition and clears the pasteboard, so `Cmd-V` in Finder beeps clearly instead of silently re-pasting the previous file. **Workaround:** rename the file to a neutral extension (e.g. `.bin`) and Windows will publish it normally.

## Reason why this was made

This was done to scratch an itch.  There are practically no active open source RDP servers for MacOS.  The closest project that does this functionality is xrdp; however this program only runs on Linux/Unix machines, and has no homebrew equivalent on Macs. Done in a few hours with the help of Claude and runs pretty well. Multi-monitor support is on the list when I'm bored or need a distraction from real life.

## License

MIT OR Apache-2.0.

[IronRDP]: https://github.com/Devolutions/IronRDP
[Thincast client]: https://thincast.com/en/products/client
[Thincast]: https://thincast.com/en/products/client
