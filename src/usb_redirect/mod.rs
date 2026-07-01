//! Generic USB redirection (MS-RDPEUSB) — **Phase-1b go/no-go spike only**.
//!
//! macrdp is the RDP server / presenting side: the physical USB device lives on
//! the client (which runs `urbdrc`), and the server drives it and presents it
//! locally. The macOS presenting mechanism is a user-space virtual USB host
//! controller via the public `IOUSBHost.framework` class
//! `IOUSBHostControllerInterface`, which requires the (granted) entitlement
//! `com.apple.developer.usb.host-controller-interface`.
//!
//! This module currently contains ONLY the go/no-go spike: does the controller
//! instantiate in a signed+provisioned build? The real feature (present a device
//! and wire MS-RDPEUSB over DVC, reusing upstream `ironrdp-rdpeusb`'s PDU layer)
//! is Phases 2–3, only after this is green. All Obj-C / IOUSBHost SPI touches are
//! quarantined in `usb_spike.m` (the maintenance boundary), built + linked by
//! `build.rs` on macOS.

#[cfg(target_os = "macos")]
mod imp {
    unsafe extern "C" {
        fn macrdp_usb_spike_run() -> core::ffi::c_int;
    }

    /// Run the Phase-1b spike: try to instantiate `IOUSBHostControllerInterface`.
    /// Returns a process exit code — 0 = GO (controller created, entitlement
    /// honored), non-zero = NO-GO. Must run inside the signed+provisioned app
    /// bundle or the managed entitlement isn't honored.
    pub fn run_spike() -> i32 {
        unsafe { macrdp_usb_spike_run() as i32 }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn run_spike() -> i32 {
        eprintln!("--usb-spike is macOS-only");
        1
    }
}

pub use imp::run_spike;
