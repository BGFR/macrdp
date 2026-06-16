//! RDPDR drive redirection — the macrdp side of the server-side RDPDR static
//! channel (the protocol state machine lives in the vendored
//! `ironrdp-server::rdpdr`). The RDP *client* redirects its local drive; the
//! Mac (server) browses/reads the client's files.
//!
//! Done: the MS-RDPEFS init handshake (1a) and device I/O — the backend can
//! `list_dir` and `read_file` the client's drive via the [`RdpdrHandle`] (1b).
//! The macOS Finder temp-folder surface (1c) is the remaining piece.
//! Opt-in via `--enable-drive-redirection`.

use ironrdp_rdpdr::pdu::efs::DeviceType;
use ironrdp_server::{
    AnnouncedDevice, RdpdrBackendFactory, RdpdrHandle, RdpdrServerFactory, RdpdrServerHandler,
    ServerEvent, ServerEventSender,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Factory for the RDPDR static channel (mirrors `MacCliprdr` / `MacRdpsnd`).
#[derive(Debug, Default)]
pub struct MacRdpdr;

impl MacRdpdr {
    pub fn new() -> Self {
        Self
    }
}

impl ServerEventSender for MacRdpdr {
    fn set_sender(&mut self, _sender: mpsc::UnboundedSender<ServerEvent>) {
        // No-op: the backend's RdpdrHandle is wired with the connection's event
        // sender by the server's `build_rdpdr`, so the factory needn't retain one.
    }
}

impl RdpdrBackendFactory for MacRdpdr {
    fn build_backend(&self) -> Box<dyn RdpdrServerHandler> {
        Box::new(MacRdpdrHandler { handle: None })
    }

    fn computer_name(&self) -> String {
        hostname()
    }
}

impl RdpdrServerFactory for MacRdpdr {}

/// Backend for the RDPDR server processor. Logs announced devices; later phases
/// drive the temp-folder Finder surface from here. The `RdpdrHandle` is used to
/// read the client's files on demand.
#[derive(Debug)]
struct MacRdpdrHandler {
    handle: Option<RdpdrHandle>,
}

impl RdpdrServerHandler for MacRdpdrHandler {
    fn set_handle(&mut self, handle: RdpdrHandle) {
        self.handle = Some(handle);
    }

    fn on_devices_announced(&mut self, devices: &[AnnouncedDevice]) {
        for d in devices {
            info!(
                device_id = d.device_id,
                device_type = ?d.device_type,
                name = %d.name,
                "drive redirection: client redirected a device"
            );
        }

        // Phase 1b verification hooks (read-only, no Finder surface yet): on the
        // first redirected filesystem, MACRDP_RDPDR_TEST_LIST=1 lists the root and
        // MACRDP_RDPDR_TEST_READ=<path> reads that path, logging the result. The
        // Finder surface (Phase 1c) is what drives these in normal use.
        let (Some(handle), Some(dev)) = (
            self.handle.clone(),
            devices
                .iter()
                .find(|d| d.device_type == DeviceType::Filesystem),
        ) else {
            return;
        };
        let device_id = dev.device_id;

        if std::env::var("MACRDP_RDPDR_TEST_LIST").is_ok() {
            let handle = handle.clone();
            tokio::spawn(async move {
                match handle.list_dir(device_id, "\\").await {
                    Ok(entries) => {
                        info!(
                            device_id,
                            count = entries.len(),
                            "drive redirection: list_dir(\\) OK"
                        );
                        for e in &entries {
                            info!(name = %e.name, size = e.size, is_dir = e.is_dir, "drive redirection:   entry");
                        }
                    }
                    Err(e) => warn!(device_id, error = %e, "drive redirection: list_dir failed"),
                }
            });
        }

        if let Ok(path) = std::env::var("MACRDP_RDPDR_TEST_READ") {
            tokio::spawn(async move {
                match handle.read_file(device_id, &path, 0, 4096).await {
                    Ok(bytes) => {
                        let n = bytes.len().min(160);
                        info!(
                            device_id,
                            path = %path,
                            bytes = bytes.len(),
                            preview = %String::from_utf8_lossy(&bytes[..n]),
                            "drive redirection: read_file OK"
                        );
                    }
                    Err(e) => {
                        warn!(device_id, path = %path, error = %e, "drive redirection: read_file failed")
                    }
                }
            });
        }
    }
}

/// The Mac's hostname, shown to the client (Explorer renders a redirected
/// share as "`<dir>` on `<hostname>`"). Falls back to `"macrdp"`.
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes up to buf.len() bytes and null-terminates.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let name = String::from_utf8_lossy(&buf[..end]).into_owned();
        if !name.is_empty() {
            return name;
        }
    }
    "macrdp".to_owned()
}
