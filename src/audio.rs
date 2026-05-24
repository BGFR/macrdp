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
use ironrdp_server::{AudioWave, ServerEvent, ServerEventSender, SoundServerFactory};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

// ScreenCaptureKit only honors 8000/16000/24000/48000 Hz, so we capture at
// 48 kHz. The advertised RDPSND format, however, is 44.1 kHz: Windows audio
// endpoints are commonly 44.1 native, and feeding clients at that rate lets
// them play directly without internal resampling, which is what was causing
// the ~20% over-feed / drift on mstsc. The capture loop resamples 48 -> 44.1
// before handing PCM to the client.
const SCK_SAMPLE_RATE: u32 = 48000;
const SAMPLE_RATE: u32 = 44100;
const CHANNELS: u16 = 2;
const BITS_PER_SAMPLE: u16 = 16;

type Sender = Arc<Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>>;
type AudioSender = Arc<Mutex<Option<mpsc::Sender<AudioWave>>>>;

#[derive(Debug)]
pub struct MacRdpsnd {
    sender: Sender,
    /// Dedicated bounded channel for Wave PDUs. Set by ironrdp-server
    /// via `set_audio_sender`. When present, the capture loop sends
    /// every Wave directly here, bypassing the unified `ServerEvent`
    /// stream. The server's `dispatch_audio` task is the sole consumer,
    /// independent of the inbound-PDU and outbound-event dispatch
    /// branches that share `Mutex<Self>` — so a sustained inbound
    /// cliprdr stream (e.g., large `--lazy-paste` Windows→Mac transfer)
    /// no longer starves audio output.
    audio_sender: AudioSender,
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
            audio_sender: Arc::new(Mutex::new(None)),
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
            audio_sender: self.audio_sender.clone(),
            generation: self.generation.clone(),
            my_gen: 0,
            formats: vec![pcm_format()],
        })
    }

    fn set_audio_sender(&mut self, audio_sender: mpsc::Sender<AudioWave>) {
        *self.audio_sender.lock().unwrap() = Some(audio_sender);
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
    audio_sender: AudioSender,
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
        let audio_sender = self.audio_sender.clone();
        let generation = self.generation.clone();
        let my_gen = self.my_gen;
        tokio::spawn(async move {
            if let Err(e) = capture_loop(sender, audio_sender, generation, my_gen).await {
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
    audio_sender: AudioSender,
    generation: Arc<AtomicU64>,
    my_gen: u64,
) -> anyhow::Result<()> {
    use anyhow::{anyhow, Context};
    use rubato::Resampler;
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
        .with_sample_rate(SCK_SAMPLE_RATE as i32)
        .with_channel_count(CHANNELS as i32);

    // SCK only delivers 8/16/24/48 kHz. We capture at 48 and resample to the
    // advertised SAMPLE_RATE (44.1 kHz) so the client plays at its native rate
    // without internal resampling. Chunk size matches a typical SCK audio
    // buffer (~21 ms at 48 kHz) — input is buffered until we have a full
    // chunk, then fed to the resampler.
    const RESAMPLE_CHUNK: usize = 1024;
    let mut resampler = rubato::FftFixedIn::<f32>::new(
        SCK_SAMPLE_RATE as usize,
        SAMPLE_RATE as usize,
        RESAMPLE_CHUNK,
        1,
        CHANNELS as usize,
    )
    .map_err(|e| anyhow!("rubato resampler init: {e}"))?;
    let mut input_buf: [Vec<f32>; 2] = [
        Vec::with_capacity(RESAMPLE_CHUNK * 2),
        Vec::with_capacity(RESAMPLE_CHUNK * 2),
    ];
    // Reused per resampler invocation so the hot path doesn't allocate two
    // Vecs per chunk (`drain(..N).collect()` did at ~46 chunks/sec).
    let mut chunk: [Vec<f32>; 2] = [vec![0.0; RESAMPLE_CHUNK], vec![0.0; RESAMPLE_CHUNK]];

    // Sender is lazily resolved on first emit, then cached. Resolving at the
    // top of capture_loop instead breaks Microsoft Remote Desktop for Mac:
    // that client appears to call `set_sender` *after* `start()`, so an
    // up-front grab returns None and exits silently. The old per-emit lock
    // worked because SCK's first sample takes ~21 ms to arrive, leaving a
    // window for set_sender to populate the Mutex. Lazy-resolve here gives
    // the same robustness without paying for a lock on every wave.
    let mut s: Option<mpsc::UnboundedSender<ServerEvent>> = None;
    let mut audio_s: Option<mpsc::Sender<AudioWave>> = None;

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

    // SCK delivery-rate measurement. The vendor server's audio-lag model
    // assumes one Wave = `WAVE_MS = 1024/48 = 21.33 ms` of real audio,
    // which is only true if SCK actually delivers at 48 kHz. Some
    // configurations (system audio rate ≠ requested capture rate) cause
    // SCK to deliver samples at a different effective rate while still
    // *reporting* 48 kHz in the format description, which manifests as
    // a steady-state audio-shipped-vs-wall-clock deficit that the resync
    // mechanism papers over every few seconds. Logging measured arrival
    // rate over a wall-clock window tells us empirically whether that's
    // the bug. Enable with `RUST_LOG=macrdp::audio=debug`.
    let mut samples_received: u64 = 0;
    let mut last_rate_log = std::time::Instant::now();
    // Output-side counters. If output samples don't match 44.1 kHz × elapsed
    // (after warm-up), the resampler is producing less audio than it should.
    // If output samples match but wave count is below 46.875/sec, average
    // wave size is bigger than the server's hardcoded WAVE_MS expects.
    let mut output_samples: u64 = 0;
    let mut waves_emitted: u64 = 0;

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
                if rate != 0.0 && (rate - f64::from(SCK_SAMPLE_RATE)).abs() > 1.0 {
                    warn!(
                        delivered = rate,
                        requested = SCK_SAMPLE_RATE,
                        "SCK audio rate differs from what we requested; \
                         the 48->44.1 resampler assumes 48 kHz input"
                    );
                }
            }
        }

        let Some(list) = sample.audio_buffer_list() else {
            continue;
        };

        // SCK delivers float32 PCM as planar (one buffer per channel) or a
        // single interleaved buffer. Normalize to planar stereo f32 at the
        // SCK rate, accumulate into the resampler input buffer, then emit
        // resampled chunks as interleaved 16-bit stereo at SAMPLE_RATE.
        let (in_left, in_right) = float_list_to_planar_f32_stereo(&list);
        if in_left.is_empty() {
            continue;
        }
        input_buf[0].extend_from_slice(&in_left);
        input_buf[1].extend_from_slice(&in_right);

        // Track actual delivery rate. One sample = one frame (per channel),
        // so left-channel count is what we compare against SCK_SAMPLE_RATE.
        samples_received += in_left.len() as u64;
        if last_rate_log.elapsed() >= std::time::Duration::from_secs(5) {
            let elapsed = last_rate_log.elapsed().as_secs_f64();
            let measured_in = samples_received as f64 / elapsed;
            let measured_out = output_samples as f64 / elapsed;
            let measured_waves = waves_emitted as f64 / elapsed;
            let expected_in = f64::from(SCK_SAMPLE_RATE);
            let expected_out = f64::from(SAMPLE_RATE);
            let expected_waves = expected_in / RESAMPLE_CHUNK as f64;
            let avg_wave_samples = if waves_emitted > 0 {
                output_samples as f64 / waves_emitted as f64
            } else {
                0.0
            };
            // Server's vendored audio-lag model assumes each wave is exactly
            // 1024/48 = 21.33 ms. If avg_wave_samples / SAMPLE_RATE * 1000
            // differs from that, the model drifts.
            let avg_wave_ms = if waves_emitted > 0 {
                avg_wave_samples / f64::from(SAMPLE_RATE) * 1000.0
            } else {
                0.0
            };
            debug!(
                in_rate = measured_in,
                in_expected = expected_in,
                out_rate = measured_out,
                out_expected = expected_out,
                waves_per_sec = measured_waves,
                waves_expected = expected_waves,
                avg_wave_samples,
                avg_wave_ms,
                model_wave_ms = 1024.0 / 48.0,
                "audio capture rates"
            );
            samples_received = 0;
            output_samples = 0;
            waves_emitted = 0;
            last_rate_log = std::time::Instant::now();
        }

        while input_buf[0].len() >= RESAMPLE_CHUNK && input_buf[1].len() >= RESAMPLE_CHUNK {
            chunk[0].copy_from_slice(&input_buf[0][..RESAMPLE_CHUNK]);
            chunk[1].copy_from_slice(&input_buf[1][..RESAMPLE_CHUNK]);
            let resampled = resampler
                .process(&chunk, None)
                .map_err(|e| anyhow!("rubato resample: {e}"))?;
            input_buf[0].drain(..RESAMPLE_CHUNK);
            input_buf[1].drain(..RESAMPLE_CHUNK);
            // Per-call output sample count (one channel; planar so all
            // channels have the same len). Counted before pcm.is_empty so
            // we capture empties too.
            output_samples += resampled[0].len() as u64;
            let pcm = planar_f32_to_interleaved_i16(&resampled);
            if pcm.is_empty() {
                continue;
            }
            waves_emitted += 1;

            let ts_ms = start_instant.elapsed().as_millis() as u32;

            // Lazy resolve both senders on first need. If set_sender /
            // set_audio_sender haven't populated their Mutexes yet, retry
            // next iteration — at 48 kHz / 1024 samples this is ~21 ms
            // later.
            if audio_s.is_none() {
                audio_s = audio_sender.lock().unwrap().clone();
            }
            if let Some(audio_ref) = audio_s.as_ref() {
                // Bounded channel: send().await applies backpressure if
                // the dispatch_audio task is behind, but if we're at
                // capacity we'd rather drop this wave at the SCK level
                // than block the capture loop (which would back up the
                // SCK ring buffer and lose newer audio anyway). Use
                // try_send: on Full, log+drop; on Closed, exit.
                match audio_ref.try_send((pcm, ts_ms)) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        debug!("audio channel full; dropping wave (dispatch_audio behind)");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                }
            } else {
                // Audio sender not wired (older server build / non-macrdp
                // host). Fall back to the unified ServerEvent channel.
                if s.is_none() {
                    s = sender.lock().unwrap().clone();
                }
                let Some(s_ref) = s.as_ref() else { continue };
                if s_ref
                    .send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(pcm, ts_ms)))
                    .is_err()
                {
                    return Ok(());
                }
            }
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

/// Normalize an SCK audio buffer list to planar stereo `f32` at the SCK rate.
/// Handles both planar (one buffer per channel) and interleaved (single buffer
/// with `number_channels` channels) layouts; a mono source is duplicated. The
/// result feeds the rubato resampler.
#[cfg(target_os = "macos")]
fn float_list_to_planar_f32_stereo(
    list: &screencapturekit::cm::AudioBufferList,
) -> (Vec<f32>, Vec<f32>) {
    let Some(first) = list.get(0) else {
        return (Vec::new(), Vec::new());
    };

    if list.num_buffers() >= 2 {
        // Planar: one mono buffer per channel. Channel 0 -> L, channel 1 -> R.
        let left = bytes_to_f32(first.data());
        let right = list
            .get(1)
            .map(|b| bytes_to_f32(b.data()))
            .unwrap_or_else(|| left.clone());
        let n = left.len().min(right.len());
        return (left[..n].to_vec(), right[..n].to_vec());
    }

    // Single buffer: interleaved across `number_channels`, or mono.
    match first.number_channels {
        0 | 1 => {
            let mono = bytes_to_f32(first.data());
            (mono.clone(), mono)
        }
        n => deinterleave_first_two_channels(first.data(), n as usize),
    }
}

/// Pull the first two channels out of an interleaved float32 buffer.
#[cfg(target_os = "macos")]
fn deinterleave_first_two_channels(bytes: &[u8], channels: usize) -> (Vec<f32>, Vec<f32>) {
    let frame_bytes = channels * 4;
    if frame_bytes == 0 {
        return (Vec::new(), Vec::new());
    }
    let frames = bytes.len() / frame_bytes;
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * frame_bytes;
        left.push(read_f32_le(&bytes[base..base + 4]));
        right.push(read_f32_le(&bytes[base + 4..base + 8]));
    }
    (left, right)
}

#[cfg(target_os = "macos")]
fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks_exact(4).map(read_f32_le).collect()
}

#[cfg(target_os = "macos")]
fn read_f32_le(b: &[u8]) -> f32 {
    f32::from_bits(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Interleave two planar float32 channels into a 16-bit LE stereo PCM payload.
#[cfg(target_os = "macos")]
fn planar_f32_to_interleaved_i16(planar: &[Vec<f32>]) -> Vec<u8> {
    if planar.len() < 2 {
        return Vec::new();
    }
    let frames = planar[0].len().min(planar[1].len());
    let mut out = Vec::with_capacity(frames * 4);
    for (lf, rf) in planar[0].iter().zip(planar[1].iter()).take(frames) {
        out.extend_from_slice(&float_to_i16(*lf).to_le_bytes());
        out.extend_from_slice(&float_to_i16(*rf).to_le_bytes());
    }
    out
}

#[cfg(target_os = "macos")]
fn float_to_i16(v: f32) -> i16 {
    let clamped = v.clamp(-1.0, 1.0);
    (clamped * 32767.0).round() as i16
}
