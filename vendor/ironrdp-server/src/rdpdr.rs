//! Server-side RDPDR (drive redirection) static channel — [MS-RDPEFS].
//!
//! This is the server-direction peer to `ironrdp-rdpdr`'s client `Rdpdr`
//! (`SvcClientProcessor`): it drives the initialization handshake the client
//! expects (Server Announce → capability exchange → Client ID Confirm → User
//! Logged On) and surfaces the client's announced devices to a
//! [`RdpdrServerHandler`] backend. Phase 1a logs the announced drives; device
//! I/O (file read) is layered on later via the same backend.
//!
//! It lives here, alongside the other server-side channel processors/bridges
//! (`echo.rs`, `gfx.rs`, `sound.rs`), and reuses `ironrdp-rdpdr` purely for the
//! MS-RDPEFS wire types. Upstreamable as a `SvcServerProcessor` peer to the
//! client `Rdpdr`.
//!
//! [MS-RDPEFS]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpefs/34d9de58-b2b5-40b6-b970-f82d4603bdb5

use core::fmt;

use ironrdp_core::{impl_as_any, ReadCursor};
use ironrdp_pdu::gcc::ChannelName;
use ironrdp_pdu::{decode_err, PduResult};
use ironrdp_rdpdr::pdu::efs::{
    Capabilities, ClientDeviceListAnnounce, ClientNameRequest, CoreCapability, CoreCapabilityKind, DeviceType,
    NtStatus, ServerDeviceAnnounceResponse, VersionAndIdPdu, VersionAndIdPduKind, VERSION_MAJOR, VERSION_MINOR_RDP51,
};
use ironrdp_rdpdr::pdu::{PacketId, RdpdrPdu, SharedHeader};
use ironrdp_svc::{CompressionCondition, SvcMessage, SvcProcessor, SvcServerProcessor};
use tracing::{debug, info, warn};

use crate::ServerEventSender;

/// A device the client announced as redirected.
#[derive(Debug, Clone)]
pub struct AnnouncedDevice {
    pub device_id: u32,
    pub device_type: DeviceType,
    /// The 8-char PreferredDosName the client sent (e.g. a drive/share label).
    pub name: String,
}

/// Backend hooks for the macrdp side of RDPDR. Phase 1a only needs to learn
/// which devices the client announced; device-I/O routing is layered on later.
pub trait RdpdrServerHandler: Send + fmt::Debug {
    /// Called when the client announces (or re-announces) its device list.
    fn on_devices_announced(&mut self, devices: &[AnnouncedDevice]);
}

/// Builds the macrdp-side backend + supplies connection config.
pub trait RdpdrBackendFactory {
    fn build_backend(&self) -> Box<dyn RdpdrServerHandler>;
    /// Computer name shown to the client (Explorer renders shares as
    /// "`<directory>` on `<computer_name>`").
    fn computer_name(&self) -> String;
}

/// Factory for the RDPDR static channel, mirroring [`SoundServerFactory`] /
/// [`CliprdrServerFactory`]. `ServerEventSender` lets later phases push
/// server-initiated device-I/O requests onto the connection's event loop.
///
/// [`SoundServerFactory`]: crate::SoundServerFactory
/// [`CliprdrServerFactory`]: crate::CliprdrServerFactory
pub trait RdpdrServerFactory: RdpdrBackendFactory + ServerEventSender {}

/// Server-side RDPDR channel processor.
pub struct RdpdrServer {
    backend: Box<dyn RdpdrServerHandler>,
    /// Shown to the client; consumed by the `Debug` impl and later phases.
    computer_name: String,
    /// Server-chosen client id, echoed through the announce handshake.
    client_id: u32,
}

impl fmt::Debug for RdpdrServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdpdrServer")
            .field("computer_name", &self.computer_name)
            .finish()
    }
}

impl_as_any!(RdpdrServer);

impl RdpdrServer {
    pub fn new(backend: Box<dyn RdpdrServerHandler>, computer_name: String) -> Self {
        Self {
            backend,
            computer_name,
            client_id: 0x0000_0001,
        }
    }

    /// The capabilities the server advertises. The client keeps only the
    /// capabilities the server also advertises (`clone_supported_by`), so the
    /// Drive capability must be present for drive redirection to survive.
    fn server_capabilities() -> CoreCapability {
        let mut caps = Capabilities::new(); // GENERAL
        caps.add_drive();
        CoreCapability {
            capabilities: caps.clone_inner(),
            kind: CoreCapabilityKind::ServerCoreCapabilityRequest,
        }
    }

    fn version_and_id(&self, kind: VersionAndIdPduKind) -> RdpdrPdu {
        RdpdrPdu::VersionAndIdPdu(VersionAndIdPdu {
            version_major: VERSION_MAJOR,
            // RDP5.1 minor → the client announces ALL devices on Client-ID
            // Confirm rather than waiting for post-logon.
            version_minor: VERSION_MINOR_RDP51,
            client_id: self.client_id,
            kind,
        })
    }
}

impl SvcProcessor for RdpdrServer {
    fn channel_name(&self) -> ChannelName {
        ChannelName::from_static(b"rdpdr\0\0\0")
    }

    fn compression_condition(&self) -> CompressionCondition {
        CompressionCondition::WhenRdpDataIsCompressed
    }

    fn start(&mut self) -> PduResult<Vec<SvcMessage>> {
        // Server-initiated: kick off MS-RDPEFS with the Server Announce Request.
        let announce = self.version_and_id(VersionAndIdPduKind::ServerAnnounceRequest);
        debug!("RDPDR: sending Server Announce Request");
        Ok(vec![SvcMessage::from(announce)])
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        let mut src = ReadCursor::new(payload);
        let header = SharedHeader::decode(&mut src).map_err(|e| decode_err!(e))?;

        match header.packet_id {
            PacketId::CoreClientidConfirm => {
                // Client Announce Reply: record the echoed id, then send BOTH the
                // Server Core Capability Request AND the Server Client ID Confirm.
                // Per MS-RDPEFS 3.3.5.1 the server sends both before the client
                // replies — mstsc waits for the Client ID Confirm and won't send
                // its capability response until it arrives (FreeRDP is lenient and
                // replies after just the capability request, which masked this).
                let reply = VersionAndIdPdu::decode(header, &mut src).map_err(|e| decode_err!(e))?;
                debug!(client_id = reply.client_id, "RDPDR: Client Announce Reply");
                self.client_id = reply.client_id;
                Ok(vec![
                    SvcMessage::from(RdpdrPdu::CoreCapability(Self::server_capabilities())),
                    SvcMessage::from(self.version_and_id(VersionAndIdPduKind::ServerClientIdConfirm)),
                ])
            }
            PacketId::CoreClientName => {
                let name = ClientNameRequest::decode(&mut src).map_err(|e| decode_err!(e))?;
                debug!(?name, "RDPDR: Client Name");
                Ok(Vec::new())
            }
            PacketId::CoreClientCapability => {
                let _resp = CoreCapability::decode(header, &mut src).map_err(|e| decode_err!(e))?;
                debug!("RDPDR: Client Capability Response — signaling user logged on");
                // Client ID Confirm was already sent; User Logged On prompts the
                // client to announce its (post-logon) devices.
                Ok(vec![SvcMessage::from(RdpdrPdu::UserLoggedon)])
            }
            PacketId::CoreDevicelistAnnounce => {
                let announce = ClientDeviceListAnnounce::decode(&mut src).map_err(|e| decode_err!(e))?;
                let devices: Vec<AnnouncedDevice> = announce
                    .device_list
                    .iter()
                    .map(|d| AnnouncedDevice {
                        device_id: d.device_id(),
                        device_type: d.device_type(),
                        name: d.preferred_dos_name().to_owned(),
                    })
                    .collect();
                for d in &devices {
                    info!(device_id = d.device_id, device_type = ?d.device_type, name = %d.name, "RDPDR: client announced device");
                }
                self.backend.on_devices_announced(&devices);
                // Acknowledge every announced device (read-only MVP).
                Ok(announce
                    .device_list
                    .iter()
                    .map(|d| {
                        SvcMessage::from(RdpdrPdu::ServerDeviceAnnounceResponse(ServerDeviceAnnounceResponse {
                            device_id: d.device_id(),
                            result_code: NtStatus::SUCCESS,
                        }))
                    })
                    .collect())
            }
            PacketId::CoreDeviceIoCompletion => {
                // Phase 1b: route the response to the completion-id waiter.
                debug!("RDPDR: Device I/O Completion (no device I/O issued yet)");
                Ok(Vec::new())
            }
            other => {
                warn!(packet_id = ?other, "RDPDR: ignoring unhandled packet");
                Ok(Vec::new())
            }
        }
    }
}

impl SvcServerProcessor for RdpdrServer {}
