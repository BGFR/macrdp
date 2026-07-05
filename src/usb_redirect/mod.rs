//! Generic USB redirection (MS-RDPEUSB) — **Phase-2 synthetic-device spike**.
//!
//! macrdp is the RDP server / presenting side: the physical USB device lives on
//! the client (which runs `urbdrc`), and the server drives it and presents it
//! locally. The macOS presenting mechanism is a user-space virtual USB host
//! controller via the public `IOUSBHost.framework` class
//! `IOUSBHostControllerInterface`, which requires the (granted) entitlement
//! `com.apple.developer.usb.host-controller-interface`.
//!
//! Phase 1b proved the controller instantiates in a signed+provisioned build.
//! Phase 2 (this code) drives the full UserHCI command protocol — controller /
//! port / device / endpoint state machines + EP0 GET_DESCRIPTOR handling — to
//! present a HARDCODED vendor-specific device so it enumerates in `ioreg -p
//! IOUSB`. Phase 3 replaces the hardcoded descriptors + transfer handling with
//! the real device redirected over MS-RDPEUSB (DVC, reusing upstream
//! `ironrdp-rdpeusb`'s PDU layer). All Obj-C / IOUSBHost SPI touches are
//! quarantined in `usb_spike.m` (the maintenance boundary), built + linked by
//! `build.rs` on macOS.

#[cfg(target_os = "macos")]
mod imp {
    unsafe extern "C" {
        fn macrdp_usb_spike_run() -> core::ffi::c_int;
    }

    /// Run the Phase-2 spike: instantiate `IOUSBHostControllerInterface` and
    /// drive the UserHCI command/doorbell protocol to enumerate a hardcoded
    /// synthetic USB device. Returns a process exit code — 0 = controller
    /// created + enumeration loop ran (check `ioreg -p IOUSB` for the device
    /// and the `[usb2]` log lines for how far the kernel drove us), non-zero =
    /// controller init failed. Must run inside the signed+provisioned app
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
