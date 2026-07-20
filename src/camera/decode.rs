//! Camera H.264 decode (Phase 2b) — VideoToolbox `VTDecompressionSession`.
//!
//! The MS-RDPECAM sample stream arrives as H.264 **Annex-B** access units with
//! in-band SPS/PPS (MS-RDPECAM §2.2.3.8.1). This decodes them to `CVPixelBuffer`s
//! via VideoToolbox — the reverse of `src/videotoolbox.rs` (which *encodes* the
//! screen). Phase 2 just confirms decode works (logs each decoded frame); Phase 3
//! hands the `CVPixelBuffer`s to a CoreMediaIO Camera Extension.
//!
//! VideoToolbox wants **AVCC** sample data (length-prefixed NALs) + the SPS/PPS
//! carried out-of-band in a `CMVideoFormatDescription` — so we parse the Annex-B
//! start codes, pull SPS (NAL type 7) / PPS (type 8) to build the format
//! description once, and re-frame each access unit's slice NALs as AVCC before
//! feeding `VTDecompressionSessionDecodeFrame`. All FFI is quarantined here.

use std::ffi::c_void;
use std::os::raw::c_int;
use std::ptr;
use std::time::Instant;

use anyhow::{anyhow, Result};

use super::feed::CameraFeed;

type OsStatus = i32;
type CfTypeRef = *const c_void;
type CmFormatDescriptionRef = CfTypeRef;
type CmBlockBufferRef = CfTypeRef;
type CmSampleBufferRef = CfTypeRef;
type CvImageBufferRef = CfTypeRef;
type VtDecompressionSessionRef = CfTypeRef;

/// `CMTime` (`<CoreMedia/CMTime.h>`) — passed by value to the output callback.
#[repr(C)]
#[derive(Clone, Copy)]
struct CmTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

/// `VTDecompressionOutputCallback` — fires (synchronously by default) per frame.
type VtDecompressionOutputCallback = extern "C" fn(
    decompression_output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: OsStatus,
    info_flags: u32,
    image_buffer: CvImageBufferRef,
    presentation_time_stamp: CmTime,
    presentation_duration: CmTime,
);

#[repr(C)]
struct VtDecompressionOutputCallbackRecord {
    callback: VtDecompressionOutputCallback,
    ref_con: *mut c_void,
}

const K_CM_BLOCK_BUFFER_ASSURE_MEMORY_NOW_FLAG: u32 = 0x04;

// Each framework needs its own `#[link]`; clippy's duplicated_attributes lint
// objects to the repeated `kind = "framework"` (unavoidable) — same as videotoolbox.rs.
#[allow(clippy::duplicated_attributes)]
#[link(name = "CoreFoundation", kind = "framework")]
#[link(name = "CoreMedia", kind = "framework")]
#[link(name = "CoreVideo", kind = "framework")]
#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CfTypeRef);

    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CfTypeRef,
        parameter_set_count: usize,
        parameter_set_pointers: *const *const u8,
        parameter_set_sizes: *const usize,
        nal_unit_header_length: c_int,
        format_description_out: *mut CmFormatDescriptionRef,
    ) -> OsStatus;

    fn CMBlockBufferCreateWithMemoryBlock(
        structure_allocator: CfTypeRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CfTypeRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        block_buffer_out: *mut CmBlockBufferRef,
    ) -> OsStatus;

    fn CMBlockBufferReplaceDataBytes(
        source_bytes: *const c_void,
        destination_buffer: CmBlockBufferRef,
        offset_into_destination: usize,
        data_length: usize,
    ) -> OsStatus;

    fn CMSampleBufferCreateReady(
        allocator: CfTypeRef,
        data_buffer: CmBlockBufferRef,
        format_description: CmFormatDescriptionRef,
        num_samples: isize,
        num_sample_timing_entries: isize,
        sample_timing_array: *const c_void,
        num_sample_size_entries: isize,
        sample_size_array: *const usize,
        sample_buffer_out: *mut CmSampleBufferRef,
    ) -> OsStatus;

    fn VTDecompressionSessionCreate(
        allocator: CfTypeRef,
        video_format_description: CmFormatDescriptionRef,
        video_decoder_specification: CfTypeRef,
        destination_image_buffer_attributes: CfTypeRef,
        output_callback: *const VtDecompressionOutputCallbackRecord,
        decompression_session_out: *mut VtDecompressionSessionRef,
    ) -> OsStatus;

    fn VTDecompressionSessionDecodeFrame(
        session: VtDecompressionSessionRef,
        sample_buffer: CmSampleBufferRef,
        decode_flags: u32,
        source_frame_ref_con: *mut c_void,
        info_flags_out: *mut u32,
    ) -> OsStatus;

    fn VTDecompressionSessionInvalidate(session: VtDecompressionSessionRef);

    fn CVPixelBufferGetWidth(pixel_buffer: CvImageBufferRef) -> usize;
    fn CVPixelBufferGetHeight(pixel_buffer: CvImageBufferRef) -> usize;
    fn CVPixelBufferGetPixelFormatType(pixel_buffer: CvImageBufferRef) -> u32;
    fn CVPixelBufferLockBaseAddress(pixel_buffer: CvImageBufferRef, lock_flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pixel_buffer: CvImageBufferRef, unlock_flags: u64) -> i32;
    // NB: signatures must match src/videotoolbox.rs (same crate-wide symbols) —
    // GetBaseAddressOfPlane returns *mut c_void there, so we cast on use.
    fn CVPixelBufferGetBaseAddressOfPlane(
        pixel_buffer: CvImageBufferRef,
        plane: usize,
    ) -> *mut c_void;
    fn CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer: CvImageBufferRef, plane: usize) -> usize;
}

const K_CV_PIXEL_BUFFER_LOCK_READ_ONLY: u64 = 0x01;

/// Read the decoded Y (luma) plane of an NV12/`420v`/`420f` CVPixelBuffer:
/// returns the average luma (a video-range black frame averages ~16, a real image
/// is much higher/varied) and, for the first few frames, dumps a **grayscale PNG**
/// so the decoded picture can be seen directly. This is the definitive
/// "is the source actually black?" check — VT decoding without error only proves
/// the bitstream is valid, not that it carries a real image.
fn inspect_luma(image_buffer: CvImageBufferRef, dump_seq: Option<u64>) -> Option<f64> {
    // SAFETY: called from the output callback with a valid image buffer.
    unsafe {
        if CVPixelBufferLockBaseAddress(image_buffer, K_CV_PIXEL_BUFFER_LOCK_READ_ONLY) != 0 {
            return None;
        }
        let w = CVPixelBufferGetWidth(image_buffer);
        let h = CVPixelBufferGetHeight(image_buffer);
        let base = CVPixelBufferGetBaseAddressOfPlane(image_buffer, 0) as *const u8;
        let stride = CVPixelBufferGetBytesPerRowOfPlane(image_buffer, 0);
        if base.is_null() || w == 0 || h == 0 || stride < w {
            CVPixelBufferUnlockBaseAddress(image_buffer, K_CV_PIXEL_BUFFER_LOCK_READ_ONLY);
            return None;
        }
        // Average luma over a coarse grid (cheap; enough to tell black from image).
        let mut sum: u64 = 0;
        let mut n: u64 = 0;
        let step_y = (h / 64).max(1);
        let step_x = (w / 64).max(1);
        let mut y = 0;
        while y < h {
            let row = base.add(y * stride);
            let mut x = 0;
            while x < w {
                sum += *row.add(x) as u64;
                n += 1;
                x += step_x;
            }
            y += step_y;
        }
        let avg = if n > 0 { sum as f64 / n as f64 } else { 0.0 };

        // Grayscale PNG of the full Y plane (first few frames only).
        if let Some(seq) = dump_seq {
            let mut gray = vec![0u8; w * h];
            for row in 0..h {
                let src = base.add(row * stride);
                let dst = &mut gray[row * w..row * w + w];
                std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), w);
            }
            let path = std::env::temp_dir().join(format!(
                "macrdp-camera-frame-{}-{seq}.png",
                std::process::id()
            ));
            match image::save_buffer(&path, &gray, w as u32, h as u32, image::ColorType::L8) {
                Ok(()) => {
                    tracing::info!(path = %path.display(), "camera Phase-2b: wrote a decoded grayscale frame PNG")
                }
                Err(e) => tracing::warn!(error = %e, "camera: frame PNG write failed"),
            }
        }
        CVPixelBufferUnlockBaseAddress(image_buffer, K_CV_PIXEL_BUFFER_LOCK_READ_ONLY);
        Some(avg)
    }
}

/// Shared state the output callback writes through its ref-con. Boxed and kept
/// alive by [`H264Decoder`] for as long as the session references it.
struct CallbackState {
    decoded: u64,
    errors: u64,
    last_log: Option<Instant>,
    dumped_pngs: u64,
    /// Phase 3b: the CoreMediaIO sink feed. Each decoded frame is enqueued here so
    /// the "macrdp Camera" presents the live webcam. `None` when the camera
    /// extension isn't active (decode-only).
    feed: Option<CameraFeed>,
}

extern "C" fn decode_output(
    ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: OsStatus,
    _info_flags: u32,
    image_buffer: CvImageBufferRef,
    _pts: CmTime,
    _dur: CmTime,
) {
    if ref_con.is_null() {
        return;
    }
    // SAFETY: `ref_con` is the `Box<CallbackState>` raw pointer H264Decoder passed
    // to VTDecompressionSessionCreate; the session (and thus this callback) is
    // invalidated before the box is dropped, so it's live here.
    let state = unsafe { &mut *(ref_con as *mut CallbackState) };
    if status != 0 || image_buffer.is_null() {
        state.errors += 1;
        return;
    }
    state.decoded += 1;
    // Phase 3b: present this frame as the macrdp Camera (best-effort; drops if the
    // sink queue is full). The CVImageBuffer is IOSurface-backed → zero-copy to the
    // extension.
    if let Some(feed) = state.feed.as_mut() {
        feed.enqueue(image_buffer);
    }
    let now = Instant::now();
    if state
        .last_log
        .map(|t| now.saturating_duration_since(t).as_millis() >= 1000)
        .unwrap_or(true)
    {
        state.last_log = Some(now);
        // SAFETY: valid CVImageBuffer per the status check.
        let (w, h, fmt) = unsafe {
            (
                CVPixelBufferGetWidth(image_buffer),
                CVPixelBufferGetHeight(image_buffer),
                CVPixelBufferGetPixelFormatType(image_buffer),
            )
        };
        // FourCC of the pixel format (e.g. '420v'/'420f' NV12, 'BGRA').
        let cc = fmt.to_be_bytes();
        let cc = String::from_utf8_lossy(&cc).into_owned();
        // Inspect the actual decoded pixels — dump the first few frames as PNG so
        // the picture is directly visible, and report avg luma (≈16 = a black
        // source; higher/varied = a real image).
        // Opt-in only (MACRDP_CAMERA_DUMP=1): the luma scan locks + walks the frame
        // and the PNG dumps write to $TMPDIR, so skip both in normal operation.
        let avg_luma = if super::camera_dump_enabled() {
            let dump_seq = (state.dumped_pngs < 3).then(|| {
                state.dumped_pngs += 1;
                state.dumped_pngs
            });
            inspect_luma(image_buffer, dump_seq)
        } else {
            None
        };
        tracing::info!(
            decoded = state.decoded,
            errors = state.errors,
            width = w,
            height = h,
            pixel_format = %cc,
            avg_luma = avg_luma.map(|v| (v * 10.0).round() / 10.0),
            "camera Phase-2b: VideoToolbox DECODED a frame (Phase-2 GREEN — the redirected webcam decodes)"
        );
    }
}

/// H.264 Annex-B → VideoToolbox decoder. Lazily creates the format description +
/// session once SPS+PPS have been seen in the stream.
pub struct H264Decoder {
    width: u32,
    height: u32,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    format_desc: CmFormatDescriptionRef,
    session: VtDecompressionSessionRef,
    /// Boxed callback state; raw ptr handed to the session as its ref-con.
    cb_state: *mut CallbackState,
    warned: bool,
}

// SAFETY: the CF/VT handles are only touched from the single owning task (the DVC
// device processor's thread); we never share the decoder across threads.
unsafe impl Send for H264Decoder {}

impl H264Decoder {
    pub fn new(width: u32, height: u32, feed: Option<CameraFeed>) -> Result<Self> {
        let cb_state = Box::into_raw(Box::new(CallbackState {
            decoded: 0,
            errors: 0,
            last_log: None,
            dumped_pngs: 0,
            feed,
        }));
        Ok(Self {
            width,
            height,
            sps: None,
            pps: None,
            format_desc: ptr::null(),
            session: ptr::null(),
            cb_state,
            warned: false,
        })
    }

    /// Feed one Annex-B access unit (one frame). Extracts SPS/PPS as they appear,
    /// (re)builds the session when both are known, and decodes the slice NALs.
    pub fn decode(&mut self, annexb: &[u8]) {
        // Collect the slice NALs (as AVCC) and update SPS/PPS from this AU.
        let mut avcc: Vec<u8> = Vec::with_capacity(annexb.len() + 8);
        for nal in AnnexBNals::new(annexb) {
            if nal.is_empty() {
                continue;
            }
            let nal_type = nal[0] & 0x1f;
            match nal_type {
                7 => self.sps = Some(nal.to_vec()), // SPS
                8 => self.pps = Some(nal.to_vec()), // PPS
                9 => {}                             // AUD — skip
                _ => {
                    // Slice NAL → AVCC: 4-byte big-endian length + NAL.
                    avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    avcc.extend_from_slice(nal);
                }
            }
        }

        if self.session.is_null() {
            if let (Some(_), Some(_)) = (&self.sps, &self.pps) {
                if let Err(e) = self.build_session() {
                    if !self.warned {
                        self.warned = true;
                        tracing::warn!(error = %e, "camera: VTDecompressionSession build failed");
                    }
                    return;
                }
            } else {
                return; // no keyframe/param sets yet — wait for the first IDR AU
            }
        }
        if avcc.is_empty() {
            return;
        }
        self.decode_avcc(&avcc);
    }

    fn build_session(&mut self) -> Result<()> {
        let sps = self.sps.as_ref().unwrap();
        let pps = self.pps.as_ref().unwrap();
        let ptrs = [sps.as_ptr(), pps.as_ptr()];
        let sizes = [sps.len(), pps.len()];
        let mut fmt: CmFormatDescriptionRef = ptr::null();
        let st = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                ptr::null(),
                2,
                ptrs.as_ptr(),
                sizes.as_ptr(),
                4,
                &mut fmt,
            )
        };
        if st != 0 || fmt.is_null() {
            return Err(anyhow!(
                "CMVideoFormatDescriptionCreateFromH264ParameterSets OSStatus {st}"
            ));
        }
        let record = VtDecompressionOutputCallbackRecord {
            callback: decode_output,
            ref_con: self.cb_state as *mut c_void,
        };
        let mut session: VtDecompressionSessionRef = ptr::null();
        let st = unsafe {
            VTDecompressionSessionCreate(
                ptr::null(),
                fmt,
                ptr::null(),
                ptr::null(),
                &record,
                &mut session,
            )
        };
        if st != 0 || session.is_null() {
            unsafe { CFRelease(fmt) };
            return Err(anyhow!("VTDecompressionSessionCreate OSStatus {st}"));
        }
        self.format_desc = fmt;
        self.session = session;
        tracing::info!(
            width = self.width,
            height = self.height,
            "camera Phase-2b: VideoToolbox H.264 decoder ready"
        );
        Ok(())
    }

    fn decode_avcc(&mut self, avcc: &[u8]) {
        // Wrap the AVCC data in a CMBlockBuffer (VT-owned copy), then a
        // CMSampleBuffer with our format description, then decode.
        let mut block: CmBlockBufferRef = ptr::null();
        let st = unsafe {
            CMBlockBufferCreateWithMemoryBlock(
                ptr::null(),
                ptr::null_mut(),
                avcc.len(),
                ptr::null(),
                ptr::null(),
                0,
                avcc.len(),
                K_CM_BLOCK_BUFFER_ASSURE_MEMORY_NOW_FLAG,
                &mut block,
            )
        };
        if st != 0 || block.is_null() {
            return;
        }
        let st = unsafe {
            CMBlockBufferReplaceDataBytes(avcc.as_ptr() as *const c_void, block, 0, avcc.len())
        };
        if st != 0 {
            unsafe { CFRelease(block) };
            return;
        }
        let sizes = [avcc.len()];
        let mut sample: CmSampleBufferRef = ptr::null();
        let st = unsafe {
            CMSampleBufferCreateReady(
                ptr::null(),
                block,
                self.format_desc,
                1,
                0,
                ptr::null(),
                1,
                sizes.as_ptr(),
                &mut sample,
            )
        };
        if st != 0 || sample.is_null() {
            unsafe { CFRelease(block) };
            return;
        }
        let mut info_flags: u32 = 0;
        unsafe {
            VTDecompressionSessionDecodeFrame(
                self.session,
                sample,
                0,
                ptr::null_mut(),
                &mut info_flags,
            );
            CFRelease(sample);
            CFRelease(block);
        }
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        unsafe {
            if !self.session.is_null() {
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session);
            }
            if !self.format_desc.is_null() {
                CFRelease(self.format_desc);
            }
            // Session (and its callback) are gone; reclaim the boxed state.
            if !self.cb_state.is_null() {
                drop(Box::from_raw(self.cb_state));
            }
        }
    }
}

/// Iterator over the NAL units of an Annex-B buffer (splits on `00 00 01` /
/// `00 00 00 01` start codes), yielding each NAL's bytes without the start code.
struct AnnexBNals<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> AnnexBNals<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Index of the next start code at/after `from`, plus its length (3 or 4).
    fn next_start_code(&self, from: usize) -> Option<(usize, usize)> {
        let b = self.buf;
        let mut i = from;
        while i + 3 <= b.len() {
            if b[i] == 0 && b[i + 1] == 0 {
                if b[i + 2] == 1 {
                    return Some((i, 3));
                }
                if i + 4 <= b.len() && b[i + 2] == 0 && b[i + 3] == 1 {
                    return Some((i, 4));
                }
            }
            i += 1;
        }
        None
    }
}

impl<'a> Iterator for AnnexBNals<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        // Find the start code that begins the next NAL.
        let (sc_pos, sc_len) = self.next_start_code(self.pos)?;
        let nal_start = sc_pos + sc_len;
        // The NAL runs until the next start code (or end of buffer).
        let nal_end = match self.next_start_code(nal_start) {
            Some((next, _)) => next,
            None => self.buf.len(),
        };
        self.pos = nal_end;
        Some(&self.buf[nal_start..nal_end])
    }
}
