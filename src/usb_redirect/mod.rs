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

use std::sync::Arc;

use ironrdp_server::{
    ServerEvent, ServerEventSender, UrbdrcServer, UrbdrcServerFactory, UsbDeviceCallback, UsbHandle,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// macrdp's server-direction USB-redirection factory (`--enable-usb-redirection`).
///
/// Installs the vendored `UrbdrcServer` DVC processor (advertises `URBDRC`, drives
/// the MS-RDPEUSB init handshake, opens a per-device DVC per `ADD_DEVICE`), and —
/// via [`device_callback`](UrbdrcServerFactory::device_callback) — receives a
/// [`UsbHandle`] onto each announced device. [`drive_device`] is the presenting-
/// side driver: today it fetches + logs the real descriptor over the handle;
/// next it creates the macOS UserHCI controller (`usb_spike.m`) for the device
/// and answers the kernel's EP0 transfers by driving that same handle.
///
/// Cross-platform (pure protocol policy — no macOS APIs yet); the UserHCI
/// controller lives behind `--usb-spike` / the future `drive_device` body.
pub struct MacUsb {
    sender: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl MacUsb {
    pub fn new() -> Self {
        Self { sender: None }
    }
}

impl Default for MacUsb {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerEventSender for MacUsb {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.sender = Some(sender);
    }
}

impl UrbdrcServerFactory for MacUsb {
    fn build_processor(&self) -> UrbdrcServer {
        UrbdrcServer::with_sender(self.sender.clone())
    }

    fn device_callback(&self) -> Option<UsbDeviceCallback> {
        Some(Arc::new(drive_device))
    }
}

/// Presenting-side driver for one redirected USB device, called (with a
/// [`UsbHandle`] onto it) when the client announces it. **Phase 3.1b(2b):** fetch
/// and log the device's real descriptor over the handle, proving the transfer
/// path. **Next:** create the macOS UserHCI controller (`usb_spike.m`) for the
/// device and answer the kernel's EP0 transfers by driving this same handle.
fn drive_device(handle: UsbHandle) {
    tokio::spawn(async move {
        match handle.device_descriptor().await {
            Ok(d) => info!(
                vid = format_args!("{:#06x}", d.vendor_id),
                pid = format_args!("{:#06x}", d.product_id),
                usb_version = format_args!("{:#06x}", d.usb_version),
                device_class = d.device_class,
                "USB redirection: device descriptor received (transfer round-trip GO)"
            ),
            Err(e) => warn!(error = %e, "USB redirection: device-descriptor fetch failed"),
        }
    });
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
