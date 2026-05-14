//! Forward macOS system audio to the RDP client via the RDPSND SVC.
//!
//! We tap the same display ScreenCaptureKit gives us video for, but with a
//! second SCStream configured for audio output (`captures_audio = true`,
//! `SCStreamOutputType::Audio`). SCK delivers 32-bit float PCM at the
//! configured sample rate; we convert to 16-bit signed PCM interleaved and
//! ship via `RdpsndServerMessage::Wave`.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use ironrdp_rdpsnd::pdu::{AudioFormat, ClientAudioFormatPdu, WaveFormat};
use ironrdp_rdpsnd::server::{RdpsndServerHandler, RdpsndServerMessage};
use ironrdp_server::{ServerEvent, ServerEventSender, SoundServerFactory};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

// ScreenCaptureKit only honors 8000/16000/24000/48000 Hz; asking for 44100
// is silently served as 48000, so the advertised RDPSND format must match or
// the client plays back slow/low-pitched and the buffer drifts unboundedly.
const SAMPLE_RATE: u32 = 48000;
const CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 16;

type Sender = Arc<Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>>;

#[derive(Debug)]
pub struct MacRdpsnd {
    sender: Sender,
    // Monotonic capture-loop generation, shared with every backend this
    // factory builds. mstsc's cert-prompt reconnect makes ironrdp build a
    // second backend (and thus a second capture loop) while the first may
    // still be alive; both would feed the shared `sender` and the client
    // would receive ~2x the audio. Each `start()` claims a new generation;
    // older capture loops observe the bump and exit, so at most one runs.
    generation: Arc<AtomicU64>,
}

impl MacRdpsnd {
    pub fn new() -> Self {
        Self {
            sender: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl ServerEventSender for MacRdpsnd {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        *self.sender.lock().unwrap() = Some(sender);
    }
}

impl SoundServerFactory for MacRdpsnd {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        Box::new(MacRdpsndBackend {
            sender: self.sender.clone(),
            generation: self.generation.clone(),
            my_gen: 0,
            formats: vec![pcm_format()],
        })
    }
}

fn pcm_format() -> AudioFormat {
    let block_align = (CHANNELS as u32) * (BITS_PER_SAMPLE as u32 / 8);
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: CHANNELS,
        n_samples_per_sec: SAMPLE_RATE,
        n_avg_bytes_per_sec: SAMPLE_RATE * block_align,
        n_block_align: block_align as u16,
        bits_per_sample: BITS_PER_SAMPLE,
        data: None,
    }
}

#[derive(Debug)]
struct MacRdpsndBackend {
    sender: Sender,
    generation: Arc<AtomicU64>,
    // Generation claimed by this backend's capture loop, 0 until `start()`.
    my_gen: u64,
    formats: Vec<AudioFormat>,
}

impl RdpsndServerHandler for MacRdpsndBackend {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn start(&mut self, client_format: &ClientAudioFormatPdu) -> Option<u16> {
        // wFormatNo in the Wave/Wave2 PDU indexes the *server's* format list
        // (what we sent in the Server Audio Formats PDU), not the client's
        // reply list. Returning a client-list index here makes the client
        // decode our audio with whatever format sits at that index in the
        // server list — audible as wrong pitch and progressive drift when the
        // two lists don't line up. So find the first server format the client
        // echoed back as accepted and return *its* index into our list.
        let format_no = self.formats.iter().position(|server_fmt| {
            client_format.formats.iter().any(|client_fmt| {
                client_fmt.format == server_fmt.format
                    && client_fmt.n_channels == server_fmt.n_channels
                    && client_fmt.n_samples_per_sec == server_fmt.n_samples_per_sec
                    && client_fmt.bits_per_sample == server_fmt.bits_per_sample
            })
        });
        let Some(format_no) = format_no else {
            warn!(
                client_formats = client_format.formats.len(),
                "client accepted none of the server audio formats; no audio"
            );
            return None;
        };
        debug!(
            format_no,
            client_formats = client_format.formats.len(),
            version = ?client_format.version,
            "rdpsnd audio format negotiated"
        );

        // Claim a fresh generation. Any capture loop from a previous
        // connection sees the bump on its next iteration and exits, so it
        // never feeds the shared event channel alongside this one.
        self.my_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;

        let sender = self.sender.clone();
        let generation = self.generation.clone();
        let my_gen = self.my_gen;
        tokio::spawn(async move {
            if let Err(e) = capture_loop(sender, generation, my_gen).await {
                warn!("audio capture loop ended: {e}");
            }
        });
        Some(format_no as u16)
    }

    fn stop(&mut self) {
        // Retire our capture loop, but only if it is still the active one — a
        // newer connection may have already superseded us.
        let _ = self.generation.compare_exchange(
            self.my_gen,
            self.my_gen + 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }
}

#[cfg(target_os = "macos")]
async fn capture_loop(
    sender: Sender,
    generation: Arc<AtomicU64>,
    my_gen: u64,
) -> anyhow::Result<()> {
    use anyhow::{anyhow, Context};
    use screencapturekit::async_api::{AsyncSCShareableContent, AsyncSCStream};
    use screencapturekit::prelude::{SCContentFilter, SCStreamConfiguration, SCStreamOutputType};

    let content = AsyncSCShareableContent::get()
        .await
        .map_err(|e| anyhow!("SCShareableContent for audio: {e:?}"))?;
    let displays = content.displays();
    let display = displays.first().context("no displays for audio capture")?;

    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build();

    let config = SCStreamConfiguration::new()
        .with_captures_audio(true)
        .with_sample_rate(SAMPLE_RATE as i32)
        .with_channel_count(CHANNELS as i32);

    // Shallow queue: SCK's async buffer is a drop-oldest ring of this depth.
    // Each slot is ~20 ms of audio, so 2 caps capture-side staleness at ~40 ms
    // while leaving one slot of headroom against scheduler jitter. Lower would
    // trade dropouts for marginal latency; the real backlog is downstream.
    let stream = AsyncSCStream::new(&filter, &config, 2, SCStreamOutputType::Audio);
    stream
        .start_capture()
        .map_err(|e| anyhow!("audio start_capture: {e:?}"))?;
    debug!("audio capture started");

    let start_instant = std::time::Instant::now();
    let mut format_logged = false;
    // Diagnostic: measure how many stereo frames we actually hand off per
    // wall-clock second. If this isn't ~SAMPLE_RATE, we're over/under-feeding
    // the client and that is the drift (and, if over, the lowered pitch).
    let mut frames_sent: u64 = 0;
    let mut last_rate_log = start_instant;

    loop {
        if generation.load(Ordering::SeqCst) != my_gen {
            debug!(my_gen, "audio capture loop superseded; exiting");
            break;
        }
        let Some(sample) = stream.next().await else {
            break;
        };

        // Log the format SCK actually delivers, once per session. SCK does not
        // always honor the requested rate/channels; a mismatch against the
        // advertised RDPSND format makes the client play at the wrong frame
        // rate — audible as drift and lowered pitch.
        if !format_logged {
            format_logged = true;
            if let Some(fd) = sample.format_description() {
                let rate = fd.audio_sample_rate().unwrap_or(0.0);
                let channels = fd.audio_channel_count().unwrap_or(0);
                info!(rate, channels, "SCK audio format");
                if rate != 0.0 && (rate - f64::from(SAMPLE_RATE)).abs() > 1.0 {
                    warn!(
                        delivered = rate,
                        advertised = SAMPLE_RATE,
                        "SCK audio rate differs from advertised RDPSND rate; \
                         playback will drift (resampling not implemented)"
                    );
                }
            }
        }

        let Some(list) = sample.audio_buffer_list() else {
            continue;
        };

        // SCK delivers float32 PCM as planar (one buffer per channel) or a
        // single interleaved buffer. Normalize to interleaved 16-bit stereo so
        // the payload always matches the advertised RDPSND format.
        let pcm = float_list_to_pcm16_stereo(&list);
        if pcm.is_empty() {
            continue;
        }

        // 4 bytes per stereo i16 frame.
        frames_sent += (pcm.len() / 4) as u64;
        let now = std::time::Instant::now();
        if now.duration_since(last_rate_log).as_secs() >= 2 {
            let elapsed = now.duration_since(start_instant).as_secs_f64();
            let effective_hz = frames_sent as f64 / elapsed;
            debug!(effective_hz, frames_sent, elapsed, "audio production rate");
            last_rate_log = now;
        }

        let ts_ms = start_instant.elapsed().as_millis() as u32;
        let s = {
            let guard = sender.lock().unwrap();
            guard.clone()
        };
        let Some(s) = s else { break };
        if s.send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(pcm, ts_ms)))
            .is_err()
        {
            break;
        }
    }

    let _ = stream.stop_capture();
    debug!("audio capture stopped");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
async fn capture_loop(
    _sender: Sender,
    _generation: Arc<AtomicU64>,
    _my_gen: u64,
) -> anyhow::Result<()> {
    Ok(())
}

/// Normalize an SCK audio buffer list to interleaved 16-bit little-endian
/// stereo, regardless of whether SCK delivered planar or interleaved float32
/// data and regardless of its channel count. A mono source is duplicated into
/// both channels. The output must always be stereo: that is what the RDPSND
/// `AudioFormat` advertises, and a channel-count mismatch makes the client
/// play back at the wrong frame rate (audible as drift and lowered pitch).
#[cfg(target_os = "macos")]
fn float_list_to_pcm16_stereo(list: &screencapturekit::cm::AudioBufferList) -> Vec<u8> {
    let Some(first) = list.get(0) else {
        return Vec::new();
    };

    if list.num_buffers() >= 2 {
        // Planar: one mono buffer per channel. Channel 0 -> L, channel 1 -> R.
        let left = first.data();
        let right = list.get(1).map(|b| b.data()).unwrap_or(left);
        return planar_pair_to_stereo_i16(left, right);
    }

    // Single buffer: interleaved across `number_channels`, or mono.
    match first.number_channels {
        0 | 1 => mono_to_stereo_i16(first.data()),
        n => interleaved_to_stereo_i16(first.data(), n as usize),
    }
}

#[cfg(target_os = "macos")]
fn read_f32_le(b: &[u8]) -> f32 {
    f32::from_bits(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Duplicate each mono float32 sample into both stereo channels.
#[cfg(target_os = "macos")]
fn mono_to_stereo_i16(bytes: &[u8]) -> Vec<u8> {
    // frames*4 bytes in (mono f32) -> frames*4 bytes out (stereo i16).
    let mut out = Vec::with_capacity(bytes.len());
    for chunk in bytes.chunks_exact(4) {
        let s = float_to_i16(read_f32_le(chunk)).to_le_bytes();
        out.extend_from_slice(&s);
        out.extend_from_slice(&s);
    }
    out
}

/// Zip two planar float32 channels into interleaved stereo i16.
#[cfg(target_os = "macos")]
fn planar_pair_to_stereo_i16(left: &[u8], right: &[u8]) -> Vec<u8> {
    let frames = left.len().min(right.len()) / 4;
    let mut out = Vec::with_capacity(frames * 4);
    for f in 0..frames {
        let off = f * 4;
        let l = float_to_i16(read_f32_le(&left[off..off + 4]));
        let r = float_to_i16(read_f32_le(&right[off..off + 4]));
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    out
}

/// Take the first two channels of an interleaved float32 buffer as stereo i16.
/// Only called with `channels >= 2`.
#[cfg(target_os = "macos")]
fn interleaved_to_stereo_i16(bytes: &[u8], channels: usize) -> Vec<u8> {
    let frame_bytes = channels * 4;
    if frame_bytes == 0 {
        return Vec::new();
    }
    let frames = bytes.len() / frame_bytes;
    let mut out = Vec::with_capacity(frames * 4);
    for f in 0..frames {
        let base = f * frame_bytes;
        let l = float_to_i16(read_f32_le(&bytes[base..base + 4]));
        let r = float_to_i16(read_f32_le(&bytes[base + 4..base + 8]));
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    out
}

#[cfg(target_os = "macos")]
fn float_to_i16(v: f32) -> i16 {
    let clamped = v.clamp(-1.0, 1.0);
    (clamped * 32767.0).round() as i16
}
