# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

Functional v0. RDP clients (mstsc, Microsoft Remote Desktop, FreeRDP) can:
- Connect over TLS to the Mac on port 3390 with a local Mac username/password.
- See the primary display at native resolution with incremental damage-region updates.
- Drive keyboard and mouse, including modifier keys, mouse buttons, and wheel.
- See the real macOS cursor shape (I-beam, hand, etc.) overlaid by the client.
- Copy/paste UTF-8 text and images (CF_DIB ↔ PNG) between Mac and remote.
- Forward macOS system audio to the remote (RDPSND, 44.1 kHz stereo 16-bit PCM).
- NLA / CredSSP authentication — no more "type username before Connect" mstsc workaround.

Not yet implemented: multi-monitor, codec negotiation (NSCodec / RemoteFx), non-US keyboard layouts, file clipboard, drive/printer redirection.

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
src/clipboard.rs  CLIPRDR ↔ NSPasteboard (CF_UNICODETEXT + CF_DIB)
src/audio.rs      RDPSND ← second SCK stream with system-audio capture
build.rs          Bakes Xcode Swift-runtime rpath into the final binary
```

Cross-cutting:
- **TLS** terminates inside the acceptor; `rustls` with a self-signed cert at `~/Library/Application Support/macrdp/{cert,key}.pem` (generated on first run, persisted thereafter for stable client TOFU). `RdpServerSecurity::Hybrid` is used so the negotiation response advertises CredSSP — the public-key bytes handed to ironrdp are the raw `subjectPublicKey` BIT STRING from the X.509 cert (not the SPKI sequence, not the keypair-derived bytes), since that's what sspi hashes client-side.
- **Auth** at startup: `--username` (defaults to `$USER`) + interactive password prompt → PAM `checkpw` service → set as the static credential ironrdp_server checks per-connection. `--skip-auth` bypasses for dev.
- **Session model** — v0 attaches to the console session of the logged-in user (single session). Multi-session / headless virtual displays would need a private framebuffer and are out of scope.

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
- **No server PointerPosition forwarding.** mstsc local-predicts the cursor from its own input; our server-side position updates lag the prediction and snap it back on fast moves. `cursor.rs::poll_position` exists but is unwired — see the comment in `cursor.rs::poll`.
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
