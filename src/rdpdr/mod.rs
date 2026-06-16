//! RDPDR drive redirection — the macrdp side of the server-side RDPDR static
//! channel (the protocol state machine lives in the vendored
//! `ironrdp-server::rdpdr`). The RDP *client* redirects its local drive; the
//! Mac (server) browses/reads the client's files.
//!
//! Phase 1a: complete the MS-RDPEFS init handshake and log the client's
//! announced drives. Device I/O (file read) and the Finder surface follow.
//! Opt-in via `--enable-drive-redirection`.

use std::sync::{Arc, Mutex};

use ironrdp_server::{
    AnnouncedDevice, RdpdrBackendFactory, RdpdrServerFactory, RdpdrServerHandler, ServerEvent,
    ServerEventSender,
};
use tokio::sync::mpsc;
use tracing::info;

type Sender = Arc<Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>>;

/// Factory for the RDPDR static channel (mirrors `MacCliprdr` / `MacRdpsnd`).
#[derive(Debug, Default)]
pub struct MacRdpdr {
    /// Kept for later phases (server-initiated device-I/O requests ride this).
    sender: Sender,
}

impl MacRdpdr {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ServerEventSender for MacRdpdr {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        *self.sender.lock().unwrap() = Some(sender);
    }
}

impl RdpdrBackendFactory for MacRdpdr {
    fn build_backend(&self) -> Box<dyn RdpdrServerHandler> {
        Box::new(MacRdpdrHandler {
            _sender: self.sender.clone(),
        })
    }

    fn computer_name(&self) -> String {
        hostname()
    }
}

impl RdpdrServerFactory for MacRdpdr {}

/// Backend for the RDPDR server processor. Phase 1a only logs the announced
/// devices; later phases drive the temp-folder Finder surface from here.
#[derive(Debug)]
struct MacRdpdrHandler {
    _sender: Sender,
}

impl RdpdrServerHandler for MacRdpdrHandler {
    fn on_devices_announced(&mut self, devices: &[AnnouncedDevice]) {
        for d in devices {
            info!(
                device_id = d.device_id,
                device_type = ?d.device_type,
                name = %d.name,
                "drive redirection: client redirected a device"
            );
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
