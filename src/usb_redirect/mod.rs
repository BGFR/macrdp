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
//! IOUSB` with its real VID/PID/strings. Phase 3.2 makes it USABLE, not just
//! enumerable: `select_client_config` sends a `SelectConfiguration` URB to open the
//! device's pipes, and [`imp::present_device`] then forwards bulk transfers on those
//! pipes ([`UsbHandle::bulk_transfer_in`]/`bulk_transfer_out`) both directions — so
//! the macOS mass-storage driver's SCSI (CBW/data/CSW over the bulk endpoints) rides
//! the client's real drive. **Verified end-to-end on a real Linux FreeRDP client**
//! (UTM-QEMU Ubuntu + a USB-2.0 hub, claimable mass-storage interface): a
//! client-redirected flash drive (ESD310C, VID 0x2174/PID 0x2100) **mounts on the
//! Mac and stays mounted** (1300+ steady bulk transfers, no resets/timeouts). Two
//! correctness fixes were load-bearing for that: (1) the same physical device is
//! deduped on its stable hardware identity (see [`PRESENTED_DEVICES`]) because the
//! client announces it twice with differing instance ids — presenting both duels
//! two virtual drives over one device (SCSI timeouts); (2) a completion that races a
//! device reset (which destroys+recreates the endpoint) is dropped on the Obj-C side
//! by endpoint-object identity, not just liveness — see `usb_spike.m`. EP0
//! **control-OUT** requests the local kernel issues (a mass-storage Bulk-Only Reset /
//! Clear-Feature(HALT), needed for SCSI error recovery) are forwarded to the real
//! device too, via [`UsbHandle::control_transfer_out`]; the standard requests the
//! host controller / SelectConfiguration already own
//! (SET_ADDRESS/CONFIGURATION/INTERFACE) stay a local ACK. EP0 **control-IN** is
//! generic too: a standard device-recipient `GET_DESCRIPTOR` rides the dedicated
//! descriptor URB (the verified enumeration path), and everything else — class/vendor
//! INs like mass-storage `Get Max LUN`, interface-directed ones like a HID
//! report-descriptor read — forwards via [`UsbHandle::control_transfer_in`] with the
//! raw SETUP preserved. Both directions translate the SETUP packet into the specific
//! **typed** URB function real Windows uses (`URB_FUNCTION_CLASS_INTERFACE`,
//! `GET_DESCRIPTOR_FROM_INTERFACE`, …) rather than the generic
//! `URB_FUNCTION_CONTROL_TRANSFER_EX`, which mstsc rejects with 0x80070057 (FreeRDP
//! accepts either) — see the vendored server's `setup_to_typed_urb`. Transfers are
//! serviced **concurrently** (one task each): the ObjC ring walk serializes per
//! endpoint, and cross-endpoint independence is what keeps a long-outstanding
//! transfer (an idle interrupt-IN, a wedged bulk) from blocking EP0 — notably the
//! reset that recovers the wedge. Every transfer await races `closed()` so a
//! disconnect mid-transfer tears down instead of wedging. Remaining 3.2 work:
//! retract/hot-unplug, true multi-device. All Obj-C /
//! IOUSBHost SPI touches are quarantined
//! in `usb_spike.m` (the maintenance boundary), built + linked by `build.rs` on
//! macOS; the standalone `--usb-spike` path still presents the hardcoded synthetic
//! device through the same async completion machinery.

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

/// Physical devices with a live presenting controller, keyed by the device's
/// stable hardware identity (VID:PID:bcdDevice), so we present each device exactly
/// once. A client can announce ONE physical device on more than one `URBDRC`
/// channel — e.g. FreeRDP announces the same drive twice with instance ids that
/// differ by a byte (`…d31` / `…d32`), and a reset re-announces it. Deduping on the
/// *client's instance id* fails there (the ids differ), so each announce would spin
/// up its own UserHCI controller for the SAME device — two virtual drives then duel
/// over the single client device (conflicting SCSI, 10 s timeouts, failed mount),
/// observed live. Keying on the descriptor identity collapses that to one. An entry
/// is removed when its controller tears down, so a genuine reset re-presents.
///
/// Limitation (multi-device is a deferred milestone): two *different* drives of the
/// identical model+revision share a key and only one presents. Distinguishing them
/// needs the iSerialNumber string — added when true multi-device lands.
static PRESENTED_DEVICES: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Claim the presenting slot for hardware-identity `key`. Returns `true` if this
/// call won it (present the device), `false` if another announce already holds it.
fn claim_device(key: &str) -> bool {
    PRESENTED_DEVICES.lock().unwrap().insert(key.to_owned())
}

/// Release the presenting slot for `key` (its controller went away).
fn release_device(key: &str) {
    PRESENTED_DEVICES.lock().unwrap().remove(key);
}

/// Presenting-side driver for one redirected USB device, called (with a
/// [`UsbHandle`] onto it) when the client announces it. Fetches the device
/// descriptor first (proving the transfer path cross-platform) and dedups on its
/// hardware identity so one physical device gets one controller (see
/// [`PRESENTED_DEVICES`]), then hands the handle to [`imp::present_device`] — which
/// on the entitled macOS build creates a UserHCI controller and answers the
/// kernel's EP0 transfers by driving this same handle, so the client's device
/// enumerates locally in `ioreg`.
fn drive_device(handle: UsbHandle) {
    tokio::spawn(async move {
        // Fetch the descriptor BEFORE claiming: the dedup key is the device's stable
        // hardware identity, which only the descriptor carries (the client's instance
        // id varies per announce — see PRESENTED_DEVICES). This read is cheap and was
        // already on the path.
        let desc = match handle.device_descriptor().await {
            Ok(d) => d,
            Err(e) => {
                warn!(error = %e, "USB redirection: device-descriptor fetch failed");
                return;
            }
        };
        let key = format!(
            "{:04x}:{:04x}:{:04x}",
            desc.vendor_id, desc.product_id, desc.device_release
        );
        if !claim_device(&key) {
            info!(
                identity = %key,
                instance_id = %handle.device_instance_id(),
                "USB redirection: device already presented (another announce of the same hardware) — skipping duplicate"
            );
            return;
        }
        info!(
            vid = format_args!("{:#06x}", desc.vendor_id),
            pid = format_args!("{:#06x}", desc.product_id),
            usb_version = format_args!("{:#06x}", desc.usb_version),
            device_class = desc.device_class,
            "USB redirection: device descriptor received (transfer round-trip GO)"
        );
        imp::present_device(handle).await;
        release_device(&key);
        info!(identity = %key, "USB redirection: presentation ended — dedup slot released");
    });
}

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::{HashMap, HashSet};
    use std::os::raw::{c_int, c_void};

    use ironrdp_server::{UsbHandle, UsbPipe};
    use tracing::{info, warn};

    // ---- USB standard-request constants ----
    const USB_REQ_GET_DESCRIPTOR: u8 = 0x06;
    const USB_DESCRIPTOR_TYPE_CONFIGURATION: u8 = 2;
    /// bmRequestType of a standard, device-recipient, device-to-host request —
    /// the exact shape of a standard `GET_DESCRIPTOR`. Class/vendor or
    /// interface/endpoint-directed INs (Get Max LUN `0xa1`, HID report-descriptor
    /// reads `0x81`) don't match and go through the generic control-IN forward.
    const USB_BMREQ_STANDARD_DEVICE_IN: u8 = 0x80;
    const STATUS_OK: i32 = 0;
    const STATUS_STALL: i32 = 1;

    /// ObjC -> Rust: the kernel wants an EP0 control-IN data stage serviced.
    /// Non-blocking — copies `setup8` and pushes it to the device driver loop.
    type ControlInFn = extern "C" fn(ctx: *mut c_void, token: u64, setup8: *const u8, max_len: u32);

    /// ObjC -> Rust: the kernel wants an EP0 control-**OUT** (host->device) request
    /// forwarded to the real device — e.g. a mass-storage reset / Clear-Feature.
    /// `setup8` is the 8-byte SETUP; `out_data`/`out_len` is any payload (empty for
    /// the no-data requests, the common case). Non-blocking.
    type ControlOutFn = extern "C" fn(
        ctx: *mut c_void,
        token: u64,
        setup8: *const u8,
        out_data: *const u8,
        out_len: u32,
    );

    /// ObjC -> Rust: the kernel wants a bulk/interrupt transfer on `endpoint_address`
    /// serviced. `is_in` != 0 is a device->host read of `out_len` bytes; else it's a
    /// host->device write of the `out_len` bytes at `out_data`. Non-blocking.
    type BulkFn = extern "C" fn(
        ctx: *mut c_void,
        token: u64,
        endpoint_address: u32,
        is_in: c_int,
        out_data: *const u8,
        out_len: u32,
    );

    unsafe extern "C" {
        fn macrdp_usb_spike_run() -> c_int;
        fn macrdp_usb_controller_create(
            control_cb: ControlInFn,
            control_out_cb: ControlOutFn,
            bulk_cb: BulkFn,
            ctx: *mut c_void,
            err: *mut c_int,
        ) -> *mut c_void;
        fn macrdp_usb_controller_destroy(handle: *mut c_void);
        // Completes a previously-raised control-IN OR bulk transfer. `bytes`/`len`
        // are the IN data to copy into the transfer buffer; for an OUT transfer pass
        // `bytes = NULL` and `len` = bytes accepted (reported as the transfer length).
        fn macrdp_usb_complete_transfer(
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

    /// A transfer the kernel raised, to service via the client and complete by
    /// `token`. EP0 control-IN reads a descriptor; bulk moves data on an endpoint.
    enum TransferReq {
        ControlIn {
            token: u64,
            setup: [u8; 8],
            max_len: u32,
        },
        ControlOut {
            token: u64,
            setup: [u8; 8],
            data: Vec<u8>,
        },
        Bulk {
            token: u64,
            endpoint_address: u8,
            is_in: bool,
            /// IN: bytes to read. OUT: `data.len()` (the write length).
            length: u32,
            /// OUT payload (empty for IN).
            data: Vec<u8>,
        },
    }

    /// The ctx handed to the C callbacks: the channel into the driver loop. Leaked
    /// for the controller's lifetime (a late callback after teardown is harmless —
    /// the send just fails once the receiver is gone). Bounded per presented device.
    struct CallbackCtx {
        tx: tokio::sync::mpsc::UnboundedSender<TransferReq>,
    }

    extern "C" fn control_in_cb(ctx: *mut c_void, token: u64, setup8: *const u8, max_len: u32) {
        // SAFETY: `ctx` is the leaked CallbackCtx from present_device; `setup8`
        // points at 8 readable bytes (the ObjC side's stack SETUP packet).
        let ctx = unsafe { &*(ctx as *const CallbackCtx) };
        let mut setup = [0u8; 8];
        unsafe { std::ptr::copy_nonoverlapping(setup8, setup.as_mut_ptr(), 8) };
        let _ = ctx.tx.send(TransferReq::ControlIn {
            token,
            setup,
            max_len,
        });
    }

    extern "C" fn control_out_cb(
        ctx: *mut c_void,
        token: u64,
        setup8: *const u8,
        out_data: *const u8,
        out_len: u32,
    ) {
        // SAFETY: `ctx` is the leaked CallbackCtx; `setup8` points at 8 readable
        // bytes; for a payload request `out_data` points at `out_len` readable bytes.
        let ctx = unsafe { &*(ctx as *const CallbackCtx) };
        let mut setup = [0u8; 8];
        unsafe { std::ptr::copy_nonoverlapping(setup8, setup.as_mut_ptr(), 8) };
        let data = if !out_data.is_null() && out_len > 0 {
            unsafe { std::slice::from_raw_parts(out_data, out_len as usize).to_vec() }
        } else {
            Vec::new()
        };
        let _ = ctx.tx.send(TransferReq::ControlOut { token, setup, data });
    }

    extern "C" fn bulk_cb(
        ctx: *mut c_void,
        token: u64,
        endpoint_address: u32,
        is_in: c_int,
        out_data: *const u8,
        out_len: u32,
    ) {
        // SAFETY: `ctx` is the leaked CallbackCtx; for an OUT transfer `out_data`
        // points at `out_len` readable bytes (the ObjC side's transfer buffer).
        let ctx = unsafe { &*(ctx as *const CallbackCtx) };
        let is_in = is_in != 0;
        let data = if !is_in && !out_data.is_null() && out_len > 0 {
            unsafe { std::slice::from_raw_parts(out_data, out_len as usize).to_vec() }
        } else {
            Vec::new()
        };
        let _ = ctx.tx.send(TransferReq::Bulk {
            token,
            endpoint_address: endpoint_address as u8,
            is_in,
            length: out_len,
            data,
        });
    }

    /// Owns the opaque UserHCI controller handle; `destroy` on drop. Only the
    /// thread-safe C functions (which `dispatch_async` onto the controller's
    /// serial queue) are called through it, so it is safe to move AND share across
    /// tokio threads: concurrent `complete_*` calls just enqueue blocks on the one
    /// serial queue (Send for the move into tasks, Sync for `Arc` sharing between
    /// them; `destroy` needs `&mut`/drop, so it can't race the shared calls).
    struct ControllerHandle(*mut c_void);
    unsafe impl Send for ControllerHandle {}
    unsafe impl Sync for ControllerHandle {}
    impl ControllerHandle {
        /// Complete an IN transfer (control-IN or bulk-IN): `bytes` are copied into
        /// the kernel's transfer buffer.
        fn complete_in(&self, token: u64, bytes: &[u8], status: i32) {
            unsafe {
                macrdp_usb_complete_transfer(
                    self.0,
                    token,
                    bytes.as_ptr(),
                    bytes.len() as u32,
                    status,
                )
            };
        }

        /// Complete an OUT transfer: no data to copy back; `moved` is the number of
        /// bytes the device accepted (reported as the transfer length).
        fn complete_out(&self, token: u64, moved: u32, status: i32) {
            unsafe { macrdp_usb_complete_transfer(self.0, token, std::ptr::null(), moved, status) };
        }
    }
    impl Drop for ControllerHandle {
        fn drop(&mut self) {
            unsafe { macrdp_usb_controller_destroy(self.0) };
        }
    }

    /// Present the client's device locally: create the UserHCI controller and
    /// service its transfers (EP0 control IN/OUT + bulk/interrupt) from the client
    /// over `handle`. A no-op (logs once) when the controller can't be created —
    /// e.g. a plain `cargo` build without the
    /// `com.apple.developer.usb.host-controller-interface` entitlement. Runs until
    /// `handle.closed()` fires (the owning device processor dropped on disconnect),
    /// then destroys the controller; every in-flight transfer await also races
    /// `closed()` inside `UsbHandle`, so a disconnect mid-transfer can't wedge the
    /// teardown. Fuller lifecycle (mid-session retract / multi-device) is deferred.
    pub async fn present_device(handle: UsbHandle) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TransferReq>();
        // Leak the ctx (a small, bounded per-presentation allocation) so a C
        // callback that races teardown stays memory-safe — the send just fails
        // once `rx` is dropped. Freeing it needs a guaranteed-drained queue,
        // which the controller destroy doesn't promise; not worth the risk.
        let ctx = Box::into_raw(Box::new(CallbackCtx { tx }));
        let mut err: c_int = 0;
        let raw = unsafe {
            macrdp_usb_controller_create(
                control_in_cb,
                control_out_cb,
                bulk_cb,
                ctx as *mut c_void,
                &mut err,
            )
        };
        if raw.is_null() {
            warn!(err, "USB redirection: UserHCI controller unavailable (needs the entitled build) — not presenting");
            return;
        }
        // Arc so per-transfer service tasks can complete after the loop exits; the
        // ObjC controller is destroyed only when the last in-flight task drops its
        // clone, so a completion can never race the destroy.
        let controller = std::sync::Arc::new(ControllerHandle(raw));
        info!("USB redirection: UserHCI controller created — presenting the client's device (watch `ioreg -p IOUSB`)");

        // Configure the client's device + learn its endpoint pipe handles — the
        // prerequisite for bulk transfers (which address the endpoint by the pipe
        // handle SelectConfiguration returns). `pipe_map` maps a bulk endpoint's
        // address (e.g. 0x01 OUT, 0x82 IN) to its client pipe handle. Best-effort:
        // control-only enumeration still works without it (bulk just can't run).
        let mut pipe_map: HashMap<u8, u32> = HashMap::new();
        // Non-bulk (interrupt) IN endpoints must not HALT on a transient failure —
        // see the bulk-IN error arm below. Track them so mass-storage BULK pipes
        // keep the strict stall.
        let mut interrupt_eps: HashSet<u8> = HashSet::new();
        match select_client_config(&handle).await {
            Ok(pipes) => {
                for p in &pipes {
                    info!(
                        endpoint = format_args!("{:#04x}", p.endpoint_address),
                        pipe_handle = p.pipe_handle,
                        bulk = p.is_bulk,
                        "USB redirection: endpoint pipe opened"
                    );
                    pipe_map.insert(p.endpoint_address, p.pipe_handle);
                    if !p.is_bulk {
                        interrupt_eps.insert(p.endpoint_address);
                    }
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

        // Service each raised transfer in ITS OWN task, so endpoints don't block
        // each other — the ObjC ring walk already serializes per endpoint (at most
        // one outstanding forwarded transfer each), and cross-endpoint concurrency
        // is how a real host controller behaves. Serializing everything through one
        // await was a deadlock-by-design for any device with a long-outstanding
        // transfer (an interrupt-IN idles until the device has data — HID) and let a
        // wedged bulk block the mass-storage RESET control-OUT queued behind it —
        // the exact recovery it exists for. A transfer failure stalls just that
        // transfer (the class driver retries / resets); teardown is solely
        // handle.closed() (link death), which also aborts every in-flight await.
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
            match req {
                TransferReq::ControlIn {
                    token,
                    setup,
                    max_len,
                } => {
                    let handle = handle.clone();
                    let controller = std::sync::Arc::clone(&controller);
                    tokio::spawn(async move {
                        // A standard device-recipient GET_DESCRIPTOR rides the
                        // dedicated descriptor URB (the verified enumeration path);
                        // every other IN (class/vendor like Get Max LUN, or
                        // interface-directed like a HID report-descriptor read)
                        // forwards generically with its raw SETUP preserved.
                        let result = if setup[0] == USB_BMREQ_STANDARD_DEVICE_IN
                            && setup[1] == USB_REQ_GET_DESCRIPTOR
                        {
                            // GET_DESCRIPTOR wValue: high byte = type, low = index.
                            let lang_id = u16::from_le_bytes([setup[4], setup[5]]);
                            handle
                                .get_descriptor(setup[3], setup[2], lang_id, max_len)
                                .await
                        } else {
                            handle.control_transfer_in(setup, max_len).await
                        };
                        match result {
                            Ok(bytes) => controller.complete_in(token, &bytes, STATUS_OK),
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    token,
                                    bmreq = format_args!("{:#04x}", setup[0]),
                                    breq = format_args!("{:#04x}", setup[1]),
                                    wvalue = format_args!("{:#06x}", u16::from_le_bytes([setup[2], setup[3]])),
                                    windex = format_args!("{:#06x}", u16::from_le_bytes([setup[4], setup[5]])),
                                    "USB redirection: control-IN forward failed — stalling"
                                );
                                controller.complete_in(token, &[], STATUS_STALL);
                            }
                        }
                    });
                }
                TransferReq::ControlOut { token, setup, data } => {
                    // Forward a host->device EP0 request to the real device (e.g. a
                    // mass-storage Bulk-Only Reset / Clear-Feature(HALT)).
                    let handle = handle.clone();
                    let controller = std::sync::Arc::clone(&controller);
                    tokio::spawn(async move {
                        match handle.control_transfer_out(setup, data).await {
                            Ok(()) => controller.complete_out(token, 0, STATUS_OK),
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    token,
                                    bmreq = format_args!("{:#04x}", setup[0]),
                                    breq = format_args!("{:#04x}", setup[1]),
                                    wvalue = format_args!("{:#06x}", u16::from_le_bytes([setup[2], setup[3]])),
                                    windex = format_args!("{:#06x}", u16::from_le_bytes([setup[4], setup[5]])),
                                    "USB redirection: control-OUT forward failed — stalling"
                                );
                                controller.complete_out(token, 0, STATUS_STALL);
                            }
                        }
                    });
                }
                TransferReq::Bulk {
                    token,
                    endpoint_address,
                    is_in,
                    length,
                    data,
                } => {
                    let Some(&pipe) = pipe_map.get(&endpoint_address) else {
                        warn!(
                            endpoint = format_args!("{:#04x}", endpoint_address),
                            "USB redirection: bulk transfer on an unmapped endpoint — stalling"
                        );
                        controller.complete_out(token, 0, STATUS_STALL);
                        continue;
                    };
                    let is_interrupt = interrupt_eps.contains(&endpoint_address);
                    let handle = handle.clone();
                    let controller = std::sync::Arc::clone(&controller);
                    tokio::spawn(async move {
                        if is_in {
                            match handle.bulk_transfer_in(pipe, length).await {
                                Ok(bytes) => controller.complete_in(token, &bytes, STATUS_OK),
                                Err(e) => {
                                    // An INTERRUPT-IN endpoint must not HALT on a
                                    // transient device failure. mstsc intermittently
                                    // fails an interrupt read with hresult 0x8007001f
                                    // (ERROR_GEN_FAILURE); completing that as a STALL
                                    // makes the macOS class driver give up polling the
                                    // pipe — e.g. a redirected gamepad goes dead
                                    // ("hang") after a few seconds. Treat a transient
                                    // failure as an EMPTY poll (0 bytes, success) so the
                                    // OS keeps the pipe alive and re-polls (interrupt
                                    // "no data ready" semantics — one dropped report is
                                    // imperceptible, and the next poll gets fresh state).
                                    // Link death (channel closed) is fatal → stall so
                                    // teardown proceeds. BULK endpoints (mass storage)
                                    // keep the strict stall: a real bulk error must
                                    // surface, and faking a short read would corrupt the
                                    // transfer.
                                    let fatal = e.to_string().contains("channel closed");
                                    if is_interrupt && !fatal {
                                        info!(error = %e, endpoint = format_args!("{:#04x}", endpoint_address), "USB redirection: interrupt-IN transient failure — completing as an empty poll to keep the pipe alive");
                                        controller.complete_in(token, &[], STATUS_OK);
                                    } else {
                                        warn!(error = %e, endpoint = format_args!("{:#04x}", endpoint_address), requested = length, "USB redirection: bulk IN failed — stalling");
                                        controller.complete_in(token, &[], STATUS_STALL);
                                    }
                                }
                            }
                        } else {
                            let n = data.len() as u32;
                            match handle.bulk_transfer_out(pipe, data).await {
                                Ok(_) => controller.complete_out(token, n, STATUS_OK),
                                Err(e) => {
                                    warn!(error = %e, endpoint = format_args!("{:#04x}", endpoint_address), "USB redirection: bulk OUT failed — stalling");
                                    controller.complete_out(token, 0, STATUS_STALL);
                                }
                            }
                        }
                    });
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
    // test uses its own unique hardware-identity keys to stay ordering-independent.

    #[test]
    fn claims_once_then_releases() {
        let id = "2174:2100:0100-claims-once";
        assert!(claim_device(id), "first claim wins");
        assert!(
            !claim_device(id),
            "second announce of the same hardware is skipped"
        );
        release_device(id);
        assert!(
            claim_device(id),
            "claimable again after its controller released (genuine reset re-add)"
        );
        release_device(id);
    }

    #[test]
    fn distinct_devices_are_independent() {
        let a = "2174:2100:0100-distinct-a";
        let b = "0781:5583:0010-distinct-b";
        assert!(claim_device(a));
        assert!(claim_device(b), "a different device presents independently");
        release_device(a);
        release_device(b);
    }
}
