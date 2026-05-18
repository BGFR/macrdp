//! Headless virtual display backed by a private `CGVirtualDisplay`
//! handle. The Mac treats it like an attached external monitor — same
//! displayID, same SCK enumeration, same bounds in the global coord
//! space — without changing the user's primary display.
//!
//! Public API is intentionally tiny and stable: `VirtualDisplay::new`
//! gives you a handle, and `display_id`/`origin_pts`/`size_pts` are the
//! three things capture / input / cursor need to address it. Everything
//! private API-related is in `private_api.rs`; nothing outside this
//! module should touch that file.

#[cfg(target_os = "macos")]
mod private_api;

#[cfg(target_os = "macos")]
pub use macos::VirtualDisplay;

#[cfg(not(target_os = "macos"))]
pub use stub::VirtualDisplay;

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{anyhow, Context, Result};
    use core_graphics::display::CGDisplay;

    use super::private_api;

    pub struct VirtualDisplay {
        // RAII: dropped last so the CG handle is released after we've
        // logged anything we want to log about it.
        _handle: private_api::Handle,
        display_id: u32,
        origin_pts: (f64, f64),
        size_pts: (f64, f64),
    }

    impl VirtualDisplay {
        /// Allocate a headless display at `width × height` pixels and
        /// `refresh_hz` Hz. Refresh rate is mostly cosmetic — the RDP
        /// server's frame cadence is independent — but a real value
        /// keeps `Displays.app` from looking weird.
        ///
        /// Returns an Err with a clear "private API unavailable" message
        /// if any of the underlying CG symbols / Obj-C classes can't be
        /// resolved on this macOS version. Caller should treat that as
        /// "this feature isn't usable here," not a fatal bug.
        pub fn new(width: u32, height: u32, refresh_hz: u32) -> Result<Self> {
            let api = private_api::PrivateApi::load()
                .context("loading private CoreGraphics symbols")?;
            let handle = private_api::create(&api, width, height, refresh_hz, "macrdp")
                .context("creating virtual display")?;

            // CGDisplayBounds gives both the origin (in global point space)
            // and the point-space size. macOS auto-arranges new displays
            // off the right edge of the main panel by default; the origin
            // is what we add to mouse coords so CGEventPost lands events
            // on the vdisplay, not on the user's main screen.
            let id = handle.display_id();
            let bounds = CGDisplay::new(id).bounds();
            if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
                return Err(anyhow!(
                    "virtual display registered (id={id}) but CGDisplayBounds \
                     returned a zero-size rect — the system hasn't finished \
                     activating it yet"
                ));
            }

            Ok(Self {
                _handle: handle,
                display_id: id,
                origin_pts: (bounds.origin.x, bounds.origin.y),
                size_pts: (bounds.size.width, bounds.size.height),
            })
        }

        pub fn display_id(&self) -> u32 {
            self.display_id
        }

        pub fn origin_pts(&self) -> (f64, f64) {
            self.origin_pts
        }

        pub fn size_pts(&self) -> (f64, f64) {
            self.size_pts
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod stub {
    use anyhow::{anyhow, Result};

    pub struct VirtualDisplay;

    impl VirtualDisplay {
        pub fn new(_width: u32, _height: u32, _refresh_hz: u32) -> Result<Self> {
            Err(anyhow!(
                "virtual display is macOS-only — this binary was built for a \
                 different target"
            ))
        }
        pub fn display_id(&self) -> u32 { 0 }
        pub fn origin_pts(&self) -> (f64, f64) { (0.0, 0.0) }
        pub fn size_pts(&self) -> (f64, f64) { (0.0, 0.0) }
    }
}
