//! AudioToolbox AAC-LC encoder for the RDPSND `WAVE_FORMAT_AAC_MS` (0xA106)
//! audio path (opt-in via `--enable-aac`).
//!
//! Conventions follow `src/auth.rs::pam_impl` and `src/videotoolbox.rs`:
//! direct `extern "C"`, no wrapper crate — the call surface (an
//! `AudioConverter`) is small. macOS-only.
//!
//! Wire-format note (MS-RDPEA + verified against FreeRDP's encoder). The
//! payload for `WAVE_FORMAT_AAC_MS` is **raw AAC-LC access units**: object
//! type 2 (AAC-LC), one access unit = 1024 frames, NO ADTS/LATM transport
//! headers and NO in-band AudioSpecificConfig. The server advertises the AAC
//! `AUDIO_FORMAT` with `cbSize = 0` (no HEAACWAVEINFO blob), so the decoder
//! derives its config from the WAVEFORMATEX sample-rate / channel fields
//! alone. AudioToolbox's `kAudioFormatMPEG4AAC` encoder emits exactly this —
//! one raw AU per output packet — and the magic cookie / ASC it can produce is
//! intentionally never sent on the wire.

#![cfg(target_os = "macos")]

use std::ffi::c_void;

use anyhow::{anyhow, bail, Result};

/// Sentinel `OSStatus` the input proc returns when the current batch is
/// exhausted. Returning `noErr` with zero packets instead would make
/// `AudioConverter` treat the stream as *ended* and refuse all further input
/// (so only the first batch ever encodes). A non-zero status signals "no more
/// data right now" without finalizing the stream; `encode` recognizes it and
/// breaks cleanly, leaving the converter's internal buffer intact for the next
/// call. Value is an arbitrary distinctive non-zero code ("NoDt").
const INPUT_EXHAUSTED: i32 = i32::from_be_bytes(*b"NoDt");

/// Frames per AAC-LC access unit. Fixed by the codec; also the unit the
/// `--enable-aac` wave timestamps / durations are computed against.
pub const FRAMES_PER_PACKET: usize = 1024;

/// One AAC-LC access unit (one output packet from the encoder) duration in
/// ms at the given sample rate. Used to tag each `AudioWave` so the vendored
/// audio-lag model gets a real playback time instead of deriving it from the
/// compressed byte length.
pub fn packet_duration_ms(sample_rate: u32) -> f64 {
    FRAMES_PER_PACKET as f64 / f64::from(sample_rate) * 1000.0
}

/// AAC-LC encoder wrapping a single `AudioConverter`. Input is interleaved
/// 16-bit signed stereo PCM at `sample_rate`; output is zero or more raw
/// AAC-LC access units per `encode` call (the converter buffers internally
/// until it has a full 1024-frame packet, so early calls may return empty).
///
/// Not `Send`/`Sync`: the converter is created and used entirely on the
/// dedicated audio capture thread (`MacRdpsndBackend::start`), so it never
/// crosses threads. Disposed on `Drop`.
pub struct AacEncoder {
    converter: ffi::AudioConverterRef,
    channels: u16,
    sample_rate: u32,
}

impl AacEncoder {
    /// Build an AAC-LC encoder. `bitrate` is the target in bits/sec
    /// (e.g. 128_000). Fails with a contextual error naming the failing
    /// `AudioConverter` call so a missing/blocked AudioToolbox surface is
    /// diagnosable.
    pub fn new(sample_rate: u32, channels: u16, bitrate: u32) -> Result<Self> {
        let bytes_per_frame = u32::from(channels) * 2; // 16-bit samples

        // Source: interleaved signed 16-bit packed LPCM.
        let src = ffi::AudioStreamBasicDescription {
            m_sample_rate: f64::from(sample_rate),
            m_format_id: ffi::K_AUDIO_FORMAT_LINEAR_PCM,
            m_format_flags: ffi::K_LINEAR_PCM_FLAG_IS_SIGNED_INTEGER
                | ffi::K_LINEAR_PCM_FLAG_IS_PACKED,
            m_bytes_per_packet: bytes_per_frame,
            m_frames_per_packet: 1,
            m_bytes_per_frame: bytes_per_frame,
            m_channels_per_frame: u32::from(channels),
            m_bits_per_channel: 16,
            m_reserved: 0,
        };

        // Destination: AAC. Leaving most fields 0 lets AudioToolbox fill in
        // the AAC-LC defaults; only the rate, channel count and 1024-frame
        // packet size are meaningful inputs.
        let dst = ffi::AudioStreamBasicDescription {
            m_sample_rate: f64::from(sample_rate),
            m_format_id: ffi::K_AUDIO_FORMAT_MPEG4_AAC,
            m_format_flags: 0,
            m_bytes_per_packet: 0,
            m_frames_per_packet: FRAMES_PER_PACKET as u32,
            m_bytes_per_frame: 0,
            m_channels_per_frame: u32::from(channels),
            m_bits_per_channel: 0,
            m_reserved: 0,
        };

        let mut converter: ffi::AudioConverterRef = std::ptr::null_mut();
        let status = unsafe { ffi::AudioConverterNew(&src, &dst, &mut converter) };
        if status != 0 || converter.is_null() {
            bail!(
                "AudioConverterNew failed (OSStatus {status}); AAC encode unavailable. \
                 Falling back to PCM is automatic if the client also advertised PCM."
            );
        }

        let encoder = Self {
            converter,
            channels,
            sample_rate,
        };

        // Set the target bitrate. Non-fatal if it's rejected — the converter
        // still encodes at its default rate — but we surface it so an
        // out-of-range `--aac-bitrate` is visible rather than silently ignored.
        let br: u32 = bitrate;
        let st = unsafe {
            ffi::AudioConverterSetProperty(
                converter,
                ffi::K_AUDIO_CONVERTER_ENCODE_BIT_RATE,
                std::mem::size_of::<u32>() as u32,
                std::ptr::addr_of!(br).cast(),
            )
        };
        if st != 0 {
            tracing::warn!(
                requested_bitrate = bitrate,
                os_status = st,
                "AudioConverter rejected the requested AAC bitrate; using encoder default"
            );
        }

        Ok(encoder)
    }

    /// Encode one batch of interleaved 16-bit stereo PCM. Returns the raw
    /// AAC-LC access units that became available (possibly empty: the encoder
    /// buffers input internally until it has a full 1024-frame packet, and has
    /// a one-time priming delay at stream start).
    ///
    /// `pcm` length must be a multiple of `channels * 2` bytes; any trailing
    /// partial frame is dropped (it never happens in practice — the resampler
    /// emits whole frames).
    pub fn encode(&mut self, pcm: &[u8]) -> Result<Vec<Vec<u8>>> {
        let bytes_per_frame = (usize::from(self.channels)) * 2;
        if bytes_per_frame == 0 {
            return Ok(Vec::new());
        }
        let usable = pcm.len() - (pcm.len() % bytes_per_frame);
        if usable == 0 {
            return Ok(Vec::new());
        }

        // Context the input proc reads from. Lives on the stack for the whole
        // `AudioConverterFillComplexBuffer` loop; the converter copies the PCM
        // into its own ring during each proc callback, so the slice only has
        // to stay valid across these synchronous calls.
        let mut ctx = ffi::InputCtx {
            data: pcm.as_ptr(),
            len: usable,
            pos: 0,
            bytes_per_frame,
            channels: u32::from(self.channels),
        };

        let mut packets: Vec<Vec<u8>> = Vec::new();
        // AAC-LC stereo at our bitrates is well under 1 KiB/packet; 4 KiB is a
        // generous ceiling reused for every pull.
        let mut out = vec![0u8; 4096];

        loop {
            let mut num_packets: u32 = 1;
            let mut buffer_list = ffi::AudioBufferList {
                m_number_buffers: 1,
                m_buffers: [ffi::AudioBuffer {
                    m_number_channels: u32::from(self.channels),
                    m_data_byte_size: out.len() as u32,
                    m_data: out.as_mut_ptr().cast(),
                }],
            };
            let mut pkt_desc = ffi::AudioStreamPacketDescription {
                m_start_offset: 0,
                m_variable_frames_in_packet: 0,
                m_data_byte_size: 0,
            };

            let status = unsafe {
                ffi::AudioConverterFillComplexBuffer(
                    self.converter,
                    input_data_proc,
                    std::ptr::addr_of_mut!(ctx).cast(),
                    &mut num_packets,
                    &mut buffer_list,
                    &mut pkt_desc,
                )
            };
            if status == INPUT_EXHAUSTED {
                // Our input proc signalled "no more data this batch". The
                // converter has consumed everything from `ctx` and is holding
                // < one packet internally for next time. Done for this batch.
                break;
            }
            if status != 0 {
                return Err(anyhow!(
                    "AudioConverterFillComplexBuffer failed (OSStatus {status})"
                ));
            }
            if num_packets == 0 {
                break;
            }
            let size = pkt_desc.m_data_byte_size as usize;
            let size = if size == 0 {
                buffer_list.m_buffers[0].m_data_byte_size as usize
            } else {
                size
            };
            packets.push(out[..size].to_vec());
        }

        Ok(packets)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        if !self.converter.is_null() {
            unsafe { ffi::AudioConverterDispose(self.converter) };
            self.converter = std::ptr::null_mut();
        }
    }
}

/// `AudioConverterComplexInputDataProc`: feed the converter from the
/// `InputCtx` cursor. Hands back a slice of the pending PCM (up to the number
/// of frames requested) and advances the cursor; returns 0 frames once the
/// batch is exhausted, which ends the `FillComplexBuffer` pull loop.
extern "C" fn input_data_proc(
    _converter: ffi::AudioConverterRef,
    io_number_data_packets: *mut u32,
    io_data: *mut ffi::AudioBufferList,
    _out_packet_description: *mut *mut ffi::AudioStreamPacketDescription,
    in_user_data: *mut c_void,
) -> i32 {
    // SAFETY: `in_user_data` is the `&mut InputCtx` we passed to
    // `FillComplexBuffer`; `io_data`/`io_number_data_packets` are valid
    // out-params owned by the converter for the duration of the call.
    unsafe {
        let ctx = &mut *(in_user_data as *mut ffi::InputCtx);
        let frames_left = (ctx.len - ctx.pos) / ctx.bytes_per_frame;
        let requested = (*io_number_data_packets) as usize;
        let give = requested.min(frames_left);

        let buf = &mut (*io_data).m_buffers[0];
        if give == 0 {
            // Batch exhausted. Return a sentinel (NOT noErr-with-0-packets,
            // which AudioConverter reads as end-of-stream) so the converter
            // keeps its buffered remainder for the next `encode` call.
            *io_number_data_packets = 0;
            buf.m_number_channels = ctx.channels;
            buf.m_data_byte_size = 0;
            buf.m_data = std::ptr::null_mut();
            return INPUT_EXHAUSTED;
        }

        let byte_off = ctx.pos;
        let bytes = give * ctx.bytes_per_frame;
        buf.m_number_channels = ctx.channels;
        buf.m_data_byte_size = bytes as u32;
        buf.m_data = ctx.data.add(byte_off) as *mut c_void;
        ctx.pos += bytes;
        *io_number_data_packets = give as u32;
        0
    }
}

mod ffi {
    use std::ffi::c_void;
    use std::os::raw::c_int;

    pub type AudioConverterRef = *mut c_void;

    // Four-char-code constants (big-endian packing of the ASCII tag).
    pub const K_AUDIO_FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
    pub const K_AUDIO_FORMAT_MPEG4_AAC: u32 = u32::from_be_bytes(*b"aac ");
    pub const K_AUDIO_CONVERTER_ENCODE_BIT_RATE: u32 = u32::from_be_bytes(*b"brat");

    // LinearPCM format flags.
    pub const K_LINEAR_PCM_FLAG_IS_SIGNED_INTEGER: u32 = 0x4;
    pub const K_LINEAR_PCM_FLAG_IS_PACKED: u32 = 0x8;

    /// `CoreAudioTypes.AudioStreamBasicDescription` (40 bytes, `#[repr(C)]`).
    #[repr(C)]
    pub struct AudioStreamBasicDescription {
        pub m_sample_rate: f64,
        pub m_format_id: u32,
        pub m_format_flags: u32,
        pub m_bytes_per_packet: u32,
        pub m_frames_per_packet: u32,
        pub m_bytes_per_frame: u32,
        pub m_channels_per_frame: u32,
        pub m_bits_per_channel: u32,
        pub m_reserved: u32,
    }

    #[repr(C)]
    pub struct AudioBuffer {
        pub m_number_channels: u32,
        pub m_data_byte_size: u32,
        pub m_data: *mut c_void,
    }

    /// `AudioBufferList` with a single inline buffer — all we ever use for
    /// interleaved stereo (or mono) audio.
    #[repr(C)]
    pub struct AudioBufferList {
        pub m_number_buffers: u32,
        pub m_buffers: [AudioBuffer; 1],
    }

    #[repr(C)]
    pub struct AudioStreamPacketDescription {
        pub m_start_offset: i64,
        pub m_variable_frames_in_packet: u32,
        pub m_data_byte_size: u32,
    }

    /// Cursor handed to the input proc; points into the caller's PCM batch.
    pub struct InputCtx {
        pub data: *const u8,
        pub len: usize,
        pub pos: usize,
        pub bytes_per_frame: usize,
        pub channels: u32,
    }

    pub type AudioConverterComplexInputDataProc = extern "C" fn(
        converter: AudioConverterRef,
        io_number_data_packets: *mut u32,
        io_data: *mut AudioBufferList,
        out_packet_description: *mut *mut AudioStreamPacketDescription,
        in_user_data: *mut c_void,
    ) -> c_int;

    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C" {
        pub fn AudioConverterNew(
            in_source_format: *const AudioStreamBasicDescription,
            in_destination_format: *const AudioStreamBasicDescription,
            out_audio_converter: *mut AudioConverterRef,
        ) -> c_int;

        pub fn AudioConverterDispose(in_audio_converter: AudioConverterRef) -> c_int;

        pub fn AudioConverterSetProperty(
            in_audio_converter: AudioConverterRef,
            in_property_id: u32,
            in_property_data_size: u32,
            in_property_data: *const c_void,
        ) -> c_int;

        pub fn AudioConverterFillComplexBuffer(
            in_audio_converter: AudioConverterRef,
            in_input_data_proc: AudioConverterComplexInputDataProc,
            in_input_data_proc_user_data: *mut c_void,
            io_output_data_packet_size: *mut u32,
            out_output_data: *mut AudioBufferList,
            out_packet_description: *mut AudioStreamPacketDescription,
        ) -> c_int;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encoding a steady stream of PCM must yield raw AAC-LC access units at
    /// roughly the 1024-frame cadence. We feed ~1 s of a 440 Hz sine at
    /// 44.1 kHz stereo and assert we get close to 44100/1024 ≈ 43 packets,
    /// all non-empty. This exercises the real AudioToolbox encoder.
    #[test]
    fn encodes_sine_to_aac_access_units() {
        let sample_rate = 44_100u32;
        let mut enc = AacEncoder::new(sample_rate, 2, 128_000).expect("AAC encoder");

        let total_frames = sample_rate as usize; // ~1 second
        let mut pcm = Vec::with_capacity(total_frames * 4);
        for n in 0..total_frames {
            let t = n as f32 / sample_rate as f32;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            let v = (s * 16_000.0) as i16;
            pcm.extend_from_slice(&v.to_le_bytes()); // L
            pcm.extend_from_slice(&v.to_le_bytes()); // R
        }

        // Feed in resampler-sized chunks (~941 frames) to mimic the real path.
        let mut packets = Vec::new();
        for chunk in pcm.chunks(941 * 4) {
            packets.extend(enc.encode(chunk).expect("encode"));
        }

        assert!(!packets.is_empty(), "expected AAC access units, got none");
        assert!(
            packets.iter().all(|p| !p.is_empty()),
            "empty AAC packet emitted"
        );
        // Allow slack for encoder priming/buffering at stream start/end.
        let expected = total_frames / FRAMES_PER_PACKET;
        assert!(
            packets.len() + 5 >= expected,
            "too few packets: got {}, expected ~{expected}",
            packets.len()
        );
    }

    #[test]
    fn packet_duration_is_about_23ms_at_44100() {
        let d = packet_duration_ms(44_100);
        assert!((d - 23.22).abs() < 0.1, "unexpected packet duration {d}");
    }
}
