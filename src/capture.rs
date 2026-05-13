//! Screen capture for the RDP display layer.
//!
//! macOS uses ScreenCaptureKit via the `screencapturekit` crate. Other targets
//! get a static-rectangle stub so the protocol layer still builds and can be
//! exercised on Linux CI.

use std::num::{NonZeroU16, NonZeroUsize};

use anyhow::Result;
use bytes::Bytes;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat, RdpServerDisplay,
    RdpServerDisplayUpdates,
};

pub struct CaptureDisplay {
    pub width: u16,
    pub height: u16,
    pub fps: u32,
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
        {
            let updates = macos::ScreenCaptureUpdates::start(self.width, self.height, self.fps)
                .await?;
            Ok(Box::new(updates))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(Box::new(stub::StubUpdates::new(self.width, self.height)?))
        }
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

    pub struct ScreenCaptureUpdates {
        stream: AsyncSCStream,
        pending: std::collections::VecDeque<DisplayUpdate>,
        // Force a full-frame seed on the first sample so the client's
        // bitmap cache starts in a known-good state; SCK's dirty rects
        // for frame 0 may not cover everything.
        seeded: bool,
    }

    impl ScreenCaptureUpdates {
        pub async fn start(width: u16, height: u16, fps: u32) -> Result<Self> {
            let content = AsyncSCShareableContent::get()
                .await
                .map_err(|e| anyhow!("AsyncSCShareableContent::get failed (likely Screen Recording permission denied): {e:?}"))?;

            let displays = content.displays();
            let display = displays.first().context("no displays available")?;

            let filter = SCContentFilter::create()
                .with_display(display)
                .with_excluding_windows(&[])
                .build();

            let config = SCStreamConfiguration::new()
                .with_width(u32::from(width))
                .with_height(u32::from(height))
                .with_pixel_format(SckPixelFormat::BGRA)
                .with_fps(fps);

            let stream =
                AsyncSCStream::new(&filter, &config, 4, SCStreamOutputType::Screen);
            stream
                .start_capture()
                .map_err(|e| anyhow!("SCStream::start_capture failed: {e:?}"))?;

            Ok(Self {
                stream,
                pending: std::collections::VecDeque::new(),
                seeded: false,
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
                if let Some(update) = self.pending.pop_front() {
                    return Ok(Some(update));
                }

                let Some(sample) = self.stream.next().await else {
                    return Ok(None);
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

                // Decide the rect set to emit. On the first frame we always
                // send the full frame so the client's bitmap cache is seeded.
                // After that, SCK's dirty_rects tells us what changed; if the
                // attachment is missing (older macOS, no key), fall back to
                // the full frame.
                let dirty = if !self.seeded {
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
                            let w = u16::try_from(w.min(u32::from(pb_width.saturating_sub(x))))
                                .ok()?;
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
            let stride =
                NonZeroUsize::new(usize::from(width) * 4).context("stride must be > 0")?;
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
