//! Generic USB redirection (MS-RDPEUSB): present a client's redirected USB device
//! locally on macOS.
//!
//! macrdp is the RDP server / presenting side: the physical USB device lives on
//! the client (which runs `urbdrc`), and the server drives it and presents it
//! locally. The macOS presenting mechanism is a user-space virtual USB host
//! controller via the public `IOUSBHost.framework` class
//! `IOUSBHostControllerInterface`, which requires the (granted) entitlement
//! `com.apple.developer.usb.host-controller-interface`.
//!
//! Phase 1b proved the controller instantiates in a signed+provisioned build.
//! Phase 2 drove the full UserHCI command protocol to present a HARDCODED device.
//! Phase 3 makes it REAL and client-sourced: [`MacUsb`] installs the vendored
//! `UrbdrcServer`, and on `ADD_DEVICE` [`drive_device`] -> [`imp::present_device`]
//! creates a UserHCI controller and answers the kernel's EP0 GET_DESCRIPTOR
//! transfers by driving the device's `UsbHandle` — so the descriptors come from
//! the client over the wire and the client's device enumerates in `ioreg -p
//! IOUSB` with its real VID/PID/strings. **Verified** on the entitled build: a
//! client-redirected flash drive (ESD310C, VID 0x2174) enumerates on macrdp's
//! controller. Phase 3.2 (usable, not just enumerable) is in progress:
//! `select_client_config` fetches the full config descriptor and sends a
//! `SelectConfiguration` URB to open the device's pipes (the prerequisite for
//! bulk) — verified correct (FreeRDP parses + attempts it), but it CANNOT be
//! completed on the loopback test because a macOS *client*'s libusb can't detach
//! the mass-storage kernel driver to claim the interface (`LIBUSB_ERROR_ACCESS`);
//! it times out and degrades to enumerate-only. Bulk forwarding + a
//! claimable-interface client (real Windows/Linux) are the remaining 3.2 work,
//! along with retract/hot-unplug and multi-device. All Obj-C / IOUSBHost SPI
//! touches are quarantined in `usb_spike.m` (the maintenance boundary), built +
//! linked by `build.rs` on macOS; the standalone `--usb-spike` path still presents
//! the hardcoded synthetic device through the same async completion machinery.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex};

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
/// side driver: it dedups the device (one controller per physical device), fetches
/// and logs the real descriptor over the handle, then creates the macOS UserHCI
/// controller (`usb_spike.m`) for it and answers the kernel's EP0 transfers by
/// driving that same handle.
///
/// This factory + `drive_device` are cross-platform (pure protocol/dedup policy);
/// every macOS-only touch (the UserHCI controller) is confined to the `imp`
/// submodule below.
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

/// Physical devices with a live presenting controller, keyed by device-instance
/// id, so we present each device exactly once. A client can announce one physical
/// device on more than one `URBDRC` channel (a SuperSpeed device announced as both
/// USB-2 and USB-3, a composite device, or a mid-session re-announce); without
/// this each announce would spin up its own UserHCI controller for the *same*
/// device — a harmless-but-confusing duplicate in `ioreg`. An entry is removed
/// when its controller tears down, so a later genuine re-add still presents.
static PRESENTED_DEVICES: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Claim the presenting slot for `instance_id`. Returns `true` if this call won it
/// (present the device), `false` if another channel already holds it (skip). A
/// device that reports no instance id (`""`) is never deduped — better a possible
/// duplicate than hiding a genuine second device behind an empty key.
fn claim_device(instance_id: &str) -> bool {
    instance_id.is_empty()
        || PRESENTED_DEVICES
            .lock()
            .unwrap()
            .insert(instance_id.to_owned())
}

/// Release the presenting slot for `instance_id` (its controller went away).
fn release_device(instance_id: &str) {
    if !instance_id.is_empty() {
        PRESENTED_DEVICES.lock().unwrap().remove(instance_id);
    }
}

/// Presenting-side driver for one redirected USB device, called (with a
/// [`UsbHandle`] onto it) when the client announces it. Dedups so one physical
/// device gets one controller (see [`PRESENTED_DEVICES`]), fetches + logs the real
/// device descriptor (proving the transfer path cross-platform), then hands the
/// handle to [`imp::present_device`] — which on the entitled macOS build creates
/// a UserHCI controller and answers the kernel's EP0 transfers by driving this
/// same handle, so the client's device enumerates locally in `ioreg`.
fn drive_device(handle: UsbHandle) {
    tokio::spawn(async move {
        let instance_id = handle.device_instance_id().to_owned();
        if !claim_device(&instance_id) {
            info!(
                instance_id = %instance_id,
                "USB redirection: device already presented on another channel — skipping duplicate"
            );
            return;
        }
        match handle.device_descriptor().await {
            Ok(d) => info!(
                vid = format_args!("{:#06x}", d.vendor_id),
                pid = format_args!("{:#06x}", d.product_id),
                usb_version = format_args!("{:#06x}", d.usb_version),
                device_class = d.device_class,
                "USB redirection: device descriptor received (transfer round-trip GO)"
            ),
            Err(e) => {
                warn!(error = %e, "USB redirection: device-descriptor fetch failed");
                release_device(&instance_id);
                return;
            }
        }
        imp::present_device(handle).await;
        release_device(&instance_id);
    });
}

#[cfg(target_os = "macos")]
mod imp {
    use std::os::raw::{c_int, c_void};

    use ironrdp_server::{UsbHandle, UsbPipe};
    use tracing::{info, warn};

    // ---- USB standard-request constants ----
    const USB_REQ_GET_DESCRIPTOR: u8 = 0x06;
    const USB_DESCRIPTOR_TYPE_CONFIGURATION: u8 = 2;
    const STATUS_OK: i32 = 0;
    const STATUS_STALL: i32 = 1;

    /// ObjC -> Rust: the kernel wants an EP0 control-IN data stage serviced.
    /// Non-blocking — copies `setup8` and pushes it to the device driver loop.
    type ControlInFn = extern "C" fn(ctx: *mut c_void, token: u64, setup8: *const u8, max_len: u32);

    unsafe extern "C" {
        fn macrdp_usb_spike_run() -> c_int;
        fn macrdp_usb_controller_create(
            cb: ControlInFn,
            ctx: *mut c_void,
            err: *mut c_int,
        ) -> *mut c_void;
        fn macrdp_usb_controller_destroy(handle: *mut c_void);
        fn macrdp_usb_complete_control_in(
            handle: *mut c_void,
            token: u64,
            bytes: *const u8,
            len: u32,
            status: i32,
        );
    }

    /// Run the Phase-2 spike: instantiate `IOUSBHostControllerInterface` and
    /// drive the UserHCI command/doorbell protocol to enumerate a hardcoded
    /// synthetic USB device (through the async completion path). Returns a
    /// process exit code — 0 = controller created + enumeration loop ran (check
    /// `ioreg -p IOUSB` for the device and the `[usb2]` logs), non-zero = init
    /// failed. Must run inside the signed+provisioned app bundle or the managed
    /// entitlement isn't honored.
    pub fn run_spike() -> i32 {
        unsafe { macrdp_usb_spike_run() as i32 }
    }

    /// One EP0 control-IN request raised by the kernel, to service via the client.
    struct ControlInReq {
        token: u64,
        setup: [u8; 8],
        max_len: u32,
    }

    /// The ctx handed to the C callback: the channel into the driver loop. Leaked
    /// for the controller's lifetime (a late callback after teardown is harmless —
    /// the send just fails once the receiver is gone). Bounded per presented
    /// device; full teardown is Phase 3.2.
    struct CallbackCtx {
        tx: tokio::sync::mpsc::UnboundedSender<ControlInReq>,
    }

    extern "C" fn control_in_cb(ctx: *mut c_void, token: u64, setup8: *const u8, max_len: u32) {
        // SAFETY: `ctx` is the leaked CallbackCtx from present_device; `setup8`
        // points at 8 readable bytes (the ObjC side's stack SETUP packet).
        let ctx = unsafe { &*(ctx as *const CallbackCtx) };
        let mut setup = [0u8; 8];
        unsafe { std::ptr::copy_nonoverlapping(setup8, setup.as_mut_ptr(), 8) };
        let _ = ctx.tx.send(ControlInReq {
            token,
            setup,
            max_len,
        });
    }

    /// Owns the opaque UserHCI controller handle; `destroy` on drop. Only the
    /// thread-safe C functions (which `dispatch_async` onto the serial queue) are
    /// called through it, so it is safe to move/share across tokio threads.
    struct ControllerHandle(*mut c_void);
    unsafe impl Send for ControllerHandle {}
    impl ControllerHandle {
        fn complete(&self, token: u64, bytes: &[u8], status: i32) {
            unsafe {
                macrdp_usb_complete_control_in(
                    self.0,
                    token,
                    bytes.as_ptr(),
                    bytes.len() as u32,
                    status,
                )
            };
        }
    }
    impl Drop for ControllerHandle {
        fn drop(&mut self) {
            unsafe { macrdp_usb_controller_destroy(self.0) };
        }
    }

    /// Present the client's device locally: create the UserHCI controller and
    /// service its EP0 control-IN transfers from the client over `handle`. A no-op
    /// (logs once) when the controller can't be created — e.g. a plain `cargo`
    /// build without the `com.apple.developer.usb.host-controller-interface`
    /// entitlement. Runs until the device goes away — either the client link
    /// fails on a fetch, or `handle.closed()` fires (the owning device processor
    /// dropped on disconnect) — then destroys the controller. Fuller lifecycle
    /// (mid-session retract / multi-device) is Phase 3.2.
    pub async fn present_device(handle: UsbHandle) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ControlInReq>();
        // Leak the ctx (a small, bounded per-presentation allocation) so a C
        // callback that races teardown stays memory-safe — the send just fails
        // once `rx` is dropped. Freeing it needs a guaranteed-drained queue,
        // which the controller destroy doesn't promise; not worth the risk.
        let ctx = Box::into_raw(Box::new(CallbackCtx { tx }));
        let mut err: c_int = 0;
        let raw =
            unsafe { macrdp_usb_controller_create(control_in_cb, ctx as *mut c_void, &mut err) };
        if raw.is_null() {
            warn!(err, "USB redirection: UserHCI controller unavailable (needs the entitled build) — not presenting");
            return;
        }
        let controller = ControllerHandle(raw);
        info!("USB redirection: UserHCI controller created — presenting the client's device (watch `ioreg -p IOUSB`)");

        // Configure the client's device + learn its endpoint pipe handles — the
        // prerequisite for bulk transfers (which address the endpoint by the pipe
        // handle SelectConfiguration returns). Best-effort: control-only enumeration
        // still works without it. Bulk forwarding (the actual mount) is the next step.
        match select_client_config(&handle).await {
            Ok(pipes) => {
                for p in &pipes {
                    info!(
                        endpoint = format_args!("{:#04x}", p.endpoint_address),
                        pipe_handle = p.pipe_handle,
                        bulk = p.is_bulk,
                        "USB redirection: endpoint pipe opened"
                    );
                }
                info!(
                    count = pipes.len(),
                    "USB redirection: SelectConfiguration succeeded — pipes ready for bulk"
                );
            }
            Err(e) => {
                warn!(error = %e, "USB redirection: SelectConfiguration failed (bulk transfers unavailable)")
            }
        }

        // EP0 control transfers are serialized by the kernel, so service each
        // inline. Stop when the device goes away: a fetch failure (link dead) or
        // the device channel closing (disconnect) via handle.closed().
        loop {
            let req = tokio::select! {
                maybe = rx.recv() => match maybe {
                    Some(req) => req,
                    None => break,
                },
                () = handle.closed() => {
                    info!("USB redirection: device channel closed (disconnect) — destroying UserHCI controller");
                    break;
                }
            };
            let b_request = req.setup[1];
            if b_request != USB_REQ_GET_DESCRIPTOR {
                // Only descriptor reads are forwarded today (enough to enumerate);
                // stall the rest (SET_* have no data stage and never reach here).
                controller.complete(req.token, &[], STATUS_STALL);
                continue;
            }
            // GET_DESCRIPTOR wValue: high byte = descriptor type, low byte = index.
            let desc_index = req.setup[2];
            let desc_type = req.setup[3];
            let lang_id = u16::from_le_bytes([req.setup[4], req.setup[5]]);
            match handle
                .get_descriptor(desc_type, desc_index, lang_id, req.max_len)
                .await
            {
                Ok(bytes) => controller.complete(req.token, &bytes, STATUS_OK),
                Err(e) => {
                    warn!(error = %e, token = req.token, "USB redirection: descriptor fetch failed — tearing down");
                    controller.complete(req.token, &[], STATUS_STALL);
                    break;
                }
            }
        }
        drop(controller);
    }

    /// Fetch the full configuration descriptor (header first for `wTotalLength`,
    /// then the whole thing) and SelectConfiguration on the client, returning the
    /// opened endpoint pipes.
    async fn select_client_config(handle: &UsbHandle) -> anyhow::Result<Vec<UsbPipe>> {
        info!("USB redirection: fetching configuration-descriptor header");
        let hdr = handle
            .get_descriptor(USB_DESCRIPTOR_TYPE_CONFIGURATION, 0, 0, 9)
            .await?;
        if hdr.len() < 4 {
            anyhow::bail!(
                "short configuration-descriptor header ({} bytes)",
                hdr.len()
            );
        }
        let total_len = u16::from_le_bytes([hdr[2], hdr[3]]);
        info!(
            total_len,
            "USB redirection: fetching full configuration descriptor"
        );
        let full = handle
            .get_descriptor(
                USB_DESCRIPTOR_TYPE_CONFIGURATION,
                0,
                0,
                u32::from(total_len),
            )
            .await?;
        info!(
            bytes = full.len(),
            "USB redirection: sending SelectConfiguration"
        );
        // A client that can't configure the device (e.g. a macOS client whose
        // kernel driver holds a mass-storage interface, so libusb can't claim it)
        // may fail the URB without a completion — bound the wait so we don't hang.
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle.select_configuration(&full),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => anyhow::bail!(
                "SelectConfiguration timed out — client did not complete it (interface claimable?)"
            ),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use ironrdp_server::UsbHandle;

    pub fn run_spike() -> i32 {
        eprintln!("--usb-spike is macOS-only");
        1
    }

    pub async fn present_device(_handle: UsbHandle) {
        // USB presenting is macOS-only (IOUSBHost UserHCI); no-op elsewhere.
    }
}

pub use imp::run_spike;

#[cfg(test)]
mod tests {
    use super::{claim_device, release_device};

    // The dedup registry is a process-wide static shared across tests, so each
    // test uses its own unique instance ids to stay independent of ordering.

    #[test]
    fn claims_once_then_releases() {
        let id = "test-instance-claims-once";
        assert!(claim_device(id), "first claim wins");
        assert!(
            !claim_device(id),
            "second claim on a live device is skipped"
        );
        release_device(id);
        assert!(
            claim_device(id),
            "claimable again after its controller released"
        );
        release_device(id);
    }

    #[test]
    fn distinct_devices_are_independent() {
        let a = "test-instance-distinct-a";
        let b = "test-instance-distinct-b";
        assert!(claim_device(a));
        assert!(claim_device(b), "a different device presents independently");
        release_device(a);
        release_device(b);
    }

    #[test]
    fn empty_instance_id_is_never_deduped() {
        // An absent instance id must never merge two genuinely different devices.
        assert!(claim_device(""));
        assert!(claim_device(""), "empty id always presents");
    }
}
