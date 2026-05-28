# vendor/ironrdp-server — divergence log

Local fork of ironrdp-server 0.10.0, pulled in via `[patch.crates-io]` in
`Cargo.toml`. The audio-lag control in `dispatch_server_events` is the live
divergence. Keep this vendor dir until (2)/(3)/(4)/(5)/(6)/(7) below are
upstreamed AND released — #1276 landing is NOT sufficient.

(1) The original "keep newest queued waves on per-batch overflow"
    direction-flip LANDED upstream (PR #1276, merged 2026-05-21) — do NOT
    treat that as the reason this fork exists; it's superseded locally by (2).

(2) Cross-batch audio-lag tracker (NOT upstreamed): replaces the per-batch cap
    with a cumulative buffer-depth model (`audio_shipped_ms` vs wall-clock
    `audio_clock_start` on `RdpServer`) so slow drift from many small client
    pauses is caught, not just one big stall. Drops oldest waves when the
    projected client buffer would exceed `MAX_LAG_MS` (200).

(3) Resize-stall resync (NOT upstreamed): when the writer stalls (mstsc
    freezing the socket during a window resize/move/fullscreen-toggle blocks on
    the EGFX video frames queued ahead of the waves on the SAME ServerEvent
    channel — H.264-only, legacy bitmaps coalesce through `dispatch_display`),
    wall-clock outruns `audio_shipped_ms` and the (2) model would read it as
    "client starving" and ship the whole stale backlog late, bloating the
    buffer and compounding each stall. Fix: if deficit
    (`real_elapsed - audio_shipped`) > 300 ms, resync `audio_shipped_ms` to
    live so the backlog is dropped to one `MAX_LAG_MS` of the freshest waves.

(4) Per-batch dispatch priority (NOT upstreamed): in `dispatch_server_events`,
    stably partition the drained batch so CLIPRDR events are written BEFORE any
    EGFX video frames in that batch. Without this, with `--enable-h264` a
    CLIPRDR FileContentsResponse queues behind dozens of large video frames
    every batch, throttling clipboard file copies to a crawl and freezing
    Windows Explorer's synchronous paste read. Audio is intentionally LEFT in
    arrival order (interleaved with EGFX) — an earlier version of this patch
    lumped audio in with clipboard as "non-EGFX" and burst-shipped each batch's
    waves in a clump, which made the client's adaptive jitter buffer extend and
    added a few hundred ms of steady-state playback latency. Partition is
    stable: H.264 inter-frame sequence and audio wave-drop ordering are both
    preserved. Gated on the egfx feature.

(5) SuppressOutput / RefreshRectangle handling (upstream PR #1319, awaiting
    review): in `handle_io_channel_data`, pattern-match the two PDUs instead of
    warn-and-drop, and flip a shared `Arc<AtomicBool> display_suppressed`
    (exposed via `display_suppressed_handle()` and overridable via
    `set_display_suppressed_handle()` so macrdp can share one flag with
    capture.rs's gate). Without honoring SuppressOutput, a minimized mstsc
    accumulates EGFX frames; the refocus chew-through locks up its input
    dispatch for seconds. See the "Honoring SuppressOutput..." quirk in
    docs/known-quirks.md for the client-side trap (first-frame arming gate +
    per-connection reset).

(6) NSCodec encoder + selection (upstream PR #1332, awaiting review): adds
    `mod nscodec;` in `encoder/mod.rs` (the file was previously dead code,
    never wired up), an `NsCodecHandler` that calls
    `nscodec::encode(bitmap, color_loss_level)`, a `nscodec: Option<(u8, u8)>`
    slot on `UpdateEncoderCodecs` with a matching `set_nscodec`, a
    `BitmapUpdater::NsCodec` dispatch variant, a selection arm below RemoteFX in
    `UpdateEncoder::new`, a `has_nscodec()` on `RdpServerOptions`, and an active
    `CodecProperty::NsCodec` server-side match arm that re-uses the client's
    confirmed CLL. Verified against the macOS Microsoft Remote Desktop / Windows
    App client — that client's legacy codec list contains only NSCodec, so
    before this wiring it silently fell through to raw BitmapUpdate at much
    higher bandwidth. The new `NsCodecHandler::new` emits a `debug!` line
    ("NSCodec encoder selected for this session") so codec selection is visible
    at `RUST_LOG=...ironrdp_server::encoder=debug`. Modern FreeRDP loads the
    NSCodec decoder module at connect but doesn't advertise the codec back in
    `ClientConfirmActive`, so xfreerdp/sfreerdp don't exercise this path — only
    the macOS Microsoft Remote Desktop / Windows App does today. Upstream PR
    shape: same wiring but the encoder lives in a new `ironrdp-nscodec` peer
    crate (CBenoit's architecture preference), gated by a new `nscodec` feature
    on `ironrdp-server`; here the vendor uses the in-tree
    `vendor/ironrdp-server/src/encoder/nscodec.rs` directly with no feature gate.

(7) Opt-in QOI Rgb-only workaround for pre-PR-#1335 `ironrdp-session` clients
    (process-global `QOI_FORCE_RGB: AtomicBool`, public setter
    `set_qoi_force_rgb`, wired through macrdp's `--qoi-force-rgb` CLI flag —
    default OFF, so `qoi_encode` emits the natural `*a` `qoi::RawChannels`
    matching the source PixelFormat, identical to upstream). When the flag is
    set, every 4-byte input maps to its `*x` sibling so the QOI header
    advertises `Channels::Rgb` instead of `Channels::Rgba`. Context: upstream
    `ironrdp-session`'s `fast_path.rs::qoi_apply` Rgba arm is
    `warn!("Unsupported RGBA QOI data")` and drops the frame, so any client
    carrying that code negotiates QOI, gets `Rgba`, and renders blank (412
    RGBA-warn lines in ~12s on the loopback repro). PR #1335 upstreams the Rgb
    behaviour as the default; a companion client-side patch (separate branch
    `feat-client-rgba-qoi` on the clintcan fork, not yet PR'd) adds Rgba decode
    to `ironrdp-session`. Once the client patch lands and a release ships, the
    workaround + `--qoi-force-rgb` can be deleted. Until then, users pointing
    `ironrdp-viewer` at macrdp should pass `--qoi-force-rgb`; mstsc / MS Remote
    Desktop / Windows App / FreeRDP don't advertise QOI and are unaffected.
