//! Server-direction MS-RDPECAM (`RDCamera_Device_Enumerator` DVC) — **Phase 0
//! protocol gate only**.
//!
//! macrdp is the RDP **server**: the RDP client owns a physical webcam and, when
//! the user enables "Video capture devices" in the client's local-resources, the
//! client redirects it over the MS-RDPECAM *Video Capture Virtual Channel
//! Extension*. The redirection begins on a single enumeration channel,
//! `RDCamera_Device_Enumerator`; the client then announces each camera on that
//! channel (`DEVICE_ADDED_NOTIFICATION`) and the server opens a per-device DVC to
//! drive it. See `docs/rdp-camera-redirection-feasibility.md`.
//!
//! **This module is the cheap go/no-go gate (Phase 0), not the feature.** It
//! advertises the enumeration channel, answers the client's version negotiation,
//! and *logs* the client's `DEVICE_ADDED_NOTIFICATION` — that log line is the
//! GREEN signal that a modern mstsc/Win11 will hand macrdp its redirected camera
//! over MS-RDPECAM (proving the target before the multi-week presentation work:
//! per-device channels, stream/media-type negotiation, H.264 sample decode, and a
//! macOS CoreMediaIO Camera Extension). It opens **no** per-device channel, drives
//! **no** stream, and touches **no** macOS code. Gated behind macrdp's
//! `--enable-camera-redirection`; when the factory isn't installed the channel is
//! never advertised and the build is byte-identical.
//!
//! **Handshake (MS-RDPECAM 3.1/3.2).** Every message begins with a 2-byte
//! `SHARED_MSG_HEADER` = `Version(u8)` + `MessageId(u8)`. Once the server opens the
//! enumeration DVC, the **client speaks first** with a `SelectVersionRequest`
//! (`0x03`) carrying its maximum supported version; the server replies
//! `SelectVersionResponse` (`0x04`) with the negotiated (lower) version; the client
//! then sends one `DeviceAddedNotification` (`0x05`) per redirected camera. So the
//! processor's `start()` is empty — there is nothing to send until the client's
//! request arrives — and everything happens in `process()`.

use ironrdp_core::{Encode, EncodeResult, WriteCursor, impl_as_any};
use ironrdp_dvc::{DvcEncode, DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::PduResult;
use tracing::{debug, info, warn};

/// The MS-RDPECAM enumeration dynamic virtual channel name (MS-RDPECAM 1.5).
pub const RDCAMERA_CHANNEL_NAME: &str = "RDCamera_Device_Enumerator";

/// MS-RDPECAM `MessageId` values (the second byte of `SHARED_MSG_HEADER`); only the
/// few Phase-0 handles are named.
mod msg_id {
    /// `SELECT_VERSION_REQUEST` — client → server, opens version negotiation.
    pub const SELECT_VERSION_REQUEST: u8 = 0x03;
    /// `SELECT_VERSION_RESPONSE` — server → client, negotiated version.
    pub const SELECT_VERSION_RESPONSE: u8 = 0x04;
    /// `DEVICE_ADDED_NOTIFICATION` — client → server, one per redirected camera.
    pub const DEVICE_ADDED_NOTIFICATION: u8 = 0x05;
    /// `DEVICE_REMOVED_NOTIFICATION` — client → server.
    pub const DEVICE_REMOVED_NOTIFICATION: u8 = 0x06;
}

/// Highest MS-RDPECAM protocol version this gate advertises. The spec defines
/// version 1 (Win10 1903) and version 2 (adds device properties); Phase 0 only
/// needs the negotiation + the added-notification, so either is fine — we accept
/// the client's max down to 1.
const OUR_MAX_VERSION: u8 = 2;

/// Server → client `SELECT_VERSION_RESPONSE`: a bare 2-byte `SHARED_MSG_HEADER`
/// (`Version` + `MessageId`). Wrapped as a [`DvcEncode`] so the DVC layer frames
/// it (mirrors `UsbHeaderMsg` in `rdpeusb.rs`).
struct SelectVersionResponse {
    version: u8,
}

impl Encode for SelectVersionResponse {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ironrdp_core::ensure_size!(in: dst, size: self.size());
        dst.write_u8(self.version);
        dst.write_u8(msg_id::SELECT_VERSION_RESPONSE);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "RDCAMERA_SELECT_VERSION_RESPONSE"
    }

    fn size(&self) -> usize {
        2
    }
}

impl DvcEncode for SelectVersionResponse {}

/// Phase-0 MS-RDPECAM enumeration-channel processor: answers version negotiation
/// and logs the client's camera announcements. Opens no per-device channel.
pub struct RdCameraServer {}

impl RdCameraServer {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for RdCameraServer {
    fn default() -> Self {
        Self::new()
    }
}

impl_as_any!(RdCameraServer);

impl DvcProcessor for RdCameraServer {
    fn channel_name(&self) -> &str {
        RDCAMERA_CHANNEL_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        // The client speaks first (SelectVersionRequest); nothing to send on open.
        info!("MS-RDPECAM enumeration channel opened — waiting for the client's SelectVersionRequest");
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // TOLERANT by construction (Phase 0): every path returns Ok, never
        // propagates — a decode error here would otherwise tear down the whole
        // RDP session for an opt-in gate (same lesson as the URBDRC processor).
        if payload.len() < 2 {
            warn!(len = payload.len(), "MS-RDPECAM message too short for SHARED_MSG_HEADER — ignoring");
            return Ok(Vec::new());
        }
        let version = payload[0];
        let message_id = payload[1];
        let body = &payload[2..];

        match message_id {
            msg_id::SELECT_VERSION_REQUEST => {
                // The client's max version is the header Version field. Negotiate
                // down to what we support (never below 1).
                let selected = version.min(OUR_MAX_VERSION).max(1);
                info!(
                    client_version = version,
                    selected_version = selected,
                    "MS-RDPECAM SelectVersionRequest — replying SelectVersionResponse"
                );
                Ok(vec![Box::new(SelectVersionResponse { version: selected })])
            }
            msg_id::DEVICE_ADDED_NOTIFICATION => {
                // THE PHASE-0 GREEN SIGNAL. Layout after the 2-byte header:
                //   DeviceName          — null-terminated UTF-16LE
                //   VirtualChannelName  — null-terminated ASCII
                // Reads are bounded to `body` (CVE-2026-57157 was an OOB scan for
                // these terminators in FreeRDP before 3.28.0).
                let (device_name, rest) = read_utf16_z(body);
                let channel_name = read_ascii_z(rest);
                info!(
                    version,
                    device_name = %device_name,
                    virtual_channel = %channel_name,
                    "MS-RDPECAM DEVICE_ADDED_NOTIFICATION — the client redirected a camera (Phase-0 GREEN)"
                );
                Ok(Vec::new())
            }
            msg_id::DEVICE_REMOVED_NOTIFICATION => {
                let channel_name = read_ascii_z(body);
                info!(version, virtual_channel = %channel_name, "MS-RDPECAM DEVICE_REMOVED_NOTIFICATION");
                Ok(Vec::new())
            }
            other => {
                debug!(
                    version,
                    message_id = format_args!("0x{other:02x}"),
                    len = body.len(),
                    "MS-RDPECAM message not handled at Phase 0 — ignoring"
                );
                Ok(Vec::new())
            }
        }
    }
}

impl DvcServerProcessor for RdCameraServer {}

/// Read a null-terminated UTF-16LE string from the front of `buf`, bounded to
/// `buf`. Returns the decoded (lossy) string and the remaining bytes AFTER the
/// two-byte null terminator (or the empty slice if no terminator was found).
fn read_utf16_z(buf: &[u8]) -> (String, &[u8]) {
    let mut units = Vec::new();
    let mut i = 0;
    while i + 1 < buf.len() {
        let u = u16::from_le_bytes([buf[i], buf[i + 1]]);
        i += 2;
        if u == 0 {
            return (String::from_utf16_lossy(&units), &buf[i..]);
        }
        units.push(u);
    }
    // No terminator within bounds: decode what we have, no remainder.
    (String::from_utf16_lossy(&units), &[])
}

/// Read a null-terminated ASCII string from the front of `buf`, bounded to `buf`.
fn read_ascii_z(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Factory for the Phase-0 MS-RDPECAM enumeration processor. Unlike the URBDRC
/// factory this needs **no** `ServerEventSender` — the gate replies inline from
/// `process()` and never asks the event loop to open a channel.
pub trait RdCameraServerFactory: Send {
    fn build_processor(&self) -> RdCameraServer;
}
