//! Server-direction MS-RDPEUSB (`URBDRC` DVC) — USB device redirection.
//!
//! macrdp is the RDP **server**: the RDP client owns a physical USB device and
//! redirects it over the `URBDRC` dynamic virtual channel; this server drives it
//! and presents it locally (on macOS, via a user-space USB host controller — see
//! macrdp's `src/usb_redirect`). This module is the server-side DVC processor,
//! written against the vendored `ironrdp-rdpeusb` **PDU layer** (the pinned rev is
//! PDU-only; the upstream `UrbdrcControlServer`/`UrbdrcDeviceServer` processors
//! live 3 PRs ahead and would force a foundation-wide pin bump, so we drive the
//! wire ourselves here — the same pattern as the server-direction RDPDR in
//! `src/rdpdr.rs`). Divergence 16.
//!
//! **Init handshake (Phase 3.0).** On the main `URBDRC` channel the server drives
//! the MS-RDPEUSB initialization sequence — RIM capability exchange →
//! `CHANNEL_CREATED` → `RIMCALL_RELEASE` — which makes the client announce each
//! redirected device with an `ADD_VIRTUAL_CHANNEL` on the device-sink interface.
//!
//! **Per-device channel (Phase 3.1a).** Each `ADD_VIRTUAL_CHANNEL` asks the server
//! to open a *new* `URBDRC` DVC for that device. The main [`UrbdrcServer`] can't
//! reach [`DrdynvcServer`](ironrdp_dvc::DrdynvcServer) from inside `process()`, so
//! it signals the server event loop via [`ServerEvent::Urbdrc`]; the loop calls
//! `DrdynvcServer::create_channel` with a [`UrbdrcDeviceProcessor`]. That device
//! processor sends its own `RIMCALL_RELEASE` on open (FreeRDP's `INIT_CHANNEL_OUT`
//! barrier), which makes the client send `ADD_DEVICE` — the real device
//! descriptors — on the per-device channel. Transfers (`UsbHandle`/router + the
//! IOUSBHost bridge) are Phase 3.1b.

use core::fmt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use ironrdp_core::{Encode, EncodeResult, WriteCursor, decode, impl_as_any};
use ironrdp_dvc::{DvcEncode, DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::PduResult;
use ironrdp_rdpeusb::pdu::caps::{Capability, RimExchangeCapabilityRequest};
use ironrdp_rdpeusb::pdu::header::{FunctionId, InterfaceId, Mask, SharedMsgHeader};
use ironrdp_rdpeusb::pdu::notify::{ChannelCreated, Direction};
use ironrdp_rdpeusb::pdu::usb_dev::ts_urb::utils::{TsUrbHeader, UrbFunction};
use ironrdp_rdpeusb::pdu::usb_dev::ts_urb::{TsUrb, TsUrbControlDescRequest};
use ironrdp_rdpeusb::pdu::usb_dev::{RegisterRequestCallback, TransferInRequest};
use ironrdp_rdpeusb::pdu::utils::RequestIdTransferInOut;
use ironrdp_rdpeusb::pdu::{UrbdrcClientPdu, UrbdrcServerPdu};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::{ServerEvent, ServerEventSender};

/// USB standard `bDescriptorType` value for a device descriptor.
const USB_DESCRIPTOR_TYPE_DEVICE: u8 = 1;
/// A USB device descriptor is 18 bytes.
const USB_DEVICE_DESCRIPTOR_LEN: u32 = 18;

/// The MS-RDPEUSB dynamic virtual channel name (this rev's `ironrdp-rdpeusb`
/// predates the upstream `CHANNEL_NAME` const, so it's spelled out here).
pub const URBDRC_CHANNEL_NAME: &str = "URBDRC";

/// Per-connection ceiling on server-opened per-device `URBDRC` DVCs. Each client
/// `ADD_VIRTUAL_CHANNEL` opens one channel (never pruned within a connection), so
/// this bounds a hostile/buggy client that spams announcements from growing the
/// DRDYNVC slab without limit. Far above any real device count.

/// Server-loop actions requested by the `URBDRC` processor / [`UsbHandle`] that
/// they can't perform themselves (they need `&mut DrdynvcServer`, which only the
/// event loop holds). Delivered via [`ServerEvent::Urbdrc`].
pub enum UrbdrcServerMessage {
    /// The client announced a device (`ADD_VIRTUAL_CHANNEL`) and wants a fresh
    /// per-device `URBDRC` DVC. The loop opens one with a [`UrbdrcDeviceProcessor`].
    OpenDeviceChannel,
    /// Ship DVC messages (a transfer request originated by a [`UsbHandle`]) on an
    /// already-open per-device channel. The loop DVC-frames them and writes them.
    SendMessages { channel_id: u32, messages: Vec<DvcMessage> },
}

impl fmt::Debug for UrbdrcServerMessage {
    // Manual: `DvcMessage` (a boxed encoder) isn't `Debug`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenDeviceChannel => write!(f, "OpenDeviceChannel"),
            Self::SendMessages { channel_id, messages } => f
                .debug_struct("SendMessages")
                .field("channel_id", channel_id)
                .field("messages", &messages.len())
                .finish(),
        }
    }
}

/// Wrap a server→client `UrbdrcServerPdu` as a [`DvcMessage`]. The PDU implements
/// `Encode` but not `DvcEncode` (orphan rules), so we hold it and delegate —
/// the DVC layer adds only its own framing around the encoded bytes. Mirrors
/// `OwnedAudioPdu` in `multitransport/audio_dvc.rs`.
struct UsbDvcPdu(UrbdrcServerPdu);

impl Encode for UsbDvcPdu {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        self.0.encode(dst)
    }

    fn name(&self) -> &'static str {
        "UrbdrcServerPdu"
    }

    fn size(&self) -> usize {
        self.0.size()
    }
}

impl DvcEncode for UsbDvcPdu {}

fn dvc_msg(pdu: UrbdrcServerPdu) -> DvcMessage {
    Box::new(UsbDvcPdu(pdu))
}

/// A bare [`SharedMsgHeader`] message with no body — used for `RIMCALL_RELEASE`,
/// which the pinned `ironrdp-rdpeusb` has no dedicated server PDU for (it's a
/// generic RPCE "release interface" call: just the shared header). The header
/// itself implements `Encode`.
struct UsbHeaderMsg(SharedMsgHeader);

impl Encode for UsbHeaderMsg {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        self.0.encode(dst)
    }

    fn name(&self) -> &'static str {
        "SharedMsgHeader"
    }

    fn size(&self) -> usize {
        self.0.size()
    }
}

impl DvcEncode for UsbHeaderMsg {}

/// Build a `RIMCALL_RELEASE` message (channel-notification interface, no body).
/// FreeRDP's `urbdrc_device_control_channel` uses it as a ready barrier: on the
/// main channel it triggers `ADD_VIRTUAL_CHANNEL`; on a per-device channel it
/// triggers `ADD_DEVICE`.
fn rimcall_release(msg_id: u32) -> DvcMessage {
    Box::new(UsbHeaderMsg(SharedMsgHeader {
        interface_id: InterfaceId::NOTIFY_CLIENT,
        mask: Mask::StreamIdProxy,
        msg_id,
        function_id: Some(FunctionId::RIMCALL_RELEASE),
    }))
}

/// Best-effort identify a client PDU by decoding just its shared header (used to
/// log meaningfully when the full body decode fails). Uses the pinned header
/// decoder only — no parallel wire parsing.
fn peek_function_id(payload: &[u8]) -> Option<FunctionId> {
    decode::<SharedMsgHeader>(payload).ok().and_then(|h| h.function_id)
}

/// The fields of a standard 18-byte USB device descriptor we surface. Keeps the
/// descriptor byte-layout knowledge in one typed place instead of inline offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub usb_version: u16, // bcdUSB
    pub device_class: u8,
    pub vendor_id: u16,  // idVendor
    pub product_id: u16, // idProduct
    pub device_release: u16, // bcdDevice
}

impl DeviceDescriptor {
    /// Parse a device descriptor (USB spec 9.6.1). Returns `None` if `buf` is too
    /// short or isn't a device descriptor.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        // bLength @0, bDescriptorType @1, then the fixed 18-byte layout.
        if buf.len() < 18 || buf[1] != USB_DESCRIPTOR_TYPE_DEVICE {
            return None;
        }
        let u16le = |i: usize| u16::from_le_bytes([buf[i], buf[i + 1]]);
        Some(Self {
            usb_version: u16le(2),
            device_class: buf[4],
            vendor_id: u16le(8),
            product_id: u16le(10),
            device_release: u16le(12),
        })
    }
}

/// Correlates outstanding transfer requests with their `URB_COMPLETION`s by the
/// request id echoed in the completion. Mirrors `rdpdr::IoRouter`. Cheaply
/// clonable (shared inner); the request id doubles as the TS_URB `RequestId`
/// (31-bit — the counter is masked to fit).
#[derive(Clone, Default)]
pub struct UsbRouter {
    inner: Arc<UsbRouterInner>,
}

#[derive(Default)]
struct UsbRouterInner {
    next_id: AtomicU32,
    pending: Mutex<HashMap<u32, oneshot::Sender<Vec<u8>>>>,
}

impl UsbRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a 31-bit request id + a receiver for its completion body.
    fn register(&self) -> (u32, oneshot::Receiver<Vec<u8>>) {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) & 0x7FFF_FFFF;
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    /// Deliver a completion body to the matching waiter (dropped if none).
    fn deliver(&self, req_id: u32, body: Vec<u8>) {
        if let Some(tx) = self.inner.pending.lock().unwrap().remove(&req_id) {
            let _ = tx.send(body);
        } else {
            debug!(req_id, "URBDRC: URB completion with no waiter (dropped)");
        }
    }
}

/// Async handle onto a single redirected USB device's per-device DVC. Clone it to
/// drive transfers from anywhere (the future macOS UserHCI side); each method
/// registers a completion waiter, ships the request through the server event loop
/// (`ServerEvent::Urbdrc(SendMessages)`) onto the device channel, and awaits the
/// matching `URB_COMPLETION`. Mirrors `rdpdr::RdpdrHandle`.
#[derive(Clone)]
pub struct UsbHandle {
    sender: mpsc::UnboundedSender<ServerEvent>,
    router: UsbRouter,
    /// The device's per-device DVC channel id (transfers ride this channel).
    channel_id: u32,
    /// The device's interface id (addresses the device in each request header).
    device_iface: InterfaceId,
}

impl UsbHandle {
    fn new(sender: mpsc::UnboundedSender<ServerEvent>, router: UsbRouter, channel_id: u32, device_iface: InterfaceId) -> Self {
        Self {
            sender,
            router,
            channel_id,
            device_iface,
        }
    }

    /// Fetch a descriptor via a `GET_DESCRIPTOR` control transfer (IN) and return
    /// its raw bytes. `desc_type`/`index`/`lang_id` are the standard
    /// `SETUP.wValue`/`wIndex`; `max_len` bounds the reply.
    pub async fn get_descriptor(&self, desc_type: u8, index: u8, lang_id: u16, max_len: u32) -> Result<Vec<u8>> {
        let (req_id, rx) = self.router.register();
        let messages = get_descriptor_request(self.device_iface, req_id, desc_type, index, lang_id, max_len);
        self.sender
            .send(ServerEvent::Urbdrc(UrbdrcServerMessage::SendMessages {
                channel_id: self.channel_id,
                messages,
            }))
            .ok()
            .context("URBDRC: server event loop gone")?;
        rx.await.context("URBDRC: connection closed before completion")
    }

    /// Convenience: fetch the 18-byte device descriptor and parse it.
    pub async fn device_descriptor(&self) -> Result<DeviceDescriptor> {
        let bytes = self
            .get_descriptor(USB_DESCRIPTOR_TYPE_DEVICE, 0, 0, USB_DEVICE_DESCRIPTOR_LEN)
            .await?;
        DeviceDescriptor::parse(&bytes).context("URBDRC: malformed device descriptor")
    }
}

/// Build a `GET_DESCRIPTOR` control-transfer request (IN): a
/// `RegisterRequestCallback` (naming the completion interface) followed by a
/// `TransferInRequest` whose TS_URB `RequestId` is `req_id` (echoed in the
/// completion, so the [`UsbRouter`] can correlate it).
fn get_descriptor_request(
    device_iface: InterfaceId,
    req_id: u32,
    desc_type: u8,
    index: u8,
    lang_id: u16,
    max_len: u32,
) -> Vec<DvcMessage> {
    // Any unique id works for the completion interface; the completion decode is
    // interface-agnostic, so reuse the device's own interface id.
    let reg = RegisterRequestCallback {
        msg_id: 0,
        udev_iface: device_iface,
        request_completion: Some(device_iface),
    };
    let ts_req_id = RequestIdTransferInOut::try_from(req_id).expect("router ids are masked to 31 bits");
    let get_desc = TransferInRequest {
        msg_id: 0,
        udev_iface: device_iface,
        ts_urb: TsUrb::CtlDescReq(TsUrbControlDescRequest {
            header: TsUrbHeader {
                func: UrbFunction::GetDescriptorFromDevice,
                req_id: ts_req_id,
                no_ack: false,
            },
            index,
            desc_type,
            lang_id,
        }),
        output_buffer_size: max_len,
    };
    vec![
        dvc_msg(UrbdrcServerPdu::RegReqCb(reg)),
        dvc_msg(UrbdrcServerPdu::TransferIn(get_desc)),
    ]
}

const MAX_DEVICE_CHANNELS: u32 = 32;

/// Main server-side `URBDRC` DVC processor: drives the init handshake and, on each
/// device announcement, asks the event loop to open a per-device channel.
pub struct UrbdrcServer {
    /// Monotonic message id for the top-level request/response pairs we originate.
    next_msg_id: u32,
    /// Event-loop channel, used to request per-device DVC creation. `None` leaves
    /// the processor observe-only (the handshake still runs; no device channels).
    sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    /// Per-connection count of device channels we've asked the loop to open, so a
    /// client that spams `ADD_VIRTUAL_CHANNEL` can't grow the DVC slab unbounded.
    device_channels_opened: u32,
}

impl UrbdrcServer {
    pub fn new() -> Self {
        Self {
            next_msg_id: 1,
            sender: None,
            device_channels_opened: 0,
        }
    }

    /// Build a processor wired to the connection's server-event sender so it can
    /// request per-device channel creation.
    pub fn with_sender(sender: Option<mpsc::UnboundedSender<ServerEvent>>) -> Self {
        Self {
            next_msg_id: 1,
            sender,
            device_channels_opened: 0,
        }
    }

    fn take_msg_id(&mut self) -> u32 {
        let id = self.next_msg_id;
        self.next_msg_id = self.next_msg_id.wrapping_add(1);
        id
    }
}

impl Default for UrbdrcServer {
    fn default() -> Self {
        Self::new()
    }
}

impl_as_any!(UrbdrcServer);

impl DvcProcessor for UrbdrcServer {
    fn channel_name(&self) -> &str {
        URBDRC_CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        // Kick off the capability exchange (MS-RDPEUSB §3.3.5.1): the server sends
        // RIM_EXCHANGE_CAPABILITY_REQUEST first; the client replies, then (after the
        // CHANNEL_CREATED + RIMCALL_RELEASE barrier) announces its devices.
        info!(channel_id, "URBDRC DVC opened — sending capability request");
        let req = RimExchangeCapabilityRequest {
            msg_id: self.take_msg_id(),
            capability: Capability::RimCapabilityVersion01,
        };
        Ok(vec![dvc_msg(UrbdrcServerPdu::Caps(req))])
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // Never tear down the session on a URBDRC decode error (opt-in feature).
        let pdu = match decode::<UrbdrcClientPdu>(payload) {
            Ok(pdu) => pdu,
            Err(e) => {
                warn!(channel_id, error = %e, "URBDRC main-channel PDU decode failed (tolerated)");
                return Ok(Vec::new());
            }
        };
        match pdu {
            UrbdrcClientPdu::Caps(resp) => {
                info!(
                    channel_id,
                    result = format_args!("{:#010x}", resp.result),
                    "URBDRC capability response received — client accepted the exchange"
                );
                // MS-RDPEUSB 3.3.5.1: after the capability exchange the server sends
                // CHANNEL_CREATED. This is the message that makes the client ANNOUNCE
                // its redirected devices (ADD_DEVICE / ADD_VIRTUAL_CHANNEL) — without
                // it the client registers the device locally but never tells us.
                info!(channel_id, "URBDRC sending CHANNEL_CREATED (triggers device announcement)");
                let created = ChannelCreated {
                    msg_id: self.take_msg_id(),
                    direction: Direction::ToClient,
                };
                return Ok(vec![dvc_msg(UrbdrcServerPdu::ChanCreated(created))]);
            }
            UrbdrcClientPdu::ChanCreated(cc) => {
                debug!(channel_id, direction = ?cc.direction, "URBDRC CHANNEL_CREATED");
                // With the channel-created handshake done, send RIMCALL_RELEASE — the
                // ready barrier (FreeRDP: urbdrc_device_control_channel, INIT_CHANNEL_IN)
                // that makes the client announce its devices via ADD_VIRTUAL_CHANNEL.
                info!(channel_id, "URBDRC sending RIMCALL_RELEASE (device-announce barrier)");
                return Ok(vec![rimcall_release(self.take_msg_id())]);
            }
            UrbdrcClientPdu::AddChan(add) => {
                // The client announced a device and wants a per-device channel. We
                // can't open a DVC from here (no DrdynvcServer handle), so ask the
                // event loop to (Phase 3.1a). ADD_DEVICE with the descriptors follows
                // on that new channel.
                info!(
                    channel_id,
                    msg_id = add.msg_id,
                    "URBDRC ADD_VIRTUAL_CHANNEL — requesting a per-device channel"
                );
                if self.device_channels_opened >= MAX_DEVICE_CHANNELS {
                    warn!(
                        channel_id,
                        opened = self.device_channels_opened,
                        "URBDRC: per-connection device-channel cap reached, ignoring ADD_VIRTUAL_CHANNEL"
                    );
                } else if let Some(sender) = self.sender.clone() {
                    if sender.send(ServerEvent::Urbdrc(UrbdrcServerMessage::OpenDeviceChannel)).is_err() {
                        warn!(channel_id, "URBDRC: server event loop gone, cannot open device channel");
                    } else {
                        self.device_channels_opened = self.device_channels_opened.saturating_add(1);
                    }
                } else {
                    debug!(channel_id, "URBDRC observe-only (no sender) — not opening a device channel");
                }
            }
            UrbdrcClientPdu::AddDev(dev) => {
                // ADD_DEVICE normally arrives on the per-device channel
                // (UrbdrcDeviceProcessor); handle it here too for robustness.
                info!(
                    channel_id,
                    usb_device = %dev.usb_device,
                    device_instance_id = %dev.device_instance_id,
                    usb_version = ?dev.usb_device_caps.supported_usb_ver,
                    speed = ?dev.usb_device_caps.device_speed,
                    "URBDRC AddDevice on the main channel — client announced a redirected USB device (GO)"
                );
            }
            _ => {
                debug!(channel_id, "URBDRC client PDU (unhandled)");
            }
        }
        Ok(Vec::new())
    }
}

impl DvcServerProcessor for UrbdrcServer {}

/// Per-device `URBDRC` DVC processor. Opened by the event loop in response to
/// `ADD_VIRTUAL_CHANNEL`; on open it sends `RIMCALL_RELEASE` (the
/// `INIT_CHANNEL_OUT` barrier) so the client sends `ADD_DEVICE`.
///
/// Its `process()` is intentionally thin — decode and route: it logs
/// `ADD_DEVICE`, hands `URB_COMPLETION`s to the [`UsbRouter`], and tolerates
/// anything else. Transfers themselves are issued through a [`UsbHandle`] (the
/// async seam the macOS UserHCI side drives). As a Phase 3.1b(2) spike — a
/// stand-in for that driver — it fetches the real device descriptor once on
/// `ADD_DEVICE` via that same handle, proving the transfer round-trip end-to-end.
pub struct UrbdrcDeviceProcessor {
    /// Connection event sender, used to build [`UsbHandle`]s that ship transfers.
    sender: mpsc::UnboundedSender<ServerEvent>,
    /// Shared with every [`UsbHandle`] we build — completions route back through it.
    router: UsbRouter,
    /// One-shot guard so the descriptor probe fires at most once per device.
    descriptor_probed: bool,
}

impl UrbdrcDeviceProcessor {
    pub fn new(sender: mpsc::UnboundedSender<ServerEvent>) -> Self {
        Self {
            sender,
            router: UsbRouter::new(),
            descriptor_probed: false,
        }
    }

    /// Spike driver (stand-in for the future UserHCI side): fetch + log the real
    /// device descriptor through the async [`UsbHandle`], off the event-loop thread.
    fn spawn_descriptor_probe(&self, channel_id: u32, device_iface: InterfaceId) {
        let handle = UsbHandle::new(self.sender.clone(), self.router.clone(), channel_id, device_iface);
        info!(channel_id, "URBDRC issuing GET_DESCRIPTOR probe (device descriptor)");
        tokio::spawn(async move {
            match handle.device_descriptor().await {
                Ok(d) => info!(
                    channel_id,
                    vid = format_args!("{:#06x}", d.vendor_id),
                    pid = format_args!("{:#06x}", d.product_id),
                    usb_version = format_args!("{:#06x}", d.usb_version),
                    device_class = d.device_class,
                    "URBDRC device descriptor — real device data received (transfer round-trip GO)"
                ),
                Err(e) => warn!(channel_id, error = %e, "URBDRC device-descriptor probe failed"),
            }
        });
    }
}

impl_as_any!(UrbdrcDeviceProcessor);

impl DvcProcessor for UrbdrcDeviceProcessor {
    fn channel_name(&self) -> &str {
        URBDRC_CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        // The per-device channel is up. Send RIMCALL_RELEASE so the client's
        // INIT_CHANNEL_OUT path fires and it sends us ADD_DEVICE (the descriptors).
        info!(channel_id, "URBDRC device channel opened — sending RIMCALL_RELEASE for ADD_DEVICE");
        Ok(vec![rimcall_release(1)])
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // A decode error on this opt-in, non-critical channel must NEVER tear down
        // the RDP session (same lesson as the ironrdp-dvc Soft-Sync divergence).
        // Tolerate it, and still recognize ADD_DEVICE from its header.
        match decode::<UrbdrcClientPdu>(payload) {
            Ok(UrbdrcClientPdu::AddDev(dev)) => {
                info!(
                    channel_id,
                    usb_device = %dev.usb_device,
                    device_instance_id = %dev.device_instance_id,
                    usb_version = ?dev.usb_device_caps.supported_usb_ver,
                    speed = ?dev.usb_device_caps.device_speed,
                    "URBDRC ADD_DEVICE — real device descriptors received (GO)"
                );
                if !self.descriptor_probed {
                    self.descriptor_probed = true;
                    self.spawn_descriptor_probe(channel_id, dev.usb_device);
                }
            }
            Ok(UrbdrcClientPdu::UrbComp(comp)) => {
                self.router.deliver(comp.req_id.into(), comp.output_buffer);
            }
            Ok(UrbdrcClientPdu::UrbCompNoData(comp)) => {
                // No payload; wake the waiter with an empty body so it doesn't hang.
                debug!(
                    channel_id,
                    hresult = format_args!("{:#010x}", comp.hresult),
                    "URBDRC URB_COMPLETION_NO_DATA"
                );
                self.router.deliver(comp.req_id.into(), Vec::new());
            }
            Ok(_) => {
                debug!(channel_id, "URBDRC device-channel PDU (unhandled)");
            }
            Err(e) if peek_function_id(payload) == Some(FunctionId::ADD_DEVICE) => {
                // The device IS being announced — decode only stumbled on the body
                // (e.g. an even newer caps value the vendored decoder doesn't name).
                // The forward works; log it as a GO from the header.
                warn!(
                    channel_id,
                    error = %e,
                    "URBDRC ADD_DEVICE received — real device announced (GO); caps body not fully parsed"
                );
            }
            Err(e) => {
                warn!(channel_id, error = %e, "URBDRC device-channel PDU decode failed (tolerated)");
            }
        }
        Ok(Vec::new())
    }
}

impl DvcServerProcessor for UrbdrcDeviceProcessor {}

/// Factory installed on [`RdpServer`](crate::RdpServer) to enable server-direction
/// USB redirection. Mirrors the other channel factories; ships inert (the server
/// only advertises `URBDRC` when the factory is `Some`). `ServerEventSender` is a
/// supertrait so the factory captures the connection's event sender and hands it
/// to each built processor (for per-device channel requests).
pub trait UrbdrcServerFactory: ServerEventSender + Send {
    /// Build the per-connection main `URBDRC` DVC processor.
    fn build_processor(&self) -> UrbdrcServer;
}
