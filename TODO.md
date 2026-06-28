# TODO / work queue

A living checklist of what's open, deferred, or parked. Detail lives in the
linked docs / vendored `CLAUDE.md`s / commit history — this is just the index of
"what's currently to be made." Keep it pruned: move items to *Done* only briefly,
then delete; promote a parked item to *In flight* when work actually starts.

## In flight (needs an action)

_(nothing in flight — see Deferred/Parked below)_

## Deferred — scoped, not started

- [ ] **UDP multitransport Phase 2 — lossy `UdpFecL` + DTLS + 1+1 redundancy** (FEC dropped).
  Phase 1 (reliable EGFX-over-UDP) shipped (v0.8.15, clean-link only). Phase 2 wants
  loss resilience. Status: lossy delivery mode + 1+1 duplicate-send (repetition code)
  exist in `ironrdp-rdpeudp` behind `MACRDP_UDP_LOSSY_AUDIO_DUP`; DTLS via `boring`
  not wired. Remaining: wire DTLS, soak the 1+1 lossy-audio path under loss.
- [x] ~~**Reed-Solomon FEC** (RDPEUDP v1 `UDPFECL`)~~ — **CLOSED, structural NO-GO.**
  FEC is a dead/legacy feature across the *whole* RDP ecosystem, not just a macrdp gap
  (2026-06-28 industry survey): the only stack that ever shipped FEC is Microsoft's
  RDP-8.x/RemoteFX-over-UDP server (v1 lossy); **Microsoft removed FEC in RDPEUDP2** and
  modern Windows (RDP 10+, AVD Shortpath) negotiates RDPUDP2 → retransmit-only, **never
  FEC** (confirmed by our own zero-FEC capture). FreeRDP deliberately skipped it
  (RDPUDP2-only). So **no current client would decode macrdp-emitted FEC** — building the
  encoder is pointless. The 1+1 redundancy stand-in (above) is the only loss-resilience
  lever reachable for a modern client. Would only reopen with legacy-Windows-8.x test
  machines, which isn't a realistic target. See the "Industry status" + "P2.3 FEC capture
  RESULT" notes in `docs/rdp-udp-multitransport-feasibility.md` + `vendor/ironrdp-rdpeudp/CLAUDE.md`.

- [ ] **Generic USB redirection (MS-RDPEUSB).**
  Viable path scoped: user-space virtual USB host controller via `IOUSBHostControllerInterface`/
  `AppleUSBUserHCI` (no kext/dext). **Blocked on Apple:** entitlement
  `com.apple.developer.usb.host-controller-interface` submitted 2026-06-24 (FB23363880),
  awaiting grant. Then: MS-RDPEUSB server-direction + UserHCI; gates to a signed build.
  See `docs/usb-redirection-feasibility.md`.

- [ ] **Multi-monitor (client-side multi-display).**
  Extend `--virtual-display` to N monitors. **Blocker:** the git-pinned `ironrdp-acceptor`
  hardcodes a single-monitor `MonitorLayoutPdu`, so true separate monitors needs an acceptor
  change. Open design question: true-monitors vs one-big-desktop vs phased. (PAUSED, no code.)

## Parked — scoped, low priority

- [ ] **Auto-sized virtual display.** Resize the vdisplay to the client's resolution on
  connect (avoids the mirror-primary scaling lag). Risk: does `CGVirtualDisplay applySettings`
  live-resize cleanly or need recreate? Needs a real mstsc test. (No code.)

- [ ] **`cycle_apps` lock nesting (`CYCLE_SESSION` → `mru`).** Currently safe by consistent
  acquire order; de-nest as a follow-up to the PR #84 hardening if revisiting that area.

- [ ] **Auto-mute on silence (audio-only).** Long-idle YouTube unpause loses audio
  (Windows audiodg suspends after hours of digital silence). Must be audio-only (not the
  shared `display_suppressed` gate, which would freeze the desktop).

- [ ] **AVC444 (4:4:4 chroma).** YUV-pack module + bench scaffold landed; `--avc444` not
  wired. VT hw-encoder serializes → 1080p-comfortable, 4K doesn't fit. Resume only if
  colored-text quality becomes a pain.

## Not planned

- **Printer redirection (RDPDR printer device).** Not implemented; no current demand.

## Upstreaming watch (no action unless a release lands)

- IronRDP forks are effectively permanent (each carries un-upstreamed divergences:
  multitransport, rdpdr server-direction, smartcard, acceptor KLID+MT, audio-lag/resize/dispatch).
  Two PRs still open upstream: **#1359** (rdpsnd) and **#1373** (acceptor honor-size).
  Nothing is currently de-vendorable. See `project_upstream_ironrdp_open_prs` memory.
