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
pub use macos::{DetachedPrimary, PrimaryOverride, VirtualDisplay};

#[cfg(not(target_os = "macos"))]
pub use stub::{DetachedPrimary, PrimaryOverride, VirtualDisplay};

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{anyhow, Context, Result};
    use core_graphics::display::{CGConfigureOption, CGDisplay};

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
            let handle = private_api::create(width, height, refresh_hz, "macrdp")
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

    /// Promotes a virtual (or any other secondary) display to be the
    /// system's primary — the one at global-coord origin `(0, 0)` that
    /// holds the menu bar and is where new app windows open.
    ///
    /// Implemented as a session-scoped `CGConfigureDisplayOrigin` swap:
    /// the target display is moved to `(0, 0)` and the old primary is
    /// shifted aside. Drop restores the original arrangement; if Drop
    /// doesn't run (signal-driven `std::process::exit`, crash), the
    /// session scope still reverts the layout when the user logs out.
    pub struct PrimaryOverride {
        primary_id: u32,
        primary_old_origin: (i32, i32),
        virtual_id: u32,
        virtual_old_origin: (i32, i32),
    }

    impl PrimaryOverride {
        /// Returns `Ok(Some(_))` after performing the swap, `Ok(None)`
        /// if the virtual display was already the system primary (the
        /// caller wanted that end state, so there's nothing to do and
        /// nothing to restore on shutdown), or `Err` on a real failure.
        pub fn install(virtual_display_id: u32) -> Result<Option<Self>> {
            let main = CGDisplay::main();
            let primary_id = main.id;
            if primary_id == virtual_display_id {
                // macOS auto-placed the vdisplay at (0,0) and made it the
                // menu-bar display — fairly common on first-time vdisplay
                // attach, since macOS persists external-monitor arrangement.
                return Ok(None);
            }

            let main_bounds = main.bounds();
            let primary_old_origin = (
                main_bounds.origin.x.round() as i32,
                main_bounds.origin.y.round() as i32,
            );

            let vd = CGDisplay::new(virtual_display_id);
            let vd_bounds = vd.bounds();
            let virtual_old_origin = (
                vd_bounds.origin.x.round() as i32,
                vd_bounds.origin.y.round() as i32,
            );
            // Width we'll shift the old primary by so it sits flush to the
            // right of the (now-primary) virtual display.
            let vd_width = vd_bounds.size.width.round() as i32;

            let config = main
                .begin_configuration()
                .map_err(|e| anyhow!("CGBeginDisplayConfiguration failed: CGError {e}"))?;
            vd.configure_display_origin(&config, 0, 0)
                .map_err(|e| anyhow!("CGConfigureDisplayOrigin(vd, 0, 0): CGError {e}"))?;
            main.configure_display_origin(&config, vd_width, 0)
                .map_err(|e| {
                    anyhow!("CGConfigureDisplayOrigin(primary, {vd_width}, 0): CGError {e}")
                })?;
            // ConfigureForSession means the swap reverts on user logout —
            // a free safety net if our explicit restore in Drop doesn't run.
            main.complete_configuration(&config, CGConfigureOption::ConfigureForSession)
                .map_err(|e| anyhow!("CGCompleteDisplayConfiguration: CGError {e}"))?;

            Ok(Some(Self {
                primary_id,
                primary_old_origin,
                virtual_id: virtual_display_id,
                virtual_old_origin,
            }))
        }
    }

    impl Drop for PrimaryOverride {
        fn drop(&mut self) {
            // Best-effort restore. Any error here is a "user's layout is now
            // wrong until logout"-level annoyance, not a panic. Don't fail.
            let main = CGDisplay::new(self.primary_id);
            let config = match main.begin_configuration() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "could not begin display reconfig during restore (CGError {e}); \
                         layout will revert on logout"
                    );
                    return;
                }
            };
            let _ = main.configure_display_origin(
                &config,
                self.primary_old_origin.0,
                self.primary_old_origin.1,
            );
            let vd = CGDisplay::new(self.virtual_id);
            let _ = vd.configure_display_origin(
                &config,
                self.virtual_old_origin.0,
                self.virtual_old_origin.1,
            );
            if let Err(e) =
                main.complete_configuration(&config, CGConfigureOption::ConfigureForSession)
            {
                tracing::warn!(
                    "could not complete display reconfig during restore (CGError {e}); \
                     layout will revert on logout"
                );
            } else {
                tracing::info!("restored original display arrangement");
            }
        }
    }

    /// Promotes the virtual display to primary AND disables the
    /// original primary display entirely — WindowServer treats the
    /// original panel as gone (backlight off, no menu bar, no
    /// windows can land there, cursor can't cross onto it).
    ///
    /// Implemented as one atomic `CGBeginDisplayConfiguration` block:
    /// move vd to `(0, 0)`, push old primary aside (so when re-enabled
    /// on Drop it doesn't collide), then `CGSConfigureDisplayEnabled(
    /// old_primary, false)`. Drop re-enables and restores positions.
    ///
    /// The config is session-scoped, so even if Drop never runs (signal
    /// exit, crash) the display layout reverts at the next logout — the
    /// user is never permanently locked out.
    pub struct DetachedPrimary {
        primary_id: u32,
        primary_old_origin: (i32, i32),
        virtual_id: u32,
        virtual_old_origin: (i32, i32),
        enabler: private_api::DisplayEnabler,
    }

    impl DetachedPrimary {
        pub fn install(virtual_display_id: u32) -> Result<Self> {
            let enabler = private_api::DisplayEnabler::load()
                .context("loading CGSConfigureDisplayEnabled private symbol")?;

            let main = CGDisplay::main();
            let primary_id = main.id;
            if primary_id == virtual_display_id {
                return Err(anyhow!(
                    "virtual display (id={virtual_display_id}) is already the \
                     system primary — disabling it would leave no usable display"
                ));
            }

            let main_bounds = main.bounds();
            let primary_old_origin = (
                main_bounds.origin.x.round() as i32,
                main_bounds.origin.y.round() as i32,
            );

            let vd = CGDisplay::new(virtual_display_id);
            let vd_bounds = vd.bounds();
            let virtual_old_origin = (
                vd_bounds.origin.x.round() as i32,
                vd_bounds.origin.y.round() as i32,
            );
            let vd_width = vd_bounds.size.width.round() as i32;

            let config = main
                .begin_configuration()
                .map_err(|e| anyhow!("CGBeginDisplayConfiguration: CGError {e}"))?;

            // Move vd to (0,0) so it's the layout primary.
            vd.configure_display_origin(&config, 0, 0)
                .map_err(|e| anyhow!("CGConfigureDisplayOrigin(vd, 0, 0): CGError {e}"))?;
            // Shift built-in aside so re-enabling on Drop doesn't overlap.
            main.configure_display_origin(&config, vd_width, 0)
                .map_err(|e| {
                    anyhow!("CGConfigureDisplayOrigin(primary, {vd_width}, 0): CGError {e}")
                })?;
            // Disable the built-in: backlight off, no menu bar, cursor barrier.
            enabler.set(config, primary_id, false)?;

            main.complete_configuration(&config, CGConfigureOption::ConfigureForSession)
                .map_err(|e| anyhow!("CGCompleteDisplayConfiguration: CGError {e}"))?;

            Ok(Self {
                primary_id,
                primary_old_origin,
                virtual_id: virtual_display_id,
                virtual_old_origin,
                enabler,
            })
        }
    }

    impl Drop for DetachedPrimary {
        fn drop(&mut self) {
            let main = CGDisplay::new(self.primary_id);
            let config = match main.begin_configuration() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "could not begin display reconfig during detach restore \
                         (CGError {e}); built-in stays disabled until logout"
                    );
                    return;
                }
            };
            // Re-enable first so subsequent origin moves apply to a live display.
            if let Err(e) = self.enabler.set(config, self.primary_id, true) {
                tracing::warn!(
                    "could not re-enable built-in display during restore ({e}); \
                     layout will revert on logout"
                );
            }
            let _ = main.configure_display_origin(
                &config,
                self.primary_old_origin.0,
                self.primary_old_origin.1,
            );
            let vd = CGDisplay::new(self.virtual_id);
            let _ = vd.configure_display_origin(
                &config,
                self.virtual_old_origin.0,
                self.virtual_old_origin.1,
            );
            if let Err(e) =
                main.complete_configuration(&config, CGConfigureOption::ConfigureForSession)
            {
                tracing::warn!(
                    "could not complete display reconfig during detach restore \
                     (CGError {e}); layout will revert on logout"
                );
            } else {
                tracing::info!("re-enabled built-in display and restored layout");
            }
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

    pub struct PrimaryOverride;

    impl PrimaryOverride {
        pub fn install(_virtual_display_id: u32) -> Result<Option<Self>> {
            Err(anyhow!("primary-display override is macOS-only"))
        }
    }

    pub struct DetachedPrimary;

    impl DetachedPrimary {
        pub fn install(_virtual_display_id: u32) -> Result<Self> {
            Err(anyhow!("primary-display detach is macOS-only"))
        }
    }
}
