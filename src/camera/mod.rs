//! Camera redirection (MS-RDPECAM) — **Phase 1: protocol negotiation → samples**.
//!
//! macrdp is the RDP server / presenting side: a physical webcam lives on the
//! client, and when the user enables "Video capture devices" in the client's
//! local-resources the client redirects it over the MS-RDPECAM *Video Capture
//! Virtual Channel Extension* (enumeration channel `RDCamera_Device_Enumerator`).
//! The decrypted-pcap investigation (see `docs/rdp-camera-redirection-feasibility.md`
//! and the `project_camera_redirection_feasibility` memory) proved mstsc redirects
//! the webcam over this dedicated video channel, NOT USB/URBDRC.
//!
//! [`MacCamera`] is the macrdp-side factory. The MS-RDPECAM state machine —
//! enumerator handshake, per-device channel, stream/media-type negotiation, and
//! the SampleRequest↔SampleResponse pull loop — lives in the vendored
//! `ironrdp-server` ([`RdCameraServer`] + `RdCameraDeviceProcessor`, divergence
//! 19). Phase 1 receives the H.264 sample stream over TCP and *logs* it (the
//! `MS-RDPECAM sample received (Phase-1 GREEN …)` line proves frames flow); it does
//! **not** decode (Phase 2 — VideoToolbox), present a macOS camera (Phase 3 —
//! CoreMediaIO Camera Extension), or migrate to UDP (Phase 4). So this module is
//! still cross-platform (pure protocol policy) — the macOS presentation code
//! arrives in Phase 2+ as a frame sink handed to the device processor.
//!
//! The factory carries the connection's server-event sender (`ServerEventSender`)
//! so the enumerator can ask the event loop to open a per-device channel — exactly
//! the URBDRC `MacUsb` pattern.
//!
//! Gated behind `--enable-camera-redirection`; when the factory isn't installed the
//! `RDCamera_Device_Enumerator` channel is never advertised (byte-identical build).

use tokio::sync::mpsc;

use ironrdp_server::{RdCameraServer, RdCameraServerFactory, ServerEvent, ServerEventSender};

/// The macrdp-side MS-RDPECAM factory (Phase 1). Holds the connection's
/// server-event sender so each built enumerator processor can request per-device
/// channel opens.
pub struct MacCamera {
    sender: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl MacCamera {
    pub fn new() -> Self {
        Self { sender: None }
    }
}

impl Default for MacCamera {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerEventSender for MacCamera {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.sender = Some(sender);
    }
}

impl RdCameraServerFactory for MacCamera {
    fn build_processor(&self) -> RdCameraServer {
        RdCameraServer::with_sender(self.sender.clone())
    }
}
