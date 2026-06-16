# vendor/ironrdp-server — divergence log

Local fork of ironrdp-server 0.10.0, pulled in via `[patch.crates-io]` in
`Cargo.toml`. The audio-lag control in the dedicated `dispatch_audio` task
(carved out of `dispatch_server_events`) is the live divergence. Keep this
vendor dir until (2)/(3)/(4)/(5)/(6)/(7)/(8)/(9)/(10)/(11) below are upstreamed
AND released — #1276 landing is NOT sufficient.

(1) The original "keep newest queued waves on per-batch overflow"
    direction-flip LANDED upstream (PR #1276, merged 2026-05-21) — do NOT
    treat that as the reason this fork exists; it's superseded locally by (2).

(2) Cross-batch audio-lag tracker (NOT upstreamed): replaces the per-batch cap
    with a cumulative buffer-depth model (`audio_shipped_ms` vs wall-clock
    `audio_clock_start`) so slow drift from many small client pauses is caught,
    not just one big stall. Drops oldest waves when the projected client buffer
    would exceed `MAX_LAG_MS` (200). The model + the Wave dispatch itself now
    live task-local in the dedicated `dispatch_audio` task in `client_loop`
    (audio was carved out of `dispatch_server_events` onto its own bounded
    `mpsc` channel via `SoundServerFactory::set_audio_sender`); the former
    `RdpServer::{audio_shipped_ms, audio_clock_start}` fields are dead state.

(3) Resize-stall resync (NOT upstreamed): when the writer stalls (mstsc
    freezing the socket during a window resize/move/fullscreen-toggle blocks an
    EGFX video `write_all` while it holds the shared socket-writer mutex; audio
    rides its own channel + `dispatch_audio` task but every `audio_writer.
    write_all` serializes on that SAME socket lock — H.264-only, legacy bitmaps
    don't contend the same way: dirty-rect + intermittent, coalesced through
    `dispatch_display`),
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
    Windows Explorer's synchronous paste read. Audio is deliberately NOT part of
    this batch at all — it ships per-wave in arrival order from the dedicated
    `dispatch_audio` task (its own channel), preserving the natural ~21 ms
    cadence; an earlier version of this patch lumped audio in with clipboard as
    "non-EGFX" and burst-shipped each batch's waves in a clump, which made the
    client's adaptive jitter buffer extend and added a few hundred ms of
    steady-state playback latency. The clipboard/EGFX partition is stable (H.264
    inter-frame sequence preserved); the audio wave-drop ordering is preserved
    independently in `dispatch_audio`. Gated on the egfx feature.

(5) SuppressOutput / RefreshRectangle handling (upstream PR #1319 ✅ MERGED
    2026-05-27, commit `aa7ff679` — not yet released; vendor stays until a
    published release carries it): in `handle_io_channel_data`, pattern-match the two PDUs instead of
    warn-and-drop, and flip a shared `Arc<AtomicBool> display_suppressed`
    (exposed via `display_suppressed_handle()` and overridable via
    `set_display_suppressed_handle()` so macrdp can share one flag with
    capture.rs's gate). Without honoring SuppressOutput, a minimized mstsc
    accumulates EGFX frames; the refocus chew-through locks up its input
    dispatch for seconds. See the "Honoring SuppressOutput..." quirk in
    docs/known-quirks.md for the client-side trap (first-frame arming gate +
    per-connection reset).

(6) NSCodec encoder + selection (upstream PR #1332 ✅ MERGED 2026-06-01, commit
    `54af8f67` — but NOT yet released; this vendor copy stays until a published
    `ironrdp-server` + `ironrdp-nscodec` release carries it): adds
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
    the macOS Microsoft Remote Desktop / Windows App does today. Upstream shape
    (as merged): same wiring but the encoder lives in a dedicated `ironrdp-nscodec`
    peer crate (CBenoit's architecture preference, confirmed in discussion #1322),
    gated by a new `nscodec` feature on `ironrdp-server`; here the vendor uses the
    in-tree `vendor/ironrdp-server/src/encoder/nscodec.rs` directly with no feature
    gate. **Post-release migration:** drop this in-tree wiring and depend on the
    published `ironrdp-nscodec` crate + enable the `nscodec` feature on
    `ironrdp-server`.

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
    RGBA-warn lines in ~12s on the loopback repro). PR #1335 ✅ MERGED 2026-06-01
    (commit `8a9ee626`) upstreams the Rgb behaviour as the default; the companion
    client-side patch landed as PR #1341 ✅ MERGED 2026-06-01 (commit `ef20ea4e`,
    branch `feat-client-rgba-qoi`) adding Rgba decode to `ironrdp-session` (plus a
    size-guard in `qoi_apply` against oversized payloads). Both are MERGED but NOT
    yet released — once a release ships them, the workaround + `--qoi-force-rgb`
    flag (commit `e22a617`) can be deleted. Until then, users pointing
    `ironrdp-viewer` at macrdp should pass `--qoi-force-rgb`; mstsc / MS Remote
    Desktop / Windows App / FreeRDP don't advertise QOI and are unaffected.

(8) AudioWave carries an explicit per-wave duration (NOT upstreamed): the
    `AudioWave` tuple in `src/sound.rs` gained a third field
    `Option<f64> duration_ms`, and the `dispatch_audio` task now uses
    `duration_ms.unwrap_or_else(|| data.len() as f64 / BYTES_PER_MS)` for
    `wave_ms` instead of always deriving it from byte length. Required for the
    `--enable-aac` path in macrdp: a compressed AAC access unit is ~120 bytes
    for ~23 ms of audio, so the hardcoded PCM `BYTES_PER_MS = 176.4` would read
    the projected client buffer as near-empty and silently disable the
    drop-oldest / resync lag control (divergences (2)/(3)). The PCM path passes
    `None` and is byte-for-byte unchanged. Small and upstreamable (generalizes a
    PCM-only constant to any advertised codec); offer it upstream alongside the
    SuppressOutput work. Until then it rides with this fork.

(9) Honor-client-desktop-size plumbing (NOT upstreamed; pairs with the
    `vendor/ironrdp-acceptor` divergence (1)): `RdpServer` gains a
    `honor_client_desktop_size: bool` (default false) + setter
    `set_honor_client_desktop_size`, forwarded in `run_connection` to each
    connection's `Acceptor` via the vendored
    `Acceptor::set_honor_client_desktop_size`. With it set, the acceptor
    adopts the desktop size the client requests in its GCC Client Core Data
    BEFORE Demand Active is sent, so the session is negotiated at the
    client's resolution from the start (no deactivation-reactivation
    resize). The display handler observes the adopted size through the
    existing `request_initial_size` call — conformant clients echo the
    Demand Active size in their Confirm Active bitmap capset, which is also
    why the confirm-active capset alone could never reveal the client's own
    request (verified empirically with sdl-freerdp `/size:1024x768`:
    confirm-active echoed the server's 1512×982). macrdp wires this from
    its default-on client-resolution auto-adopt (`--no-client-resolution`
    opts out). Offer upstream together with the acceptor change.

(11) Server-side RDPDR (drive redirection) static channel (NOT upstreamed;
    added 2026-06-16; depends on vendored `ironrdp-rdpdr` divergence (1)):
    a new `src/rdpdr.rs` houses `RdpdrServer` (a `SvcServerProcessor` peer to
    the client `Rdpdr`) that drives the MS-RDPEFS init handshake (Server
    Announce → capability exchange → Client-ID Confirm → User-Logged-On) and
    surfaces the client's announced devices to a `RdpdrServerHandler` backend,
    plus the `RdpdrServerFactory`/`RdpdrBackendFactory` traits + `AnnouncedDevice`
    (exported from `lib.rs`). Wiring mirrors cliprdr/rdpsnd exactly: a
    `rdpdr_factory: Option<Box<dyn RdpdrServerFactory>>` field on `RdpServer`, a
    `RdpServer::new` param with `set_sender` wiring, attachment in
    `attach_channels` **right after rdpsnd** (MS-RDPEFS requires rdpdr be
    co-advertised with rdpsnd), and `RdpServerBuilder::with_rdpdr_factory`.
    `ironrdp-rdpdr` added to Cargo.toml deps for the wire types. The server's
    static-channel `start()` dispatch (`client_accepted`) ships the Server
    Announce — no extra send path needed for the handshake. Phase 1b added
    device I/O: an `IoRouter` (completion-id → oneshot, like clipboard's
    `DownloadRouter`), an async `RdpdrHandle` (`read_file` = create→read→close,
    wired with the connection's event sender by `build_rdpdr` and handed to the
    backend via `RdpdrServerHandler::set_handle`), a `ServerEvent::Rdpdr`
    variant + dispatch arm (encodes the handle's `SvcMessage`s on the rdpdr
    channel), and `RdpdrServer::process` routing `CoreDeviceIoCompletion`
    responses back to the waiting caller by completion id. `RdpdrHandle` exposes
    `read_file` (create→read→close) and `list_dir` (create→query-directory loop
    until NO_MORE_FILES→close, returning `DirEntry`s).
    Phase 2 added the **write** half of `RdpdrHandle`: `write_file`
    (open→`DeviceWrite`→close), `create_file`/`create_dir` (`DeviceCreate` with
    FILE_OPEN_IF/FILE_CREATE), `remove`/`rename`/`set_len` (open→`SetInformation`
    FileDisposition/FileRename/FileEndOfFile→close), plus a generalized
    `open_with` (explicit `CreateDisposition`; `create_with` now delegates with
    FILE_OPEN) and a `file_write_access()` rights set. These depend on the
    `ironrdp-rdpdr` Phase-2 encode halves (divergence (1) there).
    Read-write; macrdp gates it behind `--enable-drive-redirection`.
    Upstreamable as the server counterpart to the client `Rdpdr`.

(10) Publish the client's keyboard-layout id to a shared cell (NOT
    upstreamed; added 2026-06-16; pairs with `vendor/ironrdp-acceptor`
    divergence (2)): `RdpServer` gains `keyboard_layout: Option<Arc<AtomicU32>>`
    (default None) + setter `set_keyboard_layout_handle`, mirroring the
    `display_suppressed` shared-flag pattern (divergence (5)). In
    `client_accepted`, the server stores `result.keyboard_layout` (the KLID the
    acceptor captured from Client Core Data) into the cell. macrdp hands the
    same `Arc<AtomicU32>` to its `MacInputHandler`, which auto-selects a
    matching non-US keyboard layout when `--keyboard-layout` isn't given
    (`src/keyboard_layout.rs`; US 0x0409 / unknown 0 keep the positional
    keycode path). Additive + matches the existing handle-setter pattern, so
    upstreamable alongside the acceptor change. Verified live: sdl-freerdp
    `/kbd:layout:0x040C` → server logs `client keyboard layout announced
    klid=1036`, input handler logs `auto-selected … layout=com.apple.keylayout.French`.
