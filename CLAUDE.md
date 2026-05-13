# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

The repository is currently empty — no `Cargo.toml`, no source, not yet a git repo. Everything below describes the *intended* project so the first round of scaffolding lands in the right shape. Update this file as soon as real code exists and the assumptions here diverge from reality.

## Project goal

A native RDP server for macOS written in Rust on top of [`ironrdp`](https://github.com/Devolutions/IronRDP). Functionally analogous to `xrdp` on Linux: Windows / cross-platform RDP clients (mstsc, Microsoft Remote Desktop, FreeRDP) connect to the Mac and see its desktop, with keyboard/mouse forwarded back.

This is **not** a client, not a VNC bridge, and not a proxy — the server terminates the RDP protocol itself and renders/feeds the local macOS session.

## Architecture (planned)

The server splits into three concerns that meet at a per-session task:

1. **Protocol layer** — `ironrdp-acceptor` / `ironrdp-server` handle the RDP nego/CredSSP/security handshake, capability exchange, and the bitmap/fastpath output and input PDUs. Treat this as the source of truth for what the wire expects; don't hand-roll PDUs.
2. **Capture layer** — macOS screen frames sourced via **ScreenCaptureKit** (preferred, 12.3+) or CoreGraphics `CGDisplayStream` as a fallback. Frames feed into a bitmap/codec encoder (RDP6 bitmap, or RemoteFX/GFX if `ironrdp` exposes the encoder) and out as fastpath updates.
3. **Input layer** — RDP scancodes/mouse PDUs translated into macOS events via `CGEvent` (`core-graphics` crate) and posted to `kCGHIDEventTap`. Mind the scancode ↔ macOS virtual keycode mapping; it is not 1:1.

Cross-cutting:
- **TLS** terminates inside the acceptor; expect to wire `rustls` with a self-signed cert on first run and document trust-on-first-use for clients.
- **Auth** starts as username/password against local accounts (PAM via `pam` crate, or `dscl`/OpenDirectory), with CredSSP/NLA as a follow-on.
- **Session model** — v0 attaches to the console session of the logged-in user (single session). Multi-session / headless virtual displays require a private framebuffer and are explicitly out of scope for v0.

When adding a feature, locate it in one of those three layers first; if it touches more than one (e.g., clipboard, audio, drive redirection), it belongs in a dedicated *channel* module sitting alongside them, driven by `ironrdp`'s static virtual channel plumbing.

## macOS-specific gotchas

These will bite repeatedly — keep them in mind before debugging "why doesn't it work":

- **Screen Recording permission** (TCC) is required for ScreenCaptureKit / `CGDisplayStream`. The binary must be granted it in System Settings → Privacy & Security → Screen Recording. Running from a fresh build path resets the grant — prefer a stable install location during development.
- **Accessibility permission** is required to post synthetic keyboard/mouse events via `CGEventPost` to other apps' windows. Same TCC caveat.
- **Input Monitoring** may additionally be needed depending on the event tap location.
- TCC prompts only fire for *signed* or *stable-path* binaries; `cargo run` from `target/debug` often silently fails to prompt. When permissions look broken, check `tccutil` and the binary's code signature first.
- Posting events to the **login window** or **secure input** contexts (password fields, lock screen) is blocked by the OS and cannot be worked around — document the limitation rather than fighting it.
- Default RDP port 3389 is privileged; bind 3389 only with elevated rights, otherwise default to something like 3390 in dev.

## Commands

Once `cargo init` has been run:

```bash
cargo build                      # debug build
cargo build --release            # release build
cargo run -- --help              # run the server binary
cargo test                       # run all tests
cargo test <name>                # run a single test by substring
cargo test -- --nocapture        # show stdout/stderr from tests
cargo clippy --all-targets -- -D warnings   # lint, treat warnings as errors
cargo fmt                        # format
```

Testing against the server locally:

```bash
# FreeRDP (brew install freerdp) — easiest to script and get verbose logs from
xfreerdp /v:127.0.0.1:3390 /u:<user> /cert:ignore /log-level:DEBUG

# Microsoft Remote Desktop.app — closest to what real users will use
```

When iterating on the capture/encode path, prefer FreeRDP with `/log-level:DEBUG` — its PDU traces are far more useful than Microsoft Remote Desktop's silent failures.

## Conventions worth keeping

- Keep `ironrdp` as the only crate that touches RDP wire format. Wrappers around it are fine; parallel parsing/emitting of PDUs is not.
- The capture and input layers should be feature-gated (`#[cfg(target_os = "macos")]`) so that at least the protocol layer remains cross-compilable and unit-testable on Linux CI.
- Errors that originate from macOS APIs (`OSStatus`, `CGError`, TCC denials) should be wrapped with enough context that the user knows *which permission* is missing — those are the #1 support question for software like this.
