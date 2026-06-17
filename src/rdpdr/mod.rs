//! RDPDR drive redirection — the macrdp side of the server-side RDPDR static
//! channel (the protocol state machine lives in the vendored
//! `ironrdp-server::rdpdr`). The RDP *client* redirects its local drive; the
//! Mac (server) browses/reads the client's files.
//!
//! Done: the MS-RDPEFS init handshake (1a), device I/O — `list_dir` /
//! `read_file` via the [`RdpdrHandle`] (1b) — and the macOS surface: a real
//! NFS mount (Phase 2). An in-process NFSv3 server backed by the `RdpdrHandle`
//! is mounted via the built-in `mount_nfs` (no root, no kext), so the client's
//! drive appears as a proper Finder volume with lazy subdirectory navigation,
//! on-demand reads, and writes (create / write / mkdir / rename / delete /
//! truncate map to RDPDR DeviceWrite / DeviceCreate / SetInformation).
//! Opt-in via `--enable-drive-redirection`. Read-write.

#[cfg(target_os = "macos")]
mod surface;

/// Unmount any RDPDR NFS volumes still mounted at process exit. macrdp's signal
/// handler `std::process::exit`s, which skips `Surface::Drop`, so without this a
/// signal stop (Ctrl-C, `kill`, `launchctl bootout`/`kickstart -k`) would strand
/// the mounts pointing at the now-dead server. Call it from the signal handler
/// next to `file_promise_lazy::shutdown_cleanup()`. No-op when no drive was
/// redirected, and off macOS.
pub fn shutdown_cleanup() {
    #[cfg(target_os = "macos")]
    surface::shutdown_cleanup();
}

#[cfg(target_os = "macos")]
use std::collections::HashMap;

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
            surfaces: HashMap::new(),
        })
    }

    fn computer_name(&self) -> String {
        hostname()
    }
}

impl RdpdrServerFactory for MacRdpdr {}

/// Backend for the RDPDR server processor. Logs announced devices and, on
/// macOS, mounts each redirected filesystem as its own real NFS volume
/// (all dropped — and unmounted — when the connection ends).
#[derive(Debug)]
struct MacRdpdrHandler {
    handle: Option<RdpdrHandle>,
    /// One live NFS mount per redirected filesystem device, keyed by device id
    /// so a re-announce doesn't double-mount an already-mounted drive.
    #[cfg(target_os = "macos")]
    surfaces: HashMap<u32, surface::Surface>,
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

        // Mount every redirected filesystem in Finder, one volume each. The
        // client may re-announce the same list several times during init (and
        // may add drives later), so mount only device ids we aren't already
        // surfacing.
        #[cfg(target_os = "macos")]
        {
            if let Some(handle) = self.handle.clone() {
                for dev in devices
                    .iter()
                    .filter(|d| d.device_type == DeviceType::Filesystem)
                {
                    if self.surfaces.contains_key(&dev.device_id) {
                        continue;
                    }
                    info!(device_id = dev.device_id, name = %dev.name, "drive redirection: mounting client drive as NFS volume");
                    let surface = surface::Surface::start(handle.clone(), dev.device_id, &dev.name);
                    self.surfaces.insert(dev.device_id, surface);
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
