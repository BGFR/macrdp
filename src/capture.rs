//! Screen capture for the RDP display layer.
//!
//! macOS uses ScreenCaptureKit via the `screencapturekit` crate. Other targets
//! get a static-rectangle stub so the protocol layer still builds and can be
//! exercised on Linux CI.

use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat, RdpServerDisplay,
    RdpServerDisplayUpdates,
};
use tokio::sync::Notify;

use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use crate::cursor::CursorState;

/// Tuning for the H.264 on-demand-keyframe feature (`--keyframe-on-change`).
/// Built from CLI flags in `main.rs`; all percentages are of the full frame.
#[derive(Clone, Copy, Debug)]
pub struct KeyframeOnChange {
    /// Master switch (`--keyframe-on-change` / `--no-keyframe-on-change`).
    pub enabled: bool,
    /// Dirty-area threshold at or above which an immediate keyframe is forced.
    /// A change this large (window raised to front, scroll, app launch) renders
    /// as a big P-frame that some clients resolve cleanly only on the next
    /// periodic IDR; an on-demand IDR lands it at once. Typing/caret changes are
    /// a few percent and stay below this. Fired only on the rising edge (see
    /// `kf_armed`), so this is "a large change *began*", not "is ongoing".
    pub change_pct: u64,
    /// Lowered threshold used briefly after a mouse click. A click often drives a
    /// moderate change (dropdown, dialog, button repaint) below the normal bar;
    /// the IDR still only fires if a real change follows, so no-op clicks cost
    /// nothing.
    pub click_pct: u64,
    /// How long after a click the lowered `click_pct` threshold applies.
    pub click_window: Duration,
}

/// Shared click→keyframe hint. `input.rs` records a mouse-down timestamp; the
/// H.264 capture path lowers its keyframe dirty-area threshold for a short
/// window afterward (see [`KEYFRAME_CHANGE_PCT_POST_CLICK`]). Cheap to clone
/// (one `Arc`); the timestamp is a relaxed atomic since exact ordering doesn't
/// matter for a heuristic.
#[derive(Clone)]
pub struct ClickSignal {
    inner: Arc<ClickInner>,
}

struct ClickInner {
    epoch: Instant,
    /// Milliseconds since `epoch` of the last click; 0 means "never clicked".
    last_click_ms: AtomicU64,
}

impl ClickSignal {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ClickInner {
                epoch: Instant::now(),
                last_click_ms: AtomicU64::new(0),
            }),
        }
    }

    /// Record a click "now". Called from the input handler on mouse-button down.
    pub fn record_click(&self) {
        let ms = self.inner.epoch.elapsed().as_millis() as u64;
        // max(1) so a click at t≈0 isn't mistaken for "never".
        self.inner.last_click_ms.store(ms.max(1), Ordering::Relaxed);
    }

    /// True if the last click was within `window` of now.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn within(&self, window: Duration) -> bool {
        let last = self.inner.last_click_ms.load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        let now = self.inner.epoch.elapsed().as_millis() as u64;
        now.saturating_sub(last) <= window.as_millis() as u64
    }
}

impl Default for ClickSignal {
    fn default() -> Self {
        Self::new()
    }
}

/// Live RDP-session counter. ironrdp_server calls `RdpServerDisplay::updates()`
/// once per accepted client connection and drops the returned stream when the
/// connection ends, so wrapping that stream is the natural place to count
/// "how many clients are currently consuming our frames." Used by the
/// `--detach-primary` session-transition watcher in `main.rs` to disable the
/// physical displays only while at least one client is connected, and re-
/// attach them as soon as the last client disconnects.
#[derive(Clone, Default)]
pub struct SessionTracker {
    pub count: Arc<AtomicUsize>,
    /// Notified whenever `count` changes. `tokio::sync::Notify` is
    /// edge-triggered; we wake one waiter per state transition, which
    /// is what the watchdog needs.
    pub notify: Arc<Notify>,
}

impl SessionTracker {
    fn enter(&self) {
        let prev = self.count.fetch_add(1, Ordering::SeqCst);
        tracing::info!(prev, now = prev + 1, "SessionTracker::enter");
        self.notify.notify_one();
    }
    fn leave(&self) {
        let prev = self.count.fetch_sub(1, Ordering::SeqCst);
        tracing::info!(prev, now = prev.saturating_sub(1), "SessionTracker::leave");
        self.notify.notify_one();
    }
}

/// Wraps an `RdpServerDisplayUpdates` so its lifetime drives a
/// `SessionTracker`. The inner trait is what ironrdp_server polls;
/// our wrapper just forwards `next_update` and bumps the counter on
/// new/drop. Zero overhead per frame — only construction and drop touch
/// the atomic.
struct CountedUpdates {
    // `+ Send` so `next_update`'s async future is Send (ironrdp_server uses
    // it under tokio::select!). The concrete inner types satisfy Send;
    // the trait object would otherwise lose it.
    inner: Box<dyn RdpServerDisplayUpdates + Send>,
    tracker: SessionTracker,
}

impl CountedUpdates {
    fn new(inner: Box<dyn RdpServerDisplayUpdates + Send>, tracker: SessionTracker) -> Self {
        tracker.enter();
        Self { inner, tracker }
    }
}

impl Drop for CountedUpdates {
    fn drop(&mut self) {
        self.tracker.leave();
    }
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for CountedUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        self.inner.next_update().await
    }
}

pub struct CaptureDisplay {
    pub width: u16,
    pub height: u16,
    pub fps: u32,
    /// `Some(CGDirectDisplayID)` captures that specific display (e.g. a
    /// `VirtualDisplay`); `None` captures the first SCK display, which
    /// is the user's primary panel. SCK's enumeration order isn't
    /// formally documented as "main first," but in practice it is —
    /// and we only fall through to it when the caller didn't ask for
    /// anything specific.
    pub display_id: Option<u32>,
    /// Target display's logical size in points — fed through to
    /// `CursorState` for the (currently disabled) position-polling
    /// path. Caller queries it from `CGDisplay::main()` for the
    /// primary path, or from `VirtualDisplay::size_pts()`.
    pub screen_size_pts: (f64, f64),
    /// Optional session tracker — when `Some`, the returned
    /// `RdpServerDisplayUpdates` is wrapped so its lifetime bumps the
    /// counter that drives the `--detach-primary` session-transition
    /// watcher. `None` disables the wrap entirely (zero overhead).
    pub session_tracker: Option<SessionTracker>,
    /// EGFX/H.264 frame sink (macOS-only, opt-in via `--enable-h264`). When
    /// `Some`, every captured SCK frame is submitted to the H.264 encoder; once
    /// EGFX has negotiated, the legacy BitmapUpdate path is suppressed and the
    /// display is served entirely over EGFX.
    #[cfg(target_os = "macos")]
    pub gfx: Option<crate::h264::Gfx>,
    /// On-demand-keyframe tuning (`--keyframe-on-change` and its threshold
    /// flags). When disabled, the periodic `--keyframe-interval` is the only IDR
    /// driver besides the forced first frame.
    pub keyframe_on_change: KeyframeOnChange,
    /// Shared mouse-click hint, used only when `keyframe_on_change.enabled`. Lets
    /// the H.264 path lower its keyframe threshold briefly after a click — see
    /// [`ClickSignal`].
    pub click_signal: Option<ClickSignal>,
    /// Trailing flush frames re-sent after the last change to drain mstsc's
    /// presentation buffer (`--flush-frames`; EGFX/H.264 path only). 0 disables.
    pub flush_frames: u32,
    /// Shared "client minimized" flag from the vendor server's
    /// `SuppressOutput` handler. When set, `next_update` short-circuits
    /// (no SCK pull, no encode, no ship) and waits on a short timer
    /// until the flag clears, at which point the H.264 path forces an
    /// IDR keyframe so the client can resume from a clean reference
    /// frame. `None` disables the gate entirely (single-binary builds
    /// where the server doesn't expose the handle — e.g., the non-macOS
    /// stub).
    pub display_suppressed: Option<Arc<AtomicBool>>,
}

/// Look up the primary display's pixel dimensions via ScreenCaptureKit.
///
/// Returns `None` on non-macOS targets so the caller can fall back to a stub
/// default. On macOS, failures surface as `Err` because they almost always
/// mean Screen Recording permission is missing — that's a setup problem the
/// user needs to see, not silently paper over.
pub async fn primary_display_size() -> Result<Option<(u16, u16)>> {
    #[cfg(target_os = "macos")]
    {
        use anyhow::{anyhow, Context};
        use screencapturekit::async_api::AsyncSCShareableContent;

        let content = AsyncSCShareableContent::get().await.map_err(|e| {
            anyhow!("AsyncSCShareableContent::get failed (Screen Recording permission?): {e:?}")
        })?;
        let displays = content.displays();
        let display = displays.first().context("no displays available")?;
        let w = u16::try_from(display.width()).context("display width > u16")?;
        let h = u16::try_from(display.height()).context("display height > u16")?;
        Ok(Some((w, h)))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(None)
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for CaptureDisplay {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: self.width,
            height: self.height,
        }
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        #[cfg(target_os = "macos")]
        let inner: Box<dyn RdpServerDisplayUpdates + Send> = Box::new(
            macos::ScreenCaptureUpdates::start(
                self.width,
                self.height,
                self.fps,
                self.display_id,
                self.screen_size_pts,
                self.gfx.clone(),
                self.keyframe_on_change,
                self.click_signal.clone(),
                self.flush_frames,
                self.display_suppressed.clone(),
            )
            .await?,
        );
        #[cfg(not(target_os = "macos"))]
        let inner: Box<dyn RdpServerDisplayUpdates + Send> =
            Box::new(stub::StubUpdates::new(self.width, self.height)?);
        Ok(match self.session_tracker.clone() {
            Some(tracker) => Box::new(CountedUpdates::new(inner, tracker)),
            None => inner,
        })
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    use anyhow::{anyhow, Context};
    use screencapturekit::async_api::{AsyncSCShareableContent, AsyncSCStream};
    use screencapturekit::cv::CVPixelBufferLockFlags;
    use screencapturekit::prelude::{
        PixelFormat as SckPixelFormat, SCContentFilter, SCStreamConfiguration, SCStreamOutputType,
    };

    /// How long the shared SuppressOutput flag must remain `true` before
    /// the gate engages. A real minimize lasts seconds-to-minutes and
    /// trips easily; backpressure flaps under heavy local CPU/IO last
    /// tens of ms and are filtered out. 1 second is comfortably between.
    const SUPPRESS_DEBOUNCE: Duration = Duration::from_secs(1);

    pub struct ScreenCaptureUpdates {
        stream: AsyncSCStream,
        pending: std::collections::VecDeque<DisplayUpdate>,
        // Force a full-frame seed on the first sample so the client's
        // bitmap cache starts in a known-good state; SCK's dirty rects
        // for frame 0 may not cover everything.
        seeded: bool,
        // When the configured desktop size differs from the Mac's native
        // display, SCK scales the output internally but emits dirty rects
        // in *source* coordinates — they don't line up with the output
        // buffer. RemoteFx (mstsc) renders the resulting mis-positioned
        // tile updates as a black canvas. Force a full-frame BitmapUpdate
        // every tick in that case; the upstream encoder's framebuffer diff
        // then keeps the actual bandwidth reasonable.
        force_full_frame: bool,
        cursor: CursorState,
        /// EGFX/H.264 frame sink; `None` unless `--enable-h264`.
        gfx: Option<crate::h264::Gfx>,
        /// On-demand-keyframe tuning (`--keyframe-on-change`). When disabled, no
        /// dirty-area work is done for keyframe decisions.
        keyframe_on_change: KeyframeOnChange,
        /// Rising-edge state for `keyframe_on_change`: true means "ready to fire
        /// on the next large change". Cleared when one fires; re-armed once the
        /// dirty area subsides below half the threshold (hysteresis). Stops
        /// sustained churn (e.g. video) from forcing an IDR every frame.
        kf_armed: bool,
        /// Mouse-click hint for the post-click keyframe-threshold drop. Only
        /// consulted when `keyframe_on_change.enabled`.
        click_signal: Option<ClickSignal>,
        /// Interval between SCK frames (1/fps). Doubles as the flush-burst
        /// timeout: when SCK goes idle we wait at most this long before
        /// re-submitting the last frame to drain mstsc's presentation buffer.
        frame_interval: Duration,
        /// How many trailing flush frames to re-send after each change
        /// (`--flush-frames`). Each is a tiny skip-P-frame; mstsc needs ≥2 to
        /// display a frame, default 4 gives margin. 0 disables the burst.
        flush_frames: u32,
        /// Trailing flush re-submits remaining after the last real change
        /// (EGFX/H.264 path only). SCK stops delivering frames on a static
        /// screen, so the last change before a pause (e.g. the final keystroke)
        /// would otherwise sit in mstsc's ~2-frame AVC420 presentation buffer
        /// until the next on-screen change or the periodic keyframe — that's
        /// the "typing follows the keyframe" lag. We re-submit the last frame
        /// `flush_frames` times to push it through within a couple of frame
        /// intervals. Stays 0 (no-op) on the legacy bitmap path.
        flush_remaining: u32,
        /// Last BGRA frame submitted to EGFX, reused across frames (no per-frame
        /// realloc) and re-encoded as cheap skip-P-frames during a flush burst.
        last_frame: Vec<u8>,
        last_stride: usize,
        /// Shared "client minimized" flag — see [`super::CaptureDisplay::display_suppressed`].
        /// `None` disables the gate.
        display_suppressed: Option<Arc<AtomicBool>>,
        /// Last observed value of `display_suppressed`, used to detect the
        /// false→true→false transition. On the un-suppress edge we force an
        /// IDR keyframe so the client resumes from a fresh reference frame
        /// (the P-frames we would have sent during the suppress depend on a
        /// state mstsc no longer has).
        was_suppressed: bool,
        /// Local "first observed suppress=true" timestamp for debouncing.
        /// `None` while the shared flag reads `false`; set to `Some(Instant::now())`
        /// the first iteration we read `true`. The suppress gate only fires
        /// after `elapsed >= SUPPRESS_DEBOUNCE` — so a real multi-second
        /// minimize trips it normally, but transient flaps under wire
        /// pressure (mstsc backs off for tens of ms when the encoded backlog
        /// is too large, then resumes) don't. Without this, those flaps
        /// stop/start the video pipeline rapidly under heavy local CPU/IO
        /// (cargo build) and the audio mute thrashes — both audible as
        /// stutter even though no real minimize happened.
        suppressed_since: Option<Instant>,
        /// True once the EGFX/H.264 encoder has accepted at least one
        /// frame for this session (i.e., `gfx.submit_bgra` returned
        /// `Ok(true)`). The suppress gate is a no-op until this is set —
        /// mstsc's normal connect handshake includes a
        /// `SuppressOutput { None }` *before* its display surface is
        /// fully initialized, and stopping the first EGFX frame from
        /// going through leaves mstsc with nothing to display + a
        /// half-initialized surface that doesn't recover when we
        /// un-suppress. FreeRDP doesn't do this. Tracking
        /// "we've delivered the first frame" gives us a clean
        /// "client is fully wired up" signal that's stricter than just
        /// "connected" but cheap to maintain.
        first_egfx_frame_sent: bool,
    }

    impl ScreenCaptureUpdates {
        #[allow(clippy::too_many_arguments)]
        pub async fn start(
            width: u16,
            height: u16,
            fps: u32,
            target_display_id: Option<u32>,
            screen_size_pts: (f64, f64),
            gfx: Option<crate::h264::Gfx>,
            keyframe_on_change: KeyframeOnChange,
            click_signal: Option<ClickSignal>,
            flush_frames: u32,
            display_suppressed: Option<Arc<AtomicBool>>,
        ) -> Result<Self> {
            let content = AsyncSCShareableContent::get()
                .await
                .map_err(|e| anyhow!("AsyncSCShareableContent::get failed (likely Screen Recording permission denied): {e:?}"))?;

            let displays = content.displays();
            let display = match target_display_id {
                Some(id) => displays
                    .iter()
                    .find(|d| d.display_id() == id)
                    .with_context(|| {
                        format!(
                            "SCK enumeration has no display with id={id} — the \
                             virtual display didn't register, or the WindowServer \
                             hasn't picked it up yet. SCK sees: [{}]",
                            displays
                                .iter()
                                .map(|d| format!(
                                    "id={} {}x{}",
                                    d.display_id(),
                                    d.width(),
                                    d.height()
                                ))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?,
                None => displays.first().context("no displays available")?,
            };

            let native_w = u16::try_from(display.width()).context("display width > u16")?;
            let native_h = u16::try_from(display.height()).context("display height > u16")?;
            // Backing (Retina) pixel size of this display. Capturing at EITHER
            // the logical point size (native_w/h) or the backing pixel size
            // (the `--hidpi` path) is a 1:1 native capture — SCK delivers the
            // panel's own pixels and its dirty-rects line up with our
            // framebuffer. Only a size matching neither (explicit
            // --width/--height that asks SCK to scale) forces full frames,
            // because then dirty-rects arrive in source coords and misalign.
            let (backing_w, backing_h) = {
                use core_graphics::display::CGDisplay;
                CGDisplay::new(display.display_id())
                    .display_mode()
                    .and_then(|m| {
                        Some((
                            u16::try_from(m.pixel_width()).ok()?,
                            u16::try_from(m.pixel_height()).ok()?,
                        ))
                    })
                    .unwrap_or((native_w, native_h))
            };
            let force_full_frame = !((native_w == width && native_h == height)
                || (backing_w == width && backing_h == height));
            if force_full_frame {
                tracing::warn!(
                    requested_w = width,
                    requested_h = height,
                    native_w,
                    native_h,
                    backing_w,
                    backing_h,
                    "configured size != native points or backing; SCK dirty rects are in \
                     source coords and would misalign — sending full frames every tick \
                     (higher bandwidth)"
                );
            }

            let filter = SCContentFilter::create()
                .with_display(display)
                .with_excluding_windows(&[])
                .build();

            let config = SCStreamConfiguration::new()
                .with_width(u32::from(width))
                .with_height(u32::from(height))
                .with_pixel_format(SckPixelFormat::BGRA)
                .with_fps(fps)
                // Hide the macOS cursor in the captured framebuffer; we forward
                // it separately as RGBAPointer so the client renders its own
                // (real-shape) cursor without doubling up.
                .with_shows_cursor(false)
                // Stretch the source to fill the output exactly instead of
                // pillarboxing/letterboxing on aspect mismatch. Without this,
                // asking for 1920x1080 on a 16:10 MacBook leaves black bars
                // on the sides; the captured Mac content then sits in a
                // shifted sub-rectangle, so client-predicted cursor sprites
                // drift away from the actual click coord (offset grows
                // proportionally to distance from screen center).
                //
                // Apple replaced `scalesToFit` with `preservesAspectRatio`
                // (inverse semantics) in macOS 14; on macOS 14+ the newer
                // property is what actually takes effect. Set both so a single
                // build works across versions — the no-longer-load-bearing one
                // is a no-op on the version where it doesn't apply.
                .with_scales_to_fit(true)
                .with_preserves_aspect_ratio(false);

            let stream = AsyncSCStream::new(&filter, &config, 4, SCStreamOutputType::Screen);
            stream
                .start_capture()
                .map_err(|e| anyhow!("SCStream::start_capture failed: {e:?}"))?;

            let cursor = CursorState::new(width, height, screen_size_pts)?;
            let frame_interval = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));

            // Reset the cross-connection suppress flag — the `Arc<AtomicBool>`
            // is owned by the server and survives disconnect/reconnect, so a
            // value left set `true` at the previous session's teardown would
            // freeze the new session the moment the gate arms (first EGFX
            // frame ships). Fresh state per connection is the only safe
            // default; the gate then trips correctly when the new client's
            // own SuppressOutput PDU lands.
            if let Some(flag) = display_suppressed.as_ref() {
                flag.store(false, Ordering::Relaxed);
            }

            Ok(Self {
                stream,
                pending: std::collections::VecDeque::new(),
                seeded: false,
                force_full_frame,
                cursor,
                gfx,
                keyframe_on_change,
                kf_armed: true,
                click_signal,
                frame_interval,
                flush_frames,
                flush_remaining: 0,
                last_frame: Vec::new(),
                last_stride: 0,
                display_suppressed,
                was_suppressed: false,
                suppressed_since: None,
                first_egfx_frame_sent: false,
            })
        }
    }

    /// Build a `BitmapUpdate` for a sub-rectangle of the captured frame by
    /// copying the rect's pixels into a tightly-packed buffer.
    fn rect_update(
        src: &[u8],
        src_stride: usize,
        x: u16,
        y: u16,
        w: u16,
        h: u16,
    ) -> Option<DisplayUpdate> {
        let (Some(width), Some(height)) = (NonZeroU16::new(w), NonZeroU16::new(h)) else {
            return None;
        };
        let row_bytes = usize::from(w) * 4;
        let stride = NonZeroUsize::new(row_bytes)?;
        let mut data = Vec::with_capacity(row_bytes * usize::from(h));
        for row in 0..usize::from(h) {
            let src_off = (usize::from(y) + row) * src_stride + usize::from(x) * 4;
            data.extend_from_slice(&src[src_off..src_off + row_bytes]);
        }
        Some(DisplayUpdate::Bitmap(BitmapUpdate {
            x,
            y,
            width,
            height,
            format: PixelFormat::BgrA32,
            data: Bytes::from(data),
            stride,
        }))
    }

    impl Drop for ScreenCaptureUpdates {
        fn drop(&mut self) {
            let _ = self.stream.stop_capture();
        }
    }

    #[async_trait::async_trait]
    impl RdpServerDisplayUpdates for ScreenCaptureUpdates {
        async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
            loop {
                // Poll cursor on every call — preempts queued bitmap rects so
                // pointer position keeps up with mouse movement instead of
                // batching behind the bitmap drain of the previous SCK sample.
                let mut cursor_updates = self.cursor.poll();
                if !cursor_updates.is_empty() {
                    let first = cursor_updates.remove(0);
                    for u in cursor_updates.into_iter().rev() {
                        self.pending.push_front(u);
                    }
                    return Ok(Some(first));
                }

                if let Some(update) = self.pending.pop_front() {
                    return Ok(Some(update));
                }

                // Client minimized (sent `SuppressOutput { desktop_rect: None }`)
                // — stop pulling SCK samples and stop encoding/shipping. Without
                // this, mstsc accumulates EGFX frames during a long minimize and
                // the refocus chew-through locks up its input dispatch for
                // seconds (typing/clicks queue behind decode-and-paint of every
                // buffered frame). Also tear down any in-flight flush burst —
                // re-submitting stale frames during a minimize is pointless.
                // Cursor + pending bitmap branches above still flow (they're
                // tiny and harmless to buffer at the client). Re-check the flag
                // every ~100 ms so resume is responsive.
                //
                // **Only honor suppress after the first EGFX frame has shipped.**
                // mstsc's normal connect handshake includes a
                // `SuppressOutput { None }` *before* its display surface is
                // ready; blocking the first frame leaves it with a half-init'd
                // surface that doesn't recover when we un-suppress (mstsc
                // freezes on the connection). FreeRDP doesn't issue suppress
                // during connect, so it was never affected. Gate the gate on
                // `first_egfx_frame_sent` so the handshake completes normally.
                //
                // **Debounce so transient flaps don't oscillate the pipeline.**
                // Under heavy local CPU/IO (cargo build) mstsc backs off
                // briefly when the encoded backlog grows, sending a quick
                // `SuppressOutput { None }` → `RefreshRectangle` pair (tens of
                // ms). Reacting to those rapid flaps stops/starts the video
                // pipeline and toggles the audio mute, both audible as
                // stutter. Track "first observed suppress" locally and only
                // engage the gate once the flag has been steady-`true` for
                // `SUPPRESS_DEBOUNCE`. Real multi-second minimizes still trip
                // normally.
                if self.first_egfx_frame_sent {
                    if let Some(flag) = self.display_suppressed.as_ref() {
                        if flag.load(Ordering::Relaxed) {
                            let started = *self.suppressed_since.get_or_insert_with(Instant::now);
                            if started.elapsed() >= SUPPRESS_DEBOUNCE {
                                self.was_suppressed = true;
                                self.flush_remaining = 0;
                                tokio::time::sleep(Duration::from_millis(100)).await;
                                continue;
                            }
                        } else {
                            self.suppressed_since = None;
                        }
                    }
                }

                // While a flush burst is pending, don't block indefinitely on
                // SCK: it stops delivering frames on a static screen, so wait at
                // most one frame interval and, on timeout, re-submit the last
                // frame as a cheap skip-P-frame. That keeps mstsc's presentation
                // buffer advancing so the last change before a pause appears
                // promptly instead of stranding until the next periodic keyframe.
                // (No flush pending — the common idle case — blocks normally.)
                let sample = if self.flush_remaining > 0 {
                    match tokio::time::timeout(self.frame_interval, self.stream.next()).await {
                        Ok(Some(sample)) => sample,
                        Ok(None) => return Ok(None),
                        Err(_) => {
                            self.flush_remaining -= 1;
                            if let Some(gfx) = self.gfx.as_ref() {
                                if !self.last_frame.is_empty() {
                                    if let Err(e) =
                                        gfx.submit_bgra(&self.last_frame, self.last_stride, false)
                                    {
                                        tracing::warn!(error = ?e, "EGFX flush submit_bgra failed");
                                    }
                                }
                            }
                            continue;
                        }
                    }
                } else {
                    match self.stream.next().await {
                        Some(sample) => sample,
                        None => return Ok(None),
                    }
                };

                // Skip non-renderable frames (Idle, Blank, Suspended, Stopped).
                if let Some(status) = sample.frame_status() {
                    if !status.has_content() {
                        continue;
                    }
                }

                let Some(pixel_buffer) = sample.image_buffer() else {
                    continue;
                };

                let guard = pixel_buffer
                    .lock(CVPixelBufferLockFlags::READ_ONLY)
                    .map_err(|e| anyhow!("CVPixelBuffer::lock OSStatus {e}"))?;

                let pb_width = u16::try_from(guard.width()).context("pixel buffer width > u16")?;
                let pb_height =
                    u16::try_from(guard.height()).context("pixel buffer height > u16")?;
                let stride_bytes = guard.bytes_per_row();
                let src = guard.as_slice();

                // Log the dimensions SCK actually delivers, once per session.
                // `pb_width`/`pb_height` are the real captured buffer size — at
                // backing (Retina) pixels with `--hidpi`, at logical points
                // otherwise. Confirms `--hidpi` took effect and that SCK didn't
                // silently downscale.
                if !self.seeded {
                    tracing::debug!(
                        pb_width,
                        pb_height,
                        stride_bytes,
                        force_full_frame = self.force_full_frame,
                        "capture: first frame delivered"
                    );
                }

                // EGFX/H.264 path: submit the full frame to the encoder. Once
                // EGFX has negotiated (`Ok(true)`), it owns the display — skip
                // the legacy BitmapUpdate emission entirely (cursor still flows
                // via the poll at the top of the loop). Before negotiation, or
                // for non-EGFX clients (`Ok(false)`), fall through to legacy.
                if let Some(gfx) = self.gfx.as_ref() {
                    // On-demand keyframes (opt-in via --keyframe-on-change). A
                    // large change at once (window raised to front, scroll, app
                    // launch) is applied cleanly by some clients (mstsc) only on a
                    // keyframe — as a P-frame it can render garbled/stale until the
                    // next periodic IDR. Force an IDR on the rising edge of a large
                    // dirty area; small changes (typing, caret) stay below the
                    // threshold and remain cheap P-frames. (We deliberately do NOT
                    // skip unchanged frames here — mstsc only flushes its ~2-frame
                    // presentation buffer when frames keep arriving, so a continuous
                    // stream is what keeps typing/window-switching snappy at 60fps.
                    // The vImage conversion makes a steady stream cheap anyway.)
                    let kfc = self.keyframe_on_change;
                    let big_change = if kfc.enabled {
                        let frame_px = u64::from(pb_width) * u64::from(pb_height);
                        // Dirty-rect area as a "how much moved" fraction (rects may
                        // be in source coords when scaling, but the ratio holds).
                        let changed_px: u64 = sample
                            .dirty_rects()
                            .map(|rects| {
                                rects
                                    .iter()
                                    .map(|r| {
                                        let s = r.size();
                                        (s.width.max(0.0) as u64) * (s.height.max(0.0) as u64)
                                    })
                                    .sum()
                            })
                            .unwrap_or(0);
                        // Briefly after a click, treat a much smaller change as
                        // keyframe-worthy too (a click usually precedes a UI
                        // update; the IDR still only fires if a change follows).
                        let high = if self
                            .click_signal
                            .as_ref()
                            .is_some_and(|s| s.within(kfc.click_window))
                        {
                            kfc.click_pct
                        } else {
                            kfc.change_pct
                        };
                        let low = (high / 2).max(1); // hysteresis re-arm band
                        let is_big = frame_px > 0 && changed_px * 100 >= frame_px * high;
                        let is_quiet = frame_px == 0 || changed_px * 100 < frame_px * low;
                        // Rising edge only: fire once when a large change begins,
                        // then stay quiet until it subsides below `low`. Without
                        // this, sustained churn (video, htop) above the threshold
                        // would force an IDR every frame and wreck quality.
                        if self.kf_armed && is_big {
                            self.kf_armed = false;
                            true
                        } else if is_quiet {
                            self.kf_armed = true;
                            false
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    // Force an IDR on the first frame after un-suppress: the
                    // P-frames we would have sent during the minimize depend
                    // on a reference frame mstsc may no longer hold (or its
                    // decoder state may have been torn down). Without a fresh
                    // keyframe the resume frame would decode against missing
                    // state and render garbled.
                    let resume_keyframe = if self.was_suppressed {
                        self.was_suppressed = false;
                        tracing::debug!("client un-suppress edge — forcing IDR on next encode");
                        true
                    } else {
                        false
                    };
                    match gfx.submit_bgra(src, stride_bytes, big_change || resume_keyframe) {
                        Ok(true) => {
                            self.seeded = true;
                            // First-EGFX-frame milestone: arms the suppress
                            // gate (see `first_egfx_frame_sent` in the struct).
                            if !self.first_egfx_frame_sent {
                                self.first_egfx_frame_sent = true;
                                tracing::debug!(
                                    "first EGFX frame shipped — suppress gate now armed"
                                );
                            }
                            // Stash this frame and arm the flush burst so that,
                            // once SCK goes idle after this change, we re-submit
                            // it enough times to drain mstsc's presentation
                            // buffer. Reuse the buffer to avoid a per-frame
                            // realloc; clear keeps the capacity.
                            self.last_frame.clear();
                            self.last_frame.extend_from_slice(src);
                            self.last_stride = stride_bytes;
                            self.flush_remaining = self.flush_frames;
                            continue;
                        }
                        Ok(false) => {}
                        Err(e) => tracing::warn!(error = ?e, "EGFX submit_bgra failed"),
                    }
                }

                // Decide the rect set to emit. On the first frame we always
                // send the full frame so the client's bitmap cache is seeded.
                // After that, SCK's dirty_rects tells us what changed; if the
                // attachment is missing (older macOS, no key), fall back to
                // the full frame.
                let dirty = if !self.seeded || self.force_full_frame {
                    None
                } else {
                    sample.dirty_rects()
                };

                let rects: Vec<(u16, u16, u16, u16)> = match dirty {
                    Some(list) if !list.is_empty() => list
                        .into_iter()
                        .filter_map(|r| {
                            let origin = r.origin();
                            let size = r.size();
                            let x = origin.x.max(0.0).round() as u32;
                            let y = origin.y.max(0.0).round() as u32;
                            let w = size.width.max(0.0).round() as u32;
                            let h = size.height.max(0.0).round() as u32;
                            let x = u16::try_from(x.min(u32::from(pb_width))).ok()?;
                            let y = u16::try_from(y.min(u32::from(pb_height))).ok()?;
                            let w =
                                u16::try_from(w.min(u32::from(pb_width.saturating_sub(x)))).ok()?;
                            let h = u16::try_from(h.min(u32::from(pb_height.saturating_sub(y))))
                                .ok()?;
                            if w == 0 || h == 0 {
                                None
                            } else {
                                Some((x, y, w, h))
                            }
                        })
                        .collect(),
                    _ => vec![(0, 0, pb_width, pb_height)],
                };

                for (x, y, w, h) in rects {
                    if let Some(update) = rect_update(src, stride_bytes, x, y, w, h) {
                        self.pending.push_back(update);
                    }
                }
                self.seeded = true;
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod stub {
    use super::*;
    use anyhow::Context;

    pub struct StubUpdates {
        pending: Option<DisplayUpdate>,
    }

    impl StubUpdates {
        pub fn new(width: u16, height: u16) -> Result<Self> {
            let w = NonZeroU16::new(width).context("width must be > 0")?;
            let h = NonZeroU16::new(height).context("height must be > 0")?;
            let stride = NonZeroUsize::new(usize::from(width) * 4).context("stride must be > 0")?;
            let pixel_count = usize::from(width) * usize::from(height);
            let mut data = Vec::with_capacity(pixel_count * 4);
            for _ in 0..pixel_count {
                data.extend_from_slice(&[0xFF, 0x10, 0x80, 0x90]);
            }
            Ok(Self {
                pending: Some(DisplayUpdate::Bitmap(BitmapUpdate {
                    x: 0,
                    y: 0,
                    width: w,
                    height: h,
                    format: PixelFormat::ARgb32,
                    data: Bytes::from(data),
                    stride,
                })),
            })
        }
    }

    #[async_trait::async_trait]
    impl RdpServerDisplayUpdates for StubUpdates {
        async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
            if let Some(u) = self.pending.take() {
                return Ok(Some(u));
            }
            std::future::pending::<()>().await;
            Ok(None)
        }
    }
}
