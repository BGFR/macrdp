//! Server-side RDPDR (drive redirection) static channel — [MS-RDPEFS].
//!
//! Server-direction peer to `ironrdp-rdpdr`'s client `Rdpdr`: drives the init
//! handshake the client expects (Server Announce → capability exchange → Client
//! ID Confirm → User Logged On), surfaces the client's announced devices to a
//! [`RdpdrServerHandler`] backend, and issues device-I/O requests (open / read /
//! close) whose completions are matched back to the awaiting caller by an
//! [`IoRouter`]. The async [`RdpdrHandle`] is what the backend uses to read the
//! client's files.
//!
//! Lives here alongside the other server channel processors (`echo.rs`,
//! `gfx.rs`, `sound.rs`); reuses `ironrdp-rdpdr` purely for the MS-RDPEFS wire
//! types. Upstreamable as a `SvcServerProcessor` peer to the client `Rdpdr`.
//!
//! [MS-RDPEFS]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpefs/34d9de58-b2b5-40b6-b970-f82d4603bdb5

use core::fmt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context as _, Result};
use ironrdp_core::{impl_as_any, ReadCursor};
use ironrdp_pdu::gcc::ChannelName;
use ironrdp_pdu::{decode_err, PduResult};
use ironrdp_rdpdr::pdu::efs::{
    Capabilities, ClientDeviceListAnnounce, ClientNameRequest, CoreCapability, CoreCapabilityKind, CreateDisposition,
    CreateOptions, DeviceCloseRequest, DeviceCreateRequest, DeviceCreateResponse, DeviceIoRequest, DeviceIoResponse,
    DeviceReadRequest, DeviceReadResponse, DeviceType, DesiredAccess, FileAttributes, FileDirectoryInformation,
    FileInformationClassLevel, MajorFunction, MinorFunction, NtStatus, ServerDeviceAnnounceResponse,
    ServerDriveIoRequest, ServerDriveQueryDirectoryRequest, SharedAccess, VersionAndIdPdu, VersionAndIdPduKind,
    VERSION_MAJOR, VERSION_MINOR_RDP51,
};
use ironrdp_rdpdr::pdu::{PacketId, RdpdrPdu, SharedHeader};
use ironrdp_svc::{CompressionCondition, SvcMessage, SvcProcessor, SvcServerProcessor};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::{ServerEvent, ServerEventSender};

/// A device the client announced as redirected.
#[derive(Debug, Clone)]
pub struct AnnouncedDevice {
    pub device_id: u32,
    pub device_type: DeviceType,
    /// The 8-char PreferredDosName the client sent (e.g. a drive/share label).
    pub name: String,
}

/// Backend hooks for the macrdp side of RDPDR.
pub trait RdpdrServerHandler: Send + fmt::Debug {
    /// Hands the backend the async [`RdpdrHandle`] used to read the client's
    /// files. Called once, before the channel starts. Default no-op.
    fn set_handle(&mut self, _handle: RdpdrHandle) {}
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
/// [`CliprdrServerFactory`].
///
/// [`SoundServerFactory`]: crate::SoundServerFactory
/// [`CliprdrServerFactory`]: crate::CliprdrServerFactory
pub trait RdpdrServerFactory: RdpdrBackendFactory + ServerEventSender {}

/// Outbound RDPDR messages the server pushes onto the connection event loop.
#[derive(Debug)]
pub enum RdpdrServerMessage {
    /// Pre-framed SVC messages to write on the rdpdr static channel.
    SendMessages(Vec<SvcMessage>),
}

/// One entry from a directory listing.
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// File/directory name (no path).
    pub name: String,
    /// Size in bytes (0 for directories).
    pub size: u64,
    pub is_dir: bool,
}

/// Matches `DeviceIoCompletion` responses to the request awaiting them, keyed by
/// completion id (the RDPDR analogue of clipboard's `DownloadRouter`). The
/// delivered `Vec<u8>` is the response body (a `DeviceIoResponse` followed by
/// the per-request tail); the waiter decodes the specific response type.
#[derive(Clone, Default)]
pub struct IoRouter {
    inner: Arc<IoRouterInner>,
}

#[derive(Default)]
struct IoRouterInner {
    next_id: AtomicU32,
    pending: Mutex<HashMap<u32, oneshot::Sender<Vec<u8>>>>,
}

impl IoRouter {
    fn new() -> Self {
        Self::default()
    }

    /// Allocate a completion id and a receiver for its response body.
    fn register(&self) -> (u32, oneshot::Receiver<Vec<u8>>) {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    /// Deliver a response body to the matching waiter (dropped if none).
    fn deliver(&self, completion_id: u32, body: Vec<u8>) {
        if let Some(tx) = self.inner.pending.lock().unwrap().remove(&completion_id) {
            let _ = tx.send(body);
        } else {
            warn!(completion_id, "RDPDR: I/O completion with no waiter (dropped)");
        }
    }
}

/// Async handle for issuing device-I/O to the client over the rdpdr channel.
/// Cheap to clone; backends keep one to read the client's files on demand.
#[derive(Clone)]
pub struct RdpdrHandle {
    sender: mpsc::UnboundedSender<ServerEvent>,
    router: IoRouter,
}

impl fmt::Debug for RdpdrHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdpdrHandle").finish_non_exhaustive()
    }
}

impl RdpdrHandle {
    fn new(sender: mpsc::UnboundedSender<ServerEvent>, router: IoRouter) -> Self {
        Self { sender, router }
    }

    /// Open `path` on `device_id`, read up to `length` bytes from `offset`, and
    /// close — returning the bytes read. Path uses Windows backslash separators
    /// relative to the redirected drive root (e.g. `\\readme.txt`).
    pub async fn read_file(&self, device_id: u32, path: &str, offset: u64, length: u32) -> Result<Vec<u8>> {
        let file_id = self
            .create_with(
                device_id,
                path,
                DesiredAccess::GENERIC_READ,
                CreateOptions::FILE_NON_DIRECTORY_FILE,
            )
            .await?;
        let data = self.read(device_id, file_id, offset, length).await;
        // Best-effort close even if the read failed, so the client releases the handle.
        let _ = self.close(device_id, file_id).await;
        data
    }

    /// List the entries of directory `dir_path` (Windows backslash path relative
    /// to the drive root; `\\` is the root). Returns the entries excluding the
    /// `.`/`..` pseudo-entries.
    pub async fn list_dir(&self, device_id: u32, dir_path: &str) -> Result<Vec<DirEntry>> {
        let file_id = self
            .create_with(
                device_id,
                dir_path,
                DesiredAccess::FILE_READ_DATA_OR_FILE_LIST_DIRECTORY | DesiredAccess::FILE_READ_ATTRIBUTES,
                CreateOptions::FILE_DIRECTORY_FILE,
            )
            .await?;
        let pattern = query_pattern(dir_path);
        let mut entries = Vec::new();
        let mut initial = true;
        loop {
            let (completion_id, rx) = self.router.register();
            let mut header = self.io_header(device_id, completion_id, MajorFunction::DirectoryControl);
            header.file_id = file_id;
            header.minor_function = MinorFunction::IRP_MN_QUERY_DIRECTORY;
            self.send(ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(
                ServerDriveQueryDirectoryRequest {
                    device_io_request: header,
                    file_info_class_lvl: FileInformationClassLevel::FILE_DIRECTORY_INFORMATION,
                    initial_query: u8::from(initial),
                    path: if initial { pattern.clone() } else { String::new() },
                },
            ))?;
            initial = false;

            let body = rx.await.context("RDPDR query-dir: connection closed")?;
            let mut src = ReadCursor::new(&body);
            let io = DeviceIoResponse::decode(&mut src).map_err(|e| anyhow!("{e}"))?;
            // The client signals end-of-enumeration with NO_MORE_FILES; some
            // return another error code there instead — either way we're done.
            if io.io_status != NtStatus::SUCCESS {
                break;
            }
            if src.len() < 4 {
                break;
            }
            let _length = src.read_u32();
            let entry = match FileDirectoryInformation::decode(&mut src) {
                Ok(e) => e,
                Err(e) => {
                    warn!(error = ?e, "RDPDR: undecodable directory entry");
                    break;
                }
            };
            if entry.file_name == "." || entry.file_name == ".." {
                continue;
            }
            let is_dir = entry.file_attributes.contains(FileAttributes::FILE_ATTRIBUTE_DIRECTORY);
            entries.push(DirEntry {
                name: entry.file_name,
                size: u64::try_from(entry.end_of_file).unwrap_or(0),
                is_dir,
            });
        }
        let _ = self.close(device_id, file_id).await;
        Ok(entries)
    }

    fn send(&self, req: ServerDriveIoRequest) -> Result<()> {
        let msg = SvcMessage::from(req);
        self.sender
            .send(ServerEvent::Rdpdr(RdpdrServerMessage::SendMessages(vec![msg])))
            .map_err(|_| anyhow!("RDPDR event channel closed"))
    }

    fn io_header(&self, device_id: u32, completion_id: u32, major: MajorFunction) -> DeviceIoRequest {
        DeviceIoRequest {
            device_id,
            file_id: 0,
            completion_id,
            major_function: major,
            minor_function: MinorFunction::from(0),
        }
    }

    async fn create_with(
        &self,
        device_id: u32,
        path: &str,
        desired_access: DesiredAccess,
        create_options: CreateOptions,
    ) -> Result<u32> {
        let (completion_id, rx) = self.router.register();
        self.send(ServerDriveIoRequest::ServerCreateDriveRequest(DeviceCreateRequest {
            device_io_request: self.io_header(device_id, completion_id, MajorFunction::Create),
            desired_access,
            allocation_size: 0,
            file_attributes: FileAttributes::empty(),
            shared_access: SharedAccess::FILE_SHARE_READ,
            create_disposition: CreateDisposition::FILE_OPEN,
            create_options,
            path: path.to_owned(),
        }))?;
        let body = rx.await.context("RDPDR create: connection closed")?;
        let resp = DeviceCreateResponse::decode(&mut ReadCursor::new(&body)).map_err(|e| anyhow!("{e}"))?;
        if resp.device_io_reply.io_status != NtStatus::SUCCESS {
            return Err(anyhow!("RDPDR create({path}) failed: {:?}", resp.device_io_reply.io_status));
        }
        Ok(resp.file_id)
    }

    async fn read(&self, device_id: u32, file_id: u32, offset: u64, length: u32) -> Result<Vec<u8>> {
        let (completion_id, rx) = self.router.register();
        let mut header = self.io_header(device_id, completion_id, MajorFunction::Read);
        header.file_id = file_id;
        self.send(ServerDriveIoRequest::DeviceReadRequest(DeviceReadRequest {
            device_io_request: header,
            length,
            offset,
        }))?;
        let body = rx.await.context("RDPDR read: connection closed")?;
        let resp = DeviceReadResponse::decode(&mut ReadCursor::new(&body)).map_err(|e| anyhow!("{e}"))?;
        if resp.device_io_reply.io_status != NtStatus::SUCCESS {
            return Err(anyhow!("RDPDR read failed: {:?}", resp.device_io_reply.io_status));
        }
        Ok(resp.read_data)
    }

    async fn close(&self, device_id: u32, file_id: u32) -> Result<()> {
        let (completion_id, rx) = self.router.register();
        let mut header = self.io_header(device_id, completion_id, MajorFunction::Close);
        header.file_id = file_id;
        self.send(ServerDriveIoRequest::DeviceCloseRequest(DeviceCloseRequest {
            device_io_request: header,
        }))?;
        let _ = rx.await; // ignore close status
        Ok(())
    }
}

/// Server-side RDPDR channel processor.
pub struct RdpdrServer {
    backend: Box<dyn RdpdrServerHandler>,
    /// Shown to the client; consumed by the `Debug` impl and later phases.
    computer_name: String,
    /// Server-chosen client id, echoed through the announce handshake.
    client_id: u32,
    /// Shared with [`RdpdrHandle`] — inbound I/O completions are delivered here.
    router: IoRouter,
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
    pub fn new(backend: Box<dyn RdpdrServerHandler>, computer_name: String, router: IoRouter) -> Self {
        Self {
            backend,
            computer_name,
            client_id: 0x0000_0001,
            router,
        }
    }

    /// The capabilities the server advertises. The client keeps only the
    /// capabilities the server also advertises, so the Drive capability must be
    /// present for drive redirection to survive.
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
                // Decode the DeviceIoResponse header to route by completion id;
                // deliver the whole body to the waiter, which decodes the
                // specific response type it asked for.
                let body = src.remaining();
                match DeviceIoResponse::decode(&mut ReadCursor::new(body)) {
                    Ok(resp) => {
                        debug!(completion_id = resp.completion_id, io_status = ?resp.io_status, "RDPDR: device I/O completion");
                        self.router.deliver(resp.completion_id, body.to_vec());
                    }
                    Err(e) => warn!(error = ?e, "RDPDR: undecodable device I/O completion"),
                }
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

/// The search pattern for an initial query-directory request: the directory
/// path with a trailing `\*` wildcard (relative to the drive root).
fn query_pattern(dir: &str) -> String {
    if dir.is_empty() || dir == "\\" {
        "\\*".to_owned()
    } else if dir.ends_with('\\') {
        format!("{dir}*")
    } else {
        format!("{dir}\\*")
    }
}

/// Build the channel processor + wire the backend's [`RdpdrHandle`]. Called from
/// `RdpServer::attach_channels` with the connection's `ServerEvent` sender.
pub(crate) fn build_rdpdr(
    factory: &dyn RdpdrServerFactory,
    sender: mpsc::UnboundedSender<ServerEvent>,
) -> RdpdrServer {
    let mut backend = factory.build_backend();
    let router = IoRouter::new();
    backend.set_handle(RdpdrHandle::new(sender, router.clone()));
    RdpdrServer::new(backend, factory.computer_name(), router)
}
