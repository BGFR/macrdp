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
//! **Phase 3.0 (this file): observe-only go/no-go.** On channel open we send the
//! capability-exchange request; on inbound we decode + log every client PDU
//! (capability response, device announcements). This answers the one open
//! question cheaply — does a reachable RDP client actually open `URBDRC` against
//! a generic server and announce a device? — before the full transfer-forwarding
//! machinery (a `UsbHandle`/router + the async IOUSBHost bridge) is built in 3.1.

use ironrdp_core::{Encode, EncodeResult, WriteCursor, decode, impl_as_any};
use ironrdp_dvc::{DvcEncode, DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::{PduResult, decode_err};
use ironrdp_rdpeusb::pdu::caps::{Capability, RimExchangeCapabilityRequest};
use ironrdp_rdpeusb::pdu::{UrbdrcClientPdu, UrbdrcServerPdu};
use tracing::{debug, info};

use crate::ServerEventSender;

/// The MS-RDPEUSB dynamic virtual channel name (this rev's `ironrdp-rdpeusb`
/// predates the upstream `CHANNEL_NAME` const, so it's spelled out here).
pub const URBDRC_CHANNEL_NAME: &str = "URBDRC";

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

/// Server-side `URBDRC` DVC processor.
///
/// Phase 3.0 is observe-only: it starts the capability exchange and logs what the
/// client sends back. Phase 3.1 grows this into the real handshake state machine
/// (device-sink `AddDevice` → per-device transfers) driven by a `UsbHandle`.
pub struct UrbdrcServer {
    /// Monotonic message id for the top-level request/response pairs we originate.
    next_msg_id: u32,
}

impl UrbdrcServer {
    pub fn new() -> Self {
        Self { next_msg_id: 1 }
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
        // RIM_EXCHANGE_CAPABILITY_REQUEST first; the client replies, then announces
        // its devices on the Device Sink interface.
        info!(channel_id, "URBDRC DVC opened — sending capability request (go/no-go spike)");
        let req = RimExchangeCapabilityRequest {
            msg_id: self.take_msg_id(),
            capability: Capability::RimCapabilityVersion01,
        };
        Ok(vec![dvc_msg(UrbdrcServerPdu::Caps(req))])
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        match decode::<UrbdrcClientPdu>(payload).map_err(|e| decode_err!(e))? {
            UrbdrcClientPdu::Caps(resp) => {
                info!(
                    channel_id,
                    result = format_args!("{:#010x}", resp.result),
                    "URBDRC capability response received — client accepted the exchange"
                );
            }
            UrbdrcClientPdu::AddChan(add) => {
                debug!(channel_id, msg_id = add.msg_id, "URBDRC ADD_VIRTUAL_CHANNEL");
            }
            UrbdrcClientPdu::AddDev(dev) => {
                info!(
                    channel_id,
                    usb_device = %dev.usb_device,
                    device_instance_id = %dev.device_instance_id,
                    speed = ?dev.usb_device_caps.device_speed,
                    "URBDRC AddDevice — client announced a redirected USB device (GO)"
                );
            }
            UrbdrcClientPdu::ChanCreated(cc) => {
                debug!(channel_id, direction = ?cc.direction, "URBDRC CHANNEL_CREATED");
            }
            _ => {
                debug!(channel_id, "URBDRC client PDU (unhandled in the observe-only spike)");
            }
        }
        Ok(Vec::new())
    }
}

impl DvcServerProcessor for UrbdrcServer {}

/// Factory installed on [`RdpServer`](crate::RdpServer) to enable server-direction
/// USB redirection. Mirrors the other channel factories; ships inert (the server
/// only advertises `URBDRC` when the factory is `Some`). `ServerEventSender` is a
/// supertrait so Phase 3.1 can wire the outbound `UsbHandle` sender here.
pub trait UrbdrcServerFactory: ServerEventSender + Send {
    /// Build the per-connection `URBDRC` DVC processor.
    fn build_processor(&self) -> UrbdrcServer;
}
