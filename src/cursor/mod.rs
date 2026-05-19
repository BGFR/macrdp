//! Forward the macOS system cursor to RDP clients.
//!
//! Primary path: `private_api::copy_current_system_cursor` (SkyLight
//! `SLSGetGlobalCursorData`) — reads the WindowServer's actually-
//! composited cursor, so I-beams in other apps, crosshairs during
//! `screencapture -i`, hand pointers over links, etc. all forward
//! correctly. Returns raw RGBA bytes + hotspot directly, no CGImage
//! round-trip.
//!
//! Fallback path: `NSCursor.currentSystemCursor` rendered via
//! `NSBitmapImageRep` — process-local cursor stack, only sees cursors
//! set in macrdp's own process. Kept as a last resort in case a future
//! macOS removes/renames the SLS symbols.
//!
//! Position is read via CGEvent.location() on a fresh no-op event but
//! intentionally not forwarded — see `poll` for the reasoning.

#[cfg(target_os = "macos")]
mod private_api;

use ironrdp_server::DisplayUpdate;

pub struct CursorState {
    #[cfg(target_os = "macos")]
    inner: macos::Inner,
    #[cfg(not(target_os = "macos"))]
    _phantom: (),
}

impl CursorState {
    /// `screen_size_pts` is the target display's logical size in
    /// points; only consumed by the (currently-disabled) position
    /// polling path, but parameterized for symmetry with input.rs.
    pub fn new(
        desktop_w: u16,
        desktop_h: u16,
        screen_size_pts: (f64, f64),
    ) -> anyhow::Result<Self> {
        #[cfg(target_os = "macos")]
        let inner = macos::Inner::new(desktop_w, desktop_h, screen_size_pts)?;
        #[cfg(not(target_os = "macos"))]
        let _ = (desktop_w, desktop_h, screen_size_pts);
        Ok(Self {
            #[cfg(target_os = "macos")]
            inner,
            #[cfg(not(target_os = "macos"))]
            _phantom: (),
        })
    }

    /// Return any cursor-related DisplayUpdates that the client should see.
    /// Cheap to call: the cost is one CGEvent::new + one NSCursor read.
    pub fn poll(&mut self) -> Vec<DisplayUpdate> {
        #[cfg(target_os = "macos")]
        return self.inner.poll();
        #[cfg(not(target_os = "macos"))]
        return Vec::new();
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    use anyhow::{anyhow, Result};
    use core_graphics::event::CGEvent;
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use ironrdp_pdu::pointer::PointerPositionAttribute;
    use ironrdp_server::{DisplayUpdate, RGBAPointer};
    use objc2::rc::Retained;
    use objc2::ClassType;
    use objc2_app_kit::{
        NSBitmapFormat, NSBitmapImageRep, NSCompositingOperation, NSCursor, NSDeviceRGBColorSpace,
        NSGraphicsContext,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use tracing::trace;

    use super::private_api;

    #[allow(dead_code)] // last_pos / desktop+screen fields are kept warm for
                        // re-enabling position polling in non-RDP-driven cases
    pub struct Inner {
        source: CGEventSource,
        last_hash: u64,
        last_pos: Option<(u16, u16)>,
        desktop_w: u16,
        desktop_h: u16,
        screen_w_pts: f64,
        screen_h_pts: f64,
    }

    // CGEventSource is a CF type — thread-safe by Apple convention, single-
    // threaded use in practice. See input.rs for the same justification.
    unsafe impl Send for Inner {}

    impl Inner {
        pub fn new(
            desktop_w: u16,
            desktop_h: u16,
            screen_size_pts: (f64, f64),
        ) -> Result<Self> {
            let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
                .map_err(|_| anyhow!("CGEventSource::new failed"))?;
            Ok(Self {
                source,
                last_hash: 0,
                last_pos: None,
                desktop_w,
                desktop_h,
                screen_w_pts: screen_size_pts.0,
                screen_h_pts: screen_size_pts.1,
            })
        }

        pub fn poll(&mut self) -> Vec<DisplayUpdate> {
            let mut updates = Vec::new();
            self.poll_shape(&mut updates);
            // Intentionally NOT polling cursor position: mstsc (and other RDP
            // clients) predict the cursor locally from the user's mouse input
            // and snap to any PointerPositionPDU we send. Our updates lag the
            // local prediction by the encode/network round-trip, so sending
            // them causes the cursor to jump back to a stale position. The
            // downside: if a Mac app programmatically moves the cursor (rare),
            // the client won't see it until something else triggers a frame.
            updates
        }

        #[allow(dead_code)]
        fn poll_position(&mut self, out: &mut Vec<DisplayUpdate>) {
            let Ok(ev) = CGEvent::new(self.source.clone()) else {
                return;
            };
            let pt = ev.location();
            // CGEvent::location is in pixels with top-left origin (display
            // coordinates), same orientation as our captured framebuffer.
            let x = (pt.x * f64::from(self.desktop_w) / self.screen_w_pts.max(1.0))
                .round()
                .clamp(0.0, f64::from(u16::MAX));
            let y = (pt.y * f64::from(self.desktop_h) / self.screen_h_pts.max(1.0))
                .round()
                .clamp(0.0, f64::from(u16::MAX));
            let x = x as u16;
            let y = y as u16;
            if self.last_pos != Some((x, y)) {
                self.last_pos = Some((x, y));
                out.push(DisplayUpdate::PointerPosition(PointerPositionAttribute {
                    x,
                    y,
                }));
            }
        }

        fn poll_shape(&mut self, out: &mut Vec<DisplayUpdate>) {
            let bytes_and_hot = unsafe { read_cursor_bitmap() };
            let Some((data, w, h, hot_x, hot_y)) = bytes_and_hot else {
                return;
            };

            let mut hasher = DefaultHasher::new();
            data.hash(&mut hasher);
            // Mix size/hot into hash so identical pixel patterns at different
            // dimensions still register as a shape change.
            w.hash(&mut hasher);
            h.hash(&mut hasher);
            hot_x.hash(&mut hasher);
            hot_y.hash(&mut hasher);
            let hash = hasher.finish();
            if hash == self.last_hash {
                return;
            }
            self.last_hash = hash;
            trace!(w, h, hot_x, hot_y, "cursor shape changed");
            out.push(DisplayUpdate::RGBAPointer(RGBAPointer {
                width: w,
                height: h,
                hot_x,
                hot_y,
                data,
            }));
        }
    }

    /// Snapshot the current system cursor into a tightly-packed RGBA buffer.
    /// Returns `(data, width, height, hot_x, hot_y)` or `None` if anything
    /// went wrong (no cursor available, weird image size, draw failed).
    ///
    /// Two-tier lookup:
    ///   1. SkyLight `SLSGetGlobalCursorData` → the actually-rendered
    ///      system cursor (any process's I-beam, crosshair, hand, etc.).
    ///      Returns bitmap + hotspot directly.
    ///   2. Fallback to `NSCursor.currentSystemCursor` for the (rare)
    ///      case where SkyLight refuses the call or the symbols vanish
    ///      in a future macOS.
    unsafe fn read_cursor_bitmap() -> Option<(Vec<u8>, u16, u16, u16, u16)> {
        // Try SkyLight first — this sees cursors set by other processes
        // (Safari's I-beam, `screencapture -i`'s crosshair, web link hand
        // pointers, etc.) and gives us the real hotspot in one call.
        if let Some(t) = private_api::copy_current_system_cursor() {
            return Some(t);
        }

        // Fallback: NSCursor only sees cursors set in macrdp's process,
        // but works as a last resort if the private SkyLight symbols are
        // ever removed.
        let cursor: Retained<NSCursor> = match NSCursor::currentSystemCursor() {
            Some(c) => c,
            None => NSCursor::currentCursor(),
        };
        let image = cursor.image();
        let hot = cursor.hotSpot();
        let size = image.size();

        let w = size.width.round() as isize;
        let h = size.height.round() as isize;
        if !(1..=256).contains(&w) || !(1..=256).contains(&h) {
            return None;
        }

        // Allocate a 32-bit RGBA, non-premultiplied, top-down bitmap rep.
        // bytesPerRow=0 makes AppKit pick an aligned stride; we read it back.
        let rep: Retained<NSBitmapImageRep> = NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            w,
            h,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            NSBitmapFormat::AlphaNonpremultiplied,
            0,
            32,
        )?;

        let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)?;
        NSGraphicsContext::saveGraphicsState_class();
        NSGraphicsContext::setCurrentContext(Some(&ctx));

        let dst_rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w as f64, h as f64));
        let zero_rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
        image.drawInRect_fromRect_operation_fraction(
            dst_rect,
            zero_rect,
            NSCompositingOperation::Copy,
            1.0,
        );

        NSGraphicsContext::restoreGraphicsState_class();

        let ptr = rep.bitmapData();
        if ptr.is_null() {
            return None;
        }
        let stride = rep.bytesPerRow() as usize;
        let row_bytes = (w as usize) * 4;
        let mut data = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for row in 0..(h as usize) {
            let src = std::slice::from_raw_parts(ptr.add(row * stride), row_bytes);
            data.extend_from_slice(src);
        }

        Some((
            data,
            w as u16,
            h as u16,
            hot.x.round().max(0.0).min(f64::from(u16::MAX)) as u16,
            hot.y.round().max(0.0).min(f64::from(u16::MAX)) as u16,
        ))
    }
}
