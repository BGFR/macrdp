# Architecture

```
src/main.rs       CLI, TCC preflight, TLS cert mgmt, RdpServer assembly
src/auth.rs       Startup PAM auth against the macOS account (libpam FFI)
src/capture.rs    ScreenCaptureKit → BgrA32 BitmapUpdate, dirty-rect driven
src/cursor.rs     NSCursor → RGBAPointer, hashed for change detection
src/input.rs      RDP scancodes/mouse PDUs → CGEvent synthesis (US ANSI),
                  per-side modifier state with NX_DEVICE bits, Caps Lock
                  toggle, AX-driven symbolic-hotkey workarounds
                  (Cmd+Tab app cycle, Cmd+` window cycle, Spotlight,
                  screencapture) since WindowServer's symbolic-hotkey
                  dispatcher won't fire for CGEventPost
src/clipboard.rs  CLIPRDR ↔ NSPasteboard (CF_UNICODETEXT + CF_DIB
                  + Mac↔Windows file copy via FileGroupDescriptorW
                  and FileContentsRequest streaming)
src/file_promise.rs  Windows→Mac EAGER download to /tmp + NSPasteboard
                     publish + Glass-chime auto-paste into Finder.
                     Default path; provides fetch_one_file (Arc<File>+
                     pwrite chunk fan-out) reused by the lazy path.
src/file_promise_lazy.rs  Windows→Mac LAZY paste (default; opt out
                          with --no-lazy-paste): pre-
                          sized empty temp file per leaf + one
                          NSFilePresenter each via NSFileCoordinator;
                          on Finder Cmd-V, relinquishPresentedItemToReader:
                          blocks while we fetch_one_file with
                          LAZY_PARALLEL_CHUNKS (2, vs eager's 8) so RDP
                          input stays responsive during the download.
                          macOS shows native "Preparing to paste" progress;
                          no Glass chime / auto-Cmd-V needed.
                          cleanup_on_disconnect drains presenters + temp
                          dir on Drop(MacCliprdrBackend); shutdown_cleanup
                          does the same via a process-global handle for
                          signal exit (which bypasses Drop).
src/runloop_thread.rs  Dedicated CFRunLoop-hosting std::thread with a
                       submit(closure) API. Exists because tokio owns
                       macrdp's main thread (no pumped runloop), and
                       NSFileCoordinator.addFilePresenter / removeFilePresenter
                       calls must land on a thread with a pumped CFRunLoop
                       to deliver presenter callbacks. Wakes via a custom
                       CFRunLoopSource; one shared thread for the process
                       lifetime, started lazily on first submit().
src/audio.rs      RDPSND ← second SCK stream with system-audio capture,
                  rubato 48→44.1 kHz resample, latency-bounded. Ships raw
                  PCM by default, or AAC-LC via src/aac.rs (--enable-aac).
src/aac.rs        AudioToolbox AAC-LC encoder (--enable-aac). AudioConverter
                  FFI: interleaved i16 PCM → raw AAC access units for the
                  WAVE_FORMAT_AAC_MS RDPSND path. macOS-only.
src/virtual_display/    Opt-in headless display via undocumented
  mod.rs                CGVirtualDisplay* private API. Public Rust
  private_api.rs        surface is `VirtualDisplay::new(w,h,hz)` +
                        display_id/origin_pts/size_pts; ALL touches to
                        private Obj-C classes/symbols are confined to
                        private_api.rs (the maintenance boundary —
                        when Apple changes the API in a future macOS,
                        update only that file).
src/h264.rs       EGFX/H.264 video pipeline (opt-in via --enable-h264).
                  Bridges the VideoToolbox encoder (src/videotoolbox.rs) to
                  upstream's GraphicsPipelineServer: per SCK frame, encode →
                  non-blocking drain → AVC420 (Annex-B framing) → DRDYNVC →
                  ServerEvent::Egfx. Uses upstream's auto-allocated surface id
                  (see the mstsc reconnect-blank quirk note below). Falls back
                  to legacy BitmapUpdate for clients that don't advertise
                  AVC420 decode.
src/videotoolbox.rs  VideoToolbox H.264 encoder (AVCC NALs + SPS/PPS).
                  Feeds VT a full-range BT.709 NV12 (420f) buffer it builds
                  from the captured BGRA — VT would otherwise emit video-range
                  YUV, which mstsc renders washed-out. The BGRA→NV12 conversion
                  is vImage (Accelerate/NEON) accelerated, ~24-32x over the
                  scalar reference kept as a fallback + benchmark baseline.
build.rs          Bakes Xcode Swift-runtime rpath into the final binary

vendor/ironrdp-server/    Local fork of ironrdp-server 0.10.0, pulled in via
                          [patch.crates-io] in Cargo.toml. The live
                          divergences (audio-lag tracker, resize-stall
                          resync, per-batch dispatch priority, SuppressOutput
                          handling, NSCodec encoder, opt-in QOI Rgb
                          workaround) are documented in
                          vendor/ironrdp-server/CLAUDE.md — that nested
                          memory loads when you work in the fork. Keep the
                          vendor dir until all of those are upstreamed AND
                          released.

(vendor/ironrdp-egfx/     DELETED 2026-05-25. The CapabilitySet::decode
                          tolerance fix was merged upstream as PR #1298
                          (Devolutions/IronRDP@67f3c63). We bumped the
                          ironrdp rev in [patch.crates-io] to that commit
                          and the vendor dir is gone. If you're seeing this
                          comment in a stale checkout — the dir is not
                          missing, it just stopped existing.)

(vendor/ironrdp-cliprdr/  DELETED 2026-05-25. All THREE divergences
                          (on_format_list_response hook #1300,
                          Preferred DropEffect advertise+inline-response
                          #1301, always-SHOW_PROGRESS_UI FD flag #1299)
                          merged upstream the same day. Ironrdp pinned
                          at-or-after Devolutions/IronRDP@879ffed8 has
                          all three; vendor dir gone. If you're seeing
                          this comment in a stale checkout — the dir is
                          not missing, it just stopped existing.)
```

Cross-cutting:
- **TLS** terminates inside the acceptor; `rustls` with a self-signed cert at `~/Library/Application Support/macrdp/{cert,key}.pem` (generated on first run, persisted thereafter for stable client TOFU). `RdpServerSecurity::Hybrid` is used so the negotiation response advertises CredSSP — the public-key bytes handed to ironrdp are the raw `subjectPublicKey` BIT STRING from the X.509 cert (not the SPKI sequence, not the keypair-derived bytes), since that's what sspi hashes client-side.
- **Auth** at startup: `--username` (defaults to `$USER`) + interactive password prompt → PAM `checkpw` service → set as the static credential ironrdp_server checks per-connection. `--skip-auth` bypasses for dev.
- **Session model** — by default macrdp attaches to the console session of the logged-in user (single session, mirrors the primary panel). With `--virtual-display --width W --height H`, the server instead allocates a headless `CGVirtualDisplay` and serves *that*; the local Mac screen is untouched and the remote sees its own desktop at the requested resolution. The CG-side display is owned by `main()`'s scope, registered via `[CGVirtualDisplay initWithDescriptor:]` + `applySettings:`, and torn down on normal exit (signal-driven `std::process::exit(0)` skips Drop, but macOS reaps the registration when the owning process dies). Capture / input / cursor all parameterize on `(displayID, origin_pts, size_pts)` so they target the right surface regardless of which path is in effect.
- **Signal handling** — `main.rs` spawns a task that awaits SIGINT/SIGTERM and `std::process::exit(0)`s. Without it, ScreenCaptureKit's framework threads can leave the process unkillable by Ctrl-C once an SCStream is active.
- **Audio rate** — SCK only supports 8/16/24/48 kHz, so capture is at 48 kHz, but `src/audio.rs` resamples to 44.1 kHz via `rubato` before sending. 44.1 matches the native rate of most Windows audio endpoints, so the client plays directly without internal resampling — which used to cause a ~20% sustained over-feed and multi-second audio backlogs on mstsc. The advertised RDPSND `AudioFormat` is therefore 44.1 kHz / 2 ch / 16-bit.
- **Single capture loop** — `MacRdpsnd` (the audio factory) holds an `Arc<AtomicU64>` generation counter shared with every backend it builds. Each `start()` claims a fresh generation; older capture loops observe the bump on their next iteration and exit. Without this, an mstsc cert-prompt reconnect leaves the first capture loop running while the second starts, both feeding the shared event channel → ~2× audio reaching the client.

When adding a feature, locate it in one of those modules first; if it spans them (e.g., a new virtual channel), it belongs in a dedicated module alongside `clipboard.rs`, driven by `ironrdp_server`'s factory traits.
