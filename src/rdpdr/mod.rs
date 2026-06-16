//! RDPDR drive redirection — the macrdp side of the server-side RDPDR static
//! channel (the protocol state machine lives in the vendored
//! `ironrdp-server::rdpdr`). The RDP *client* redirects its local drive; the
//! Mac (server) browses/reads the client's files.
//!
//! Done: the MS-RDPEFS init handshake (1a), device I/O — `list_dir` /
//! `read_file` via the [`RdpdrHandle`] (1b) — and the macOS surface: a real
//! NFS mount (Phase 2). An in-process NFSv3 server backed by the `RdpdrHandle`
//! is mounted at `/Volumes/<label>` via the built-in `mount_nfs` (no root, no
//! kext), so the client's drive appears as a proper Finder volume with lazy
//! subdirectory navigation and on-demand reads.
//! Opt-in via `--enable-drive-redirection`. Read-only.

#[cfg(target_os = "macos")]
mod surface;

#[cfg(target_os = "macos")]
use ironrdp_rdpdr::pdu::efs::DeviceType;
use ironrdp_server::{
    AnnouncedDevice, RdpdrBackendFactory, RdpdrHandle, RdpdrServerFactory, RdpdrServerHandler,
    ServerEvent, ServerEventSender,
};
use tokio::sync::mpsc;
use tracing::info;

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
        Box::new(MacRdpdrHandler {
            handle: None,
            #[cfg(target_os = "macos")]
            surface: None,
        })
    }

    fn computer_name(&self) -> String {
        hostname()
    }
}

impl RdpdrServerFactory for MacRdpdr {}

/// Backend for the RDPDR server processor. Logs announced devices and, on
/// macOS, mounts the first redirected filesystem as a real NFS volume
/// (dropped — and unmounted — when the connection ends).
#[derive(Debug)]
struct MacRdpdrHandler {
    handle: Option<RdpdrHandle>,
    #[cfg(target_os = "macos")]
    surface: Option<surface::Surface>,
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

        // Mount the first redirected filesystem in Finder (once — the client may
        // re-announce the same device list several times during init).
        #[cfg(target_os = "macos")]
        {
            if self.surface.is_none() {
                if let (Some(handle), Some(dev)) = (
                    self.handle.clone(),
                    devices
                        .iter()
                        .find(|d| d.device_type == DeviceType::Filesystem),
                ) {
                    info!(device_id = dev.device_id, name = %dev.name, "drive redirection: mounting client drive as NFS volume");
                    self.surface = Some(surface::Surface::start(handle, dev.device_id, &dev.name));
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = &self.handle;
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
