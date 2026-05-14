//! Forward macOS system audio to the RDP client via the RDPSND SVC.
//!
//! We tap the same display ScreenCaptureKit gives us video for, but with a
//! second SCStream configured for audio output (`captures_audio = true`,
//! `SCStreamOutputType::Audio`). SCK delivers 32-bit float PCM at the
//! configured sample rate; we convert to 16-bit signed PCM interleaved and
//! ship via `RdpsndServerMessage::Wave`.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use ironrdp_rdpsnd::pdu::{AudioFormat, ClientAudioFormatPdu, WaveFormat};
use ironrdp_rdpsnd::server::{RdpsndServerHandler, RdpsndServerMessage};
use ironrdp_server::{ServerEvent, ServerEventSender, SoundServerFactory};
use tokio::sync::mpsc;
use tracing::{debug, warn};

const SAMPLE_RATE: u32 = 44100;
const CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 16;

type Sender = Arc<Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>>;

#[derive(Debug)]
pub struct MacRdpsnd {
    sender: Sender,
}

impl MacRdpsnd {
    pub fn new() -> Self {
        Self {
            sender: Arc::new(Mutex::new(None)),
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
            running: Arc::new(AtomicBool::new(false)),
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
    running: Arc<AtomicBool>,
    formats: Vec<AudioFormat>,
}

impl RdpsndServerHandler for MacRdpsndBackend {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn start(&mut self, client_format: &ClientAudioFormatPdu) -> Option<u16> {
        // Find our PCM format in the client's accepted list and use its index.
        let target = pcm_format();
        let index = client_format.formats.iter().position(|f| {
            f.format == target.format
                && f.n_channels == target.n_channels
                && f.n_samples_per_sec == target.n_samples_per_sec
                && f.bits_per_sample == target.bits_per_sample
        })?;

        if self.running.swap(true, Ordering::SeqCst) {
            // Already running; reuse the existing capture task.
            return Some(index as u16);
        }

        let sender = self.sender.clone();
        let running = self.running.clone();
        tokio::spawn(async move {
            if let Err(e) = capture_loop(sender, running.clone()).await {
                warn!("audio capture loop ended: {e}");
            }
            running.store(false, Ordering::SeqCst);
        });
        Some(index as u16)
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

#[cfg(target_os = "macos")]
async fn capture_loop(sender: Sender, running: Arc<AtomicBool>) -> anyhow::Result<()> {
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

    let stream = AsyncSCStream::new(&filter, &config, 8, SCStreamOutputType::Audio);
    stream
        .start_capture()
        .map_err(|e| anyhow!("audio start_capture: {e:?}"))?;
    debug!("audio capture started");

    let start_instant = std::time::Instant::now();

    loop {
        if !running.load(Ordering::SeqCst) {
            break;
        }
        let Some(sample) = stream.next().await else {
            break;
        };
        let Some(list) = sample.audio_buffer_list() else {
            continue;
        };

        // SCK delivers float32 PCM, normally one buffer per channel (planar)
        // OR one interleaved buffer. Normalize both to interleaved i16le.
        let pcm = float_list_to_pcm16_interleaved(&list);
        if pcm.is_empty() {
            continue;
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
async fn capture_loop(_sender: Sender, _running: Arc<AtomicBool>) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn float_list_to_pcm16_interleaved(list: &screencapturekit::cm::AudioBufferList) -> Vec<u8> {
    let num_buffers = list.num_buffers();
    if num_buffers == 0 {
        return Vec::new();
    }
    // Planar case: num_buffers == channels, each buffer one channel of floats.
    // Interleaved case: num_buffers == 1, that buffer has channels*frames floats.
    let mut planar: Vec<&[u8]> = Vec::with_capacity(num_buffers);
    for i in 0..num_buffers {
        if let Some(b) = list.get(i) {
            planar.push(b.data());
        }
    }
    if planar.is_empty() {
        return Vec::new();
    }

    let bytes_per_sample = 4; // float32
    if num_buffers == 1 {
        // Already interleaved (or mono). Convert in order.
        return floats_to_pcm16(planar[0]);
    }

    // Planar: zip channels frame-by-frame.
    let frames = planar[0].len() / bytes_per_sample;
    let mut out = Vec::with_capacity(frames * num_buffers * 2);
    for f in 0..frames {
        for ch in &planar {
            let off = f * bytes_per_sample;
            if off + bytes_per_sample > ch.len() {
                continue;
            }
            let bits = u32::from_le_bytes([ch[off], ch[off + 1], ch[off + 2], ch[off + 3]]);
            let v = f32::from_bits(bits);
            out.extend_from_slice(&float_to_i16(v).to_le_bytes());
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn floats_to_pcm16(bytes: &[u8]) -> Vec<u8> {
    let n = bytes.len() / 4;
    let mut out = Vec::with_capacity(n * 2);
    for chunk in bytes.chunks_exact(4) {
        let bits = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let v = f32::from_bits(bits);
        out.extend_from_slice(&float_to_i16(v).to_le_bytes());
    }
    out
}

#[cfg(target_os = "macos")]
fn float_to_i16(v: f32) -> i16 {
    let clamped = v.clamp(-1.0, 1.0);
    (clamped * 32767.0).round() as i16
}
