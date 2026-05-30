# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> Layout note: the big reference sections live in separate files and are pulled
> in via `@import` below, so the full context still loads every session.
> - `@docs/architecture.md` — module map + cross-cutting design
> - `@docs/macos-gotchas.md` — TCC, CGVirtualDisplay, QoS, activation
> - `@docs/known-quirks.md` — hard-won client/codec/audio behavioural notes
>
> The `vendor/ironrdp-server/` fork has its own nested `CLAUDE.md` (the
> divergence log) that loads only when you work inside that directory.

## Status

Functional v0. RDP clients (mstsc, Microsoft Remote Desktop, FreeRDP) can:
- Connect over TLS to the Mac on port 3390 with a local Mac username/password.
- See the primary display at native resolution with incremental damage-region updates.
- Optionally capture the primary display at its **backing (Retina) pixel resolution** (`--hidpi`, e.g. 3024×1964 instead of 1512×982 logical points) so clients render crisp native pixels instead of upscaling a point-density frame. Opt-in (it's ~4× the pixels); the win is biggest with `--enable-h264`. Verified crisp; input/cursor are resolution-correct. **Caveat:** mstsc decodes 4× the pixels per frame and feels laggy at HiDPI — Thincast/FreeRDP stay snappy. See the HiDPI quirk note below.
- Optionally stream the display as **H.264 over EGFX** (`--enable-h264`, AVC420, Annex-B framing, VideoToolbox-encoded) — far less bandwidth than legacy bitmaps. Verified rendering on mstsc, on FreeRDP built with H.264 decode, and on the macOS Windows App / Microsoft Remote Desktop client (it decodes AVC420 over EGFX — only its *legacy* bitmap-codec list is NSCodec-only). Clients that genuinely don't advertise AVC420 decode (e.g. a decoder-less FreeRDP build) fall back to legacy BitmapUpdate automatically. **Caveat:** reconnecting *mstsc* to a still-running macrdp can show a blank screen (mstsc-specific EGFX surface-handling quirk — confirmed not a server bug, since FreeRDP reconnects cleanly); reliable workaround is to fully close and reopen the mstsc window (clears its surface cache). See the H.264 quirk note below.
- Drive keyboard and mouse, including modifier keys (per-side L/R tracking with NX_DEVICE bits, Caps Lock as a toggle, MS-RDPBCGR Synchronize lock-state reconciliation), mouse buttons, and wheel.
- Forward macOS symbolic hotkeys that WindowServer's dispatcher refuses to fire for user-space CGEventPost: Cmd+Tab / Cmd+Shift+Tab cycle apps via Accessibility API (per-bundle dedup with MRU, dead-pid filtering via `kill(pid, 0)`), Cmd+\` / Cmd+Shift+\` cycle windows of the current app (AXRaise + window AXMain + app AXMainWindow for Electron compatibility), Cmd+Space invokes Spotlight via AppleScript, Cmd+Shift+3/4/5 shell out to `/usr/sbin/screencapture` or open Screenshot.app.
- See the real macOS cursor shape (I-beam, hand, etc.) overlaid by the client.
- Copy/paste UTF-8 text and images (CF_DIB ↔ PNG) between Mac and remote.
- Mac→Windows file copy, including whole folders: copying a file or directory in Finder and pasting on Windows produces a real file/tree in Explorer. The pasteboard walk recurses into directories (skipping symlinks, capped at 10 000 descriptors per copy) and emits one FILEGROUPDESCRIPTORW entry per leaf with `relative_path` set so upstream's wire encoder reconstructs the right `MyFolder\sub\file.txt` cFileName. Bytes stream via MS-RDPECLIP `FileContentsRequest` SIZE + RANGE chunks (4 MiB per chunk). Reaches upstream `Cliprdr::initiate_file_copy` via the vendored `ServerEvent::ClipboardFileCopy(Vec<FileDescriptor>)` variant — that's the only API that populates `local_file_list`, without which upstream short-circuits every byte fetch with CB_RESPONSE_FAIL. Finder hands out *file-reference* URLs (`/.file/id=...`); we resolve them through `NSURL::URLByResolvingSymlinksInPath` because `std::fs::metadata` can't stat them directly.
- Windows→Mac file copy (single files OR folder trees via Ctrl-A+Ctrl-C inside a folder; raw Ctrl-C on a folder doesn't work — see caveat below). Two paths, switched by `--no-lazy-paste` (lazy is the default):
  - **Lazy (default, `src/file_promise_lazy.rs`):** create a pre-sized empty temp file per leaf, register one `NSFilePresenter` per file via `NSFileCoordinator.addFilePresenter:`, publish only the top-level `NSURL`s to NSPasteboard. Bytes stream only when Finder's Cmd-V triggers the coordinator's `relinquishPresentedItemToReader:` callback, during which we synchronously fetch chunks (1 MiB × 2 in flight, `LAZY_PARALLEL_CHUNKS` — lower than eager because the user is actively interacting at paste time and a higher count visibly stutters RDP input) into the pre-allocated file, then invoke `reader(nil)`. macOS shows its native "Preparing to paste" progress dialog during the wait. No Glass chime / auto-Cmd-V needed (the user pasted; Finder handles it). On fetch failure we delete the temp file so Finder errors loudly rather than silently copying a zero-padded ghost.
  - **Eager (`--no-lazy-paste`, `src/file_promise.rs`):** when Windows announces a `FileGroupDescriptorW` we download every entry to `/tmp/macrdp-paste-<pid>-<nanos>/` via parallel `FileContentsRequest` chunks (1 MiB × 8 in flight, `EAGER_PARALLEL_CHUNKS`), recreating any directory structure encoded in each descriptor's `relative_path`, then publish the top-level entries to NSPasteboard as real `NSURL`s. On completion we play `/System/Library/Sounds/Glass.aiff` (`afplay` bypasses notification permissions; `osascript display notification` is silently suppressed because macOS attributes the banner to the unsigned macrdp binary) and, *only if Finder is the frontmost app*, fire `Cmd-V` via System Events so the paste the user attempted finishes automatically. Kept as a fallback for users who prefer the up-front download + auto-paste UX, and for any file whose descriptor lacks a size (lazy falls back to eager automatically in that case). Both paths share `paste_temp_dir` + `self_change_count` on the backend and clean up on disconnect (Drop on `MacCliprdrBackend`) and signal exit (`shutdown_cleanup()` via process-global handle, since `std::process::exit` bypasses Drop).
  - Both paths use the same `resolve_dest` for `relative_path` sanitization (rejects `.`, `..`, embedded `/`) so a malicious remote can't escape the temp sandbox; both share the same `fetch_one_file` chunk fan-out (pwrite via `FileExt::write_at` over an `Arc<File>`, no per-chunk open+seek+close); both rely on `CAN_LOCK_CLIPDATA` being negotiated (see clipboard.rs `client_capabilities`) so cliprdr auto-issues Lock/Unlock around the descriptor — without that cap, Windows treated the descriptor as ephemeral and would silently drop rapid follow-up Ctrl-C *and* release file data mid-stream on large downloads (CB_RESPONSE_FAIL). A `SelfChangeCount` atomic stops our own NSPasteboard write from being rebroadcast to Windows by the change-count poller.

  **Ctrl-C on a folder in Windows Explorer is a known no-op** — not our bug, and not fixable from the server side. Explorer puts `CFSTR_SHELLIDLIST` (Shell IDList Array) on the clipboard as the primary format and delay-renders `FileGroupDescriptorW` only when a shell-aware receiver asks. mstsc doesn't request the delayed format, so it never forwards anything via CLIPRDR — `cliprdr=debug` shows zero PDUs for the folder copy attempt. Workaround for the user: enter the folder in Explorer, `Ctrl-A` then `Ctrl-C` to copy the contents (with directory descriptors for any subfolders) — that path uses `FileGroupDescriptorW` directly and forwards correctly. True drag-from-Windows folder copy would need drive redirection (a different RDP feature, not clipboard).
- Forward macOS system audio to the remote (RDPSND, 44.1 kHz stereo 16-bit PCM; SCK captures at 48 kHz and the capture loop resamples via `rubato`). Optionally compress it as **AAC-LC** (`--enable-aac`, `WAVE_FORMAT_AAC_MS` over RDPSND, ~128 kbps vs PCM's ~1.4 Mbit/s) — AudioToolbox-encoded (`src/aac.rs`), raw access units, advertised ahead of PCM so clients that decode AAC negotiate it while everyone else falls back to PCM automatically. Opt-in because AAC adds ~40–50 ms of encoder priming latency, so PCM stays the zero-latency LAN default. See the AAC quirk note below.
- NLA / CredSSP authentication — no more "type username before Connect" mstsc workaround.
- Optionally attach a **headless virtual display** (`--virtual-display --width W --height H`) and serve that to the client instead of mirroring the primary panel — behaves like plugging in an external monitor, so the local Mac screen stays available while the remote session has its own desktop at any requested resolution. Backed by undocumented `CGVirtualDisplay*` private API; see the maintenance note below.
- Optionally go **fully headless while a client is connected** via one of two mechanisms (mutually exclusive):
  - `--virtual-display ... --detach-primary`: disables every active physical display at the WindowServer level once the first RDP client actually connects (private `CGSConfigureDisplayEnabled`). Backlight off, no menu bar, cursor can't cross over. Cleaning a stale detach if macrdp dies hard happens automatically — the detach uses `CGConfigureForAppOnly` so SIGKILL / panic / power loss trigger an OS-level revert with no logout required. **Caveat:** on some macOS versions / displays the disable transaction succeeds but the panel keeps showing the desktop; if that's the case, use `--capture-primary` instead.
  - `--virtual-display ... --capture-primary`: takes exclusive `CGDisplayCapture` of every physical display once a client connects AND forces each panel's gamma LUT to map every input to black via `CGSetDisplayTransferByFormula(_, 0,0,1, 0,0,1, 0,0,1)`. Capture alone doesn't visually blank modern macOS panels (the "fill with black on capture" semantic disappeared around 10.10) — the gamma trick is what actually makes the panel render solid black while the WindowServer keeps compositing the desktop to it. Backlight stays on, cursor sunk by the capture. Both gamma changes and capture tokens are process-scoped, so SIGKILL / panic auto-restores. Uses only public CG symbols — no private SkyLight surface, no `CGError 1001` window.
  Either way, the original layout is restored the moment the last client disconnects; local Mac usage is normal whenever no one is connected.

Not yet implemented: multi-monitor (client-side multi-display), non-US keyboard layouts, drive/printer redirection.

## Project goal

A native RDP server for macOS written in Rust on top of [`ironrdp`](https://github.com/Devolutions/IronRDP). Functionally analogous to `xrdp` on Linux: Windows / cross-platform RDP clients connect to the Mac and see its desktop, with keyboard/mouse forwarded back.

Not a client, not a VNC bridge, not a proxy — the server terminates the RDP protocol itself and renders/feeds the local macOS session.

@docs/architecture.md

@docs/macos-gotchas.md

@docs/known-quirks.md

## Commands

```bash
cargo build                    # debug build
cargo build --release          # release build (LTO, ~30s)
cargo run                      # prompts for password, runs against PAM
cargo run -- --skip-auth --password test  # bypass PAM for quick tests
cargo run -- --virtual-display --width 1920 --height 1080  # headless remote desktop, local screen untouched
cargo test                     # run all tests
cargo clippy --all-targets -- -D warnings  # lint as errors
cargo fmt                      # format
RUST_LOG=debug cargo run       # crank logging for troubleshooting
```

Useful CLI flags (see `src/main.rs::Args` for the full set):
```
--bind 0.0.0.0:3390       # listen address
--username NAME           # default: $USER
--password PASS           # avoid the interactive prompt (logs are warned)
--skip-auth               # bypass PAM (also skips password validation)
--width  / --height       # override autodetected display size
--hidpi                   # capture the primary display at backing (Retina) pixels
                          #   instead of logical points (~4x pixels; crisp; best
                          #   with --enable-h264). Ignored with --width/--height
                          #   or --virtual-display. macOS-only.
--fps N                   # default 60 with --enable-h264, else 15
--enable-h264             # stream H.264 over EGFX (AVC420) instead of legacy bitmaps
--keyframe-interval SECS  # periodic IDR safety net (default 2; only with --enable-h264)
--flush-frames N          # trailing skip-P-frames re-sent after each change to drain
                          #   mstsc's presentation buffer (default 4; 0 disables; --enable-h264)
--enable-aac              # Compress RDPSND audio as AAC-LC (WAVE_FORMAT_AAC_MS)
                          #   instead of raw PCM; ~11x less bandwidth. PCM fallback is
                          #   automatic for clients without AAC decode. Adds ~40-50 ms
                          #   latency, so off by default.
--aac-bitrate BPS         # AAC target bitrate (default 128000; only with --enable-aac)
--no-lazy-paste           # Opt out of lazy Windows→Mac file paste (default ON).
                          #   Lazy streams bytes on Cmd-V (NSFilePresenter) with native
                          #   "Preparing to paste" progress and lower chunk parallelism;
                          #   --no-lazy-paste reverts to eager download + auto-paste hack.
--cert-dir PATH           # default ~/Library/Application Support/macrdp
```

Testing against the server:
```bash
# FreeRDP — easiest to script and get verbose logs from.
xfreerdp /v:127.0.0.1:3390 /u:$USER /cert:ignore /log-level:DEBUG

# Microsoft Remote Desktop / Windows App.app — closest to real-user UX.
# Windows mstsc: just enter the computer and click Connect — NLA/CredSSP
# is enabled, mstsc will prompt for credentials in its own dialog.
# Expect one "Broken pipe" error in the log on the first attempt: that's
# mstsc's cert-trust prompt closing and reopening the socket. The next
# attempt succeeds.
```

When iterating on the capture/encode path, prefer FreeRDP with `/log-level:DEBUG` — its PDU traces are far more useful than mstsc's silent failures.

## Conventions worth keeping

- Keep `ironrdp` as the only crate that touches RDP wire format. Wrappers around it are fine; parallel parsing/emitting of PDUs is not.
- Per-platform code (capture, input, cursor, clipboard) is feature-gated via `#[cfg(target_os = "macos")]` so the protocol layer remains cross-compilable on Linux CI. Each module has a non-macOS stub for that reason.
- Errors that originate from macOS APIs (`OSStatus`, `CGError`, TCC denials, PAM error codes) should be wrapped with enough context that the user knows *which permission or service* is missing — those are the #1 support question.
- Direct FFI via `extern "C"` is preferred over heavyweight wrapper crates when the call surface is small (see `src/auth.rs::pam_impl`).
- Default log level is `info`; reach for `RUST_LOG=debug` when investigating, don't make debug the default.
