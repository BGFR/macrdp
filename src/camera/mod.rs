//! Camera redirection (MS-RDPECAM) — **Phase 0 protocol gate only**.
//!
//! macrdp is the RDP server / presenting side: a physical webcam lives on the
//! client, and when the user enables "Video capture devices" in the client's
//! local-resources the client redirects it over the MS-RDPECAM *Video Capture
//! Virtual Channel Extension* (enumeration channel `RDCamera_Device_Enumerator`).
//! The decrypted-pcap investigation (see `docs/rdp-camera-redirection-feasibility.md`
//! and the `project_camera_redirection_feasibility` memory) proved mstsc redirects
//! the webcam over this dedicated video channel, NOT USB/URBDRC.
//!
//! [`MacCamera`] is the cheap go/no-go gate: it installs the vendored
//! [`RdCameraServer`] enumeration processor, which advertises the channel, answers
//! the client's version negotiation, and *logs* the client's
//! `DEVICE_ADDED_NOTIFICATION`. That log line is the Phase-0 GREEN signal — proof
//! that a modern mstsc/Win11 will hand macrdp its redirected camera over
//! MS-RDPECAM — before committing to the multi-week presentation work (per-device
//! channels, stream/media-type negotiation, H.264 sample decode, and a macOS
//! CoreMediaIO Camera Extension). It opens no per-device channel, drives no stream,
//! and touches no macOS code — so this module is cross-platform (pure protocol
//! policy), like `src/multitransport.rs`.
//!
//! Gated behind `--enable-camera-redirection`; when the factory isn't installed the
//! `RDCamera_Device_Enumerator` channel is never advertised (byte-identical build).

use ironrdp_server::{RdCameraServer, RdCameraServerFactory};

/// The macrdp-side MS-RDPECAM factory (Phase 0). Stateless — the enumeration
/// processor replies inline from `process()` and needs no event sender.
pub struct MacCamera {}

impl MacCamera {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for MacCamera {
    fn default() -> Self {
        Self::new()
    }
}

impl RdCameraServerFactory for MacCamera {
    fn build_processor(&self) -> RdCameraServer {
        RdCameraServer::new()
    }
}
