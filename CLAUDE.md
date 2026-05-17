# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

Functional v0. RDP clients (mstsc, Microsoft Remote Desktop, FreeRDP) can:
- Connect over TLS to the Mac on port 3390 with a local Mac username/password.
- See the primary display at native resolution with incremental damage-region updates.
- Drive keyboard and mouse, including modifier keys, mouse buttons, and wheel.
- See the real macOS cursor shape (I-beam, hand, etc.) overlaid by the client.
- Copy/paste UTF-8 text and images (CF_DIB ↔ PNG) between Mac and remote.
- Mac→Windows file copy, including whole folders: copying a file or directory in Finder and pasting on Windows produces a real file/tree in Explorer. The pasteboard walk recurses into directories (skipping symlinks, capped at 10 000 descriptors per copy) and emits one FILEGROUPDESCRIPTORW entry per leaf with `relative_path` set so upstream's wire encoder reconstructs the right `MyFolder\sub\file.txt` cFileName. Bytes stream via MS-RDPECLIP `FileContentsRequest` SIZE + RANGE chunks (4 MiB per chunk). Reaches upstream `Cliprdr::initiate_file_copy` via the vendored `ServerEvent::ClipboardFileCopy(Vec<FileDescriptor>)` variant — that's the only API that populates `local_file_list`, without which upstream short-circuits every byte fetch with CB_RESPONSE_FAIL. Finder hands out *file-reference* URLs (`/.file/id=...`); we resolve them through `NSURL::URLByResolvingSymlinksInPath` because `std::fs::metadata` can't stat them directly.
- Windows→Mac file copy (one or more files; recursive folder copy via Ctrl-C does *not* work — see caveat below): when Windows announces a `FileGroupDescriptorW` we **eagerly** download every entry to `/tmp/macrdp-paste-<pid>-<nanos>/` via parallel `FileContentsRequest` chunks (1 MiB × 8 in flight), recreating any directory structure encoded in each descriptor's `relative_path`, then publish the top-level entries to NSPasteboard as real `NSURL`s. The eager approach is forced because Cocoa's `NSFilePromiseProvider` / `NSFilePromiseReceiver` is drag-and-drop-only — Finder's Cmd-V never calls into a promise delegate. `resolve_dest` path-sanitizes every `relative_path` component (rejects `.`, `..`, embedded `/`) so a malicious remote can't escape the temp sandbox. When the download lands we play `/System/Library/Sounds/Glass.aiff` (`afplay` bypasses notification permissions; `osascript display notification` was silently suppressed because macOS attributes the banner to the unsigned macrdp binary) and, *only if Finder is the frontmost app*, fire `Cmd-V` via System Events so the paste the user attempted finishes automatically. A `SelfChangeCount` atomic stops our own pasteboard write from being rebroadcast to Windows by the change-count poller.

  **Ctrl-C on a folder in Windows Explorer is a known no-op** — not our bug, and not fixable from the server side. Explorer puts `CFSTR_SHELLIDLIST` (Shell IDList Array) on the clipboard as the primary format and delay-renders `FileGroupDescriptorW` only when a shell-aware receiver asks. mstsc doesn't request the delayed format, so it never forwards anything via CLIPRDR — `cliprdr=debug` shows zero PDUs for the folder copy attempt. Workaround for the user: enter the folder in Explorer, `Ctrl-A` then `Ctrl-C` to copy the contents (with directory descriptors for any subfolders) — that path uses `FileGroupDescriptorW` directly and forwards correctly. True drag-from-Windows folder copy would need drive redirection (a different RDP feature, not clipboard).
- Forward macOS system audio to the remote (RDPSND, 44.1 kHz stereo 16-bit PCM; SCK captures at 48 kHz and the capture loop resamples via `rubato`).
- NLA / CredSSP authentication — no more "type username before Connect" mstsc workaround.

Not yet implemented: multi-monitor, non-US keyboard layouts, drive/printer redirection.

## Project goal

A native RDP server for macOS written in Rust on top of [`ironrdp`](https://github.com/Devolutions/IronRDP). Functionally analogous to `xrdp` on Linux: Windows / cross-platform RDP clients connect to the Mac and see its desktop, with keyboard/mouse forwarded back.

Not a client, not a VNC bridge, not a proxy — the server terminates the RDP protocol itself and renders/feeds the local macOS session.

## Architecture

```
src/main.rs       CLI, TCC preflight, TLS cert mgmt, RdpServer assembly
src/auth.rs       Startup PAM auth against the macOS account (libpam FFI)
src/capture.rs    ScreenCaptureKit → BgrA32 BitmapUpdate, dirty-rect driven
src/cursor.rs     NSCursor → RGBAPointer, hashed for change detection
src/input.rs      RDP scancodes/mouse PDUs → CGEvent synthesis (US ANSI)
src/clipboard.rs  CLIPRDR ↔ NSPasteboard (CF_UNICODETEXT + CF_DIB
                  + Mac↔Windows file copy via FileGroupDescriptorW
                  and FileContentsRequest streaming)
src/file_promise.rs  Windows→Mac eager download to /tmp + NSPasteboard
                     publish + Glass-chime auto-paste into Finder
src/audio.rs      RDPSND ← second SCK stream with system-audio capture,
                  rubato 48→44.1 kHz resample, latency-bounded
build.rs          Bakes Xcode Swift-runtime rpath into the final binary

vendor/ironrdp-server/    Local fork of ironrdp-server 0.10.0, pulled in
                          via [patch.crates-io] in Cargo.toml. Single
                          targeted fix in dispatch_server_events: keep the
                          NEWEST queued waves on per-batch overflow instead
                          of the oldest (upstream 0.10.0 keeps oldest, which
                          bakes any dispatch stall into a permanent audio
                          offset). Submitted upstream — delete this vendor
                          dir once it lands in a released version.
```

Cross-cutting:
- **TLS** terminates inside the acceptor; `rustls` with a self-signed cert at `~/Library/Application Support/macrdp/{cert,key}.pem` (generated on first run, persisted thereafter for stable client TOFU). `RdpServerSecurity::Hybrid` is used so the negotiation response advertises CredSSP — the public-key bytes handed to ironrdp are the raw `subjectPublicKey` BIT STRING from the X.509 cert (not the SPKI sequence, not the keypair-derived bytes), since that's what sspi hashes client-side.
- **Auth** at startup: `--username` (defaults to `$USER`) + interactive password prompt → PAM `checkpw` service → set as the static credential ironrdp_server checks per-connection. `--skip-auth` bypasses for dev.
- **Session model** — v0 attaches to the console session of the logged-in user (single session). Multi-session / headless virtual displays would need a private framebuffer and are out of scope.
- **Signal handling** — `main.rs` spawns a task that awaits SIGINT/SIGTERM and `std::process::exit(0)`s. Without it, ScreenCaptureKit's framework threads can leave the process unkillable by Ctrl-C once an SCStream is active.
- **Audio rate** — SCK only supports 8/16/24/48 kHz, so capture is at 48 kHz, but `src/audio.rs` resamples to 44.1 kHz via `rubato` before sending. 44.1 matches the native rate of most Windows audio endpoints, so the client plays directly without internal resampling — which used to cause a ~20% sustained over-feed and multi-second audio backlogs on mstsc. The advertised RDPSND `AudioFormat` is therefore 44.1 kHz / 2 ch / 16-bit.
- **Single capture loop** — `MacRdpsnd` (the audio factory) holds an `Arc<AtomicU64>` generation counter shared with every backend it builds. Each `start()` claims a fresh generation; older capture loops observe the bump on their next iteration and exit. Without this, an mstsc cert-prompt reconnect leaves the first capture loop running while the second starts, both feeding the shared event channel → ~2× audio reaching the client.

When adding a feature, locate it in one of those modules first; if it spans them (e.g., a new virtual channel), it belongs in a dedicated module alongside `clipboard.rs`, driven by `ironrdp_server`'s factory traits.

## macOS-specific gotchas

- **Screen Recording permission** (TCC) is required for ScreenCaptureKit. Granted in System Settings → Privacy & Security → Screen Recording.
- **Accessibility permission** is required to post synthetic keyboard/mouse events via `CGEventPost`. Granted in System Settings → Privacy & Security → Accessibility. Without it, posted events are silently dropped.
- **TCC grants are path-keyed AND signature-keyed.** `target/debug/macrdp` and `target/release/macrdp` are tracked separately. An *unsigned* rebuild at the same path also invalidates the grant — every fresh link gets a different identity. Ad-hoc sign the release binary (`codesign -s - --force target/release/macrdp`) to get a stable code-signature identity so the grant survives rebuilds. Cargo doesn't have a post-link hook, so do it manually or via a wrapper script.
- **Posting events to the login window or secure-input contexts** (password fields, lock screen) is blocked by the OS and cannot be worked around — document the limitation rather than fighting it.
- **Default RDP port 3389 is privileged**; bind 3389 only with elevated rights, otherwise default to 3390 in dev.
- **OpenPAM, not Linux-PAM.** `checkpw` uses `use_first_pass`, so `pam_opendirectory` reads the password from `pam_set_item(PAM_AUTHTOK, ...)` and never invokes the conv callback. See `src/auth.rs`.

### Known behavioural quirks

- **Server-side upscale breaks SCK damage tracking.** Passing `--width N --height M` when N×M ≠ the Mac's native size makes SCK scale internally and only emit dirty-rects for changed source regions; the rest of the upscaled output buffer goes stale and the client only refreshes where the cursor passes. Fix: leave width/height unset (autodetected) and use the client's smart-sizing if you want a different window size.
- **No server PointerPosition forwarding (intentional).** mstsc and Microsoft Remote Desktop local-predict the cursor from the user's mouse input; any `PointerPositionAttribute` PDU we send arrives one encode-plus-network round-trip late and snaps the cursor back to a stale position on fast moves. The current design (`poll_shape` only, `poll_position` defined but unwired) is deliberately the right one for all interactive use. *Do not* simply wire `poll_position` up — that's the bug, not the fix. The only thing this design misses is Mac-side *programmatic* cursor moves (an app calling `CGWarpMouseCursorPosition`, rare); fixing that properly requires emitting position only when the Mac cursor diverges from where the last RDP mouse event left it and enough time has passed since that event, which needs shared state between `input.rs` and `cursor.rs`.
- **Codec mismatch with Microsoft Remote Desktop on macOS.** That client only offers NSCodec; the server advertises RemoteFx/QOI. They fall back to legacy BitmapUpdate, which works but is bandwidth-heavy.

## Commands

```bash
cargo build                    # debug build
cargo build --release          # release build (LTO, ~30s)
cargo run                      # prompts for password, runs against PAM
cargo run -- --skip-auth --password test  # bypass PAM for quick tests
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
--fps N                   # default 15
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
