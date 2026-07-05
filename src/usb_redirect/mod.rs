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

use ironrdp_server::{ServerEvent, ServerEventSender, UrbdrcServer, UrbdrcServerFactory};
use tokio::sync::mpsc;

/// macrdp's server-direction USB-redirection factory (`--enable-usb-redirection`).
///
/// **Phase 3.0 (observe-only):** installs the vendored `UrbdrcServer` DVC
/// processor, which advertises the `URBDRC` channel, runs the MS-RDPEUSB
/// capability exchange, and logs what a client announces — answering the
/// go/no-go question (does a reachable client open `URBDRC` + announce a device?)
/// before the transfer-forwarding machinery is built. Phase 3.1 grows this to
/// build a `UsbHandle` and own the macOS UserHCI controller.
///
/// Cross-platform (pure protocol policy — no macOS APIs yet); the presenting
/// side lives behind `--usb-spike` / the future controller module.
pub struct MacUsb;

impl MacUsb {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MacUsb {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerEventSender for MacUsb {
    fn set_sender(&mut self, _sender: mpsc::UnboundedSender<ServerEvent>) {
        // No-op for the observe-only spike — there's no outbound `UsbHandle` yet
        // (Phase 3.1 will retain the sender to drive transfers).
    }
}

impl UrbdrcServerFactory for MacUsb {
    fn build_processor(&self) -> UrbdrcServer {
        UrbdrcServer::new()
    }
}

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
