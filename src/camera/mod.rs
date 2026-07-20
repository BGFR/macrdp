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
//! `ironrdp-server` ([`RdCameraServer`] + `RdCameraDeviceProcessor`, divergence 19).
//!
//! The full pipeline is LIVE (Phases 1–3, verified 2026-07-20): H.264 samples arrive
//! over TCP → [`decode`] turns them into `420v` `CVPixelBuffer`s (VideoToolbox) →
//! [`feed`] enqueues those onto the CoreMediaIO sink of the **macrdp Camera** system
//! extension (`gui/Sources/macrdpcamera`), which presents them as a real macOS camera
//! in Photo Booth / Zoom / FaceTime. Phase 4 (migrating the channel to UDP) is
//! scoped but deferred — TCP carries the stream fine.
//!
//! This file stays the cross-platform policy layer; the macOS-specific decode and
//! CMIO-feed code is behind `#[cfg(target_os = "macos")]` submodules.
//!
//! The factory carries the connection's server-event sender (`ServerEventSender`)
//! so the enumerator can ask the event loop to open a per-device channel — exactly
//! the URBDRC `MacUsb` pattern.
//!
//! Gated behind `--enable-camera-redirection`; when the factory isn't installed the
//! `RDCamera_Device_Enumerator` channel is never advertised (byte-identical build).

use tokio::sync::mpsc;

use ironrdp_server::{
    CameraSampleSink, RdCameraServer, RdCameraServerFactory, ServerEvent, ServerEventSender,
};

#[cfg(target_os = "macos")]
mod decode;
#[cfg(target_os = "macos")]
mod feed;

/// MS-RDPECAM `CAM_MEDIA_FORMAT` for H.264.
const FORMAT_H264: u8 = 0x01;

/// Opt-in decode diagnostics (`MACRDP_CAMERA_DUMP=1`): write the raw H.264 elementary
/// stream (~10 MiB cap) plus the first few decoded frames as PNG to `$TMPDIR`, and
/// report average luma. These exist to debug the decode path (is the source black? is
/// the bitstream decodable?) and are **off by default** — a camera session shouldn't
/// litter `$TMPDIR` or pay for the per-frame luma scan in normal use.
///
/// Deliberately NOT `#[cfg(target_os = "macos")]`: the raw-stream dump lives in the
/// cross-platform `on_media_type`, so gating this on macOS breaks the Linux stub build.
pub(crate) fn camera_dump_enabled() -> bool {
    matches!(
        std::env::var("MACRDP_CAMERA_DUMP").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// Stable UID of the "macrdp Camera" virtual device — MUST match the `deviceID`
/// UUID the CoreMediaIO extension sets (`gui/Sources/macrdpcamera/main.swift`). The
/// sink-feed producer (`feed.rs`) matches the CMIO device on this exact string.
#[cfg(target_os = "macos")]
const MACRDP_CAMERA_DEVICE_UID: &str = "6F1B2C3D-4E5A-6B7C-8D9E-A0B1C2D3E4F0";

/// Phase-2a verification sink: writes the raw H.264 **Annex-B elementary stream**
/// to a file so it can be played back (`ffplay`/VLC) to confirm the redirected
/// webcam is a real, decodable, live image — zero decode code, just I/O. Capped so
/// it can't fill the disk. On macOS this also drives the VideoToolbox decoder
/// (`decode.rs`) — the Phase-2b path that produces `CVPixelBuffer`s for Phase 3.
struct CameraSink {
    format: u8,
    width: u32,
    height: u32,
    /// Raw-stream dump (Phase 2a). `None` once the cap is hit or on open failure.
    dump: Option<std::fs::File>,
    dumped_bytes: u64,
    frames: u64,
    dump_path: std::path::PathBuf,
    #[cfg(target_os = "macos")]
    decoder: Option<decode::H264Decoder>,
}

/// Cap the raw-stream dump (~10 MiB ≈ 25–30 s at 1080p20) so it can't grow forever.
const DUMP_CAP_BYTES: u64 = 10 * 1024 * 1024;

impl CameraSink {
    fn new() -> Self {
        Self {
            format: 0,
            width: 0,
            height: 0,
            dump: None,
            dumped_bytes: 0,
            frames: 0,
            dump_path: std::path::PathBuf::new(),
            #[cfg(target_os = "macos")]
            decoder: None,
        }
    }
}

impl CameraSampleSink for CameraSink {
    fn on_media_type(&mut self, format: u8, width: u32, height: u32) {
        self.format = format;
        self.width = width;
        self.height = height;
        // Opt-in raw-stream dump (H.264 only — MJPG/raw would need a different
        // container/verification). Off unless MACRDP_CAMERA_DUMP=1.
        if format == FORMAT_H264 && camera_dump_enabled() {
            let path = std::env::temp_dir().join(format!(
                "macrdp-camera-{}-{}x{}.h264",
                std::process::id(),
                width,
                height
            ));
            match std::fs::File::create(&path) {
                Ok(f) => {
                    tracing::info!(
                        path = %path.display(),
                        width,
                        height,
                        "camera Phase-2a: dumping the H.264 elementary stream (play with `ffplay <path>`)"
                    );
                    self.dump_path = path;
                    self.dump = Some(f);
                }
                Err(e) => tracing::warn!(error = %e, "camera: could not open the H.264 dump file"),
            }
        } else if camera_dump_enabled() {
            tracing::info!(
                format = format_args!("0x{format:02x}"),
                "camera: non-H.264 format — raw-stream dump skipped"
            );
        }
        // Phase 2b/3b (macOS): stand up the VideoToolbox decoder for H.264, and
        // (Phase 3b) a CoreMediaIO sink feed to present the decoded frames as the
        // "macrdp Camera". The feed is best-effort — if the camera system extension
        // isn't installed/active there's no device to feed, so we just decode.
        #[cfg(target_os = "macos")]
        if format == FORMAT_H264 {
            let feed = match feed::CameraFeed::new() {
                Ok(f) => Some(f),
                Err(e) => {
                    tracing::info!(error = %e, "camera Phase-3b: no sink feed (extension inactive?) — decoding only");
                    None
                }
            };
            match decode::H264Decoder::new(width, height, feed) {
                Ok(d) => self.decoder = Some(d),
                Err(e) => tracing::warn!(error = %e, "camera: VideoToolbox decoder init failed"),
            }
        }
    }

    fn on_sample(&mut self, data: &[u8]) {
        self.frames += 1;
        // Phase 2a: append the Annex-B access unit to the dump (until capped).
        if let Some(f) = self.dump.as_mut() {
            use std::io::Write;
            if self.dumped_bytes < DUMP_CAP_BYTES {
                if let Err(e) = f.write_all(data) {
                    tracing::warn!(error = %e, "camera: H.264 dump write failed — stopping dump");
                    self.dump = None;
                } else {
                    self.dumped_bytes += data.len() as u64;
                    if self.dumped_bytes >= DUMP_CAP_BYTES {
                        tracing::info!(
                            path = %self.dump_path.display(),
                            frames = self.frames,
                            bytes = self.dumped_bytes,
                            "camera Phase-2a: H.264 dump reached its cap — play it to confirm a live image"
                        );
                        let _ = f.flush();
                        self.dump = None;
                    }
                }
            }
        }
        // Phase 2b (macOS): decode the access unit → CVPixelBuffer (+ dump a JPEG
        // frame periodically to confirm a decoded live image).
        #[cfg(target_os = "macos")]
        if let Some(d) = self.decoder.as_mut() {
            d.decode(data);
        }
    }
}

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

    fn build_sample_sink(&self) -> Option<Box<dyn CameraSampleSink>> {
        Some(Box::new(CameraSink::new()))
    }
}
