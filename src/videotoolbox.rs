//! VideoToolbox H.264 encoder, fed from `BitmapUpdate`s and producing
//! compressed H.264 sample buffers for the EGFX (AVC420) path.
//!
//! Scoped to "Phase 2 Step 1" of the H.264 plan: get a working
//! `VTCompressionSession`, push BGRA frames in, collect compressed bytes
//! out via the output callback. Wiring the output into
//! `Avc420BitmapStream` and the `GfxServerFactory` bridge is the next
//! step and lives in `src/h264.rs`.
//!
//! Conventions follow `src/auth.rs::pam_impl`: direct `extern "C"`,
//! no wrapper crate — the call surface here is small. macOS-only.
//!
//! Bitstream format note. VideoToolbox emits *AVCC*-style sample data:
//! NAL units prefixed with a 4-byte big-endian length, and SPS+PPS
//! carried out-of-band in the `CMFormatDescription`. MS-RDPRFX-AVC420
//! wants *Annex-B*: NAL units prefixed with `00 00 00 01` start codes,
//! with SPS/PPS prepended to each keyframe. The conversion is straight-
//! forward (rewrite length prefixes → start codes, prepend
//! parameter-set NALs to keyframes) but lives outside this module —
//! this file's job is purely to surface the encoded NALs.

#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::sync::mpsc;

use anyhow::{anyhow, bail, Result};

/// One H.264 sample as emitted by VideoToolbox. The payload is AVCC-
/// formatted (length-prefixed NAL units, no start codes). When this is
/// a keyframe (`is_keyframe = true`), `parameter_sets` carries the
/// SPS/PPS NALs that the client needs to decode it — these come out of
/// the `CMFormatDescription` and are required only on keyframes for
/// Annex-B conversion downstream.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    #[allow(dead_code)]
    pub pts: i64,
    /// SPS / PPS NAL units (raw, no length prefix and no start code).
    /// Populated only on keyframes — empty on non-keyframes.
    pub parameter_sets: Vec<Vec<u8>>,
}

pub struct Encoder {
    inner: ffi::SessionGuard,
    rx: mpsc::Receiver<EncodedFrame>,
    /// Heap-allocated sender pointed at by VT's `outputCallbackRefCon`.
    /// Owned here so it outlives the session; freed when the session is
    /// invalidated in `Drop`.
    _tx_ctx: Box<mpsc::Sender<EncodedFrame>>,
    width: u16,
    height: u16,
    next_pts: i64,
    /// Frame duration as a `CMTime` ratio (numerator, denominator).
    /// VT uses this for rate control even when we drive PTS manually.
    fps: u32,
}

impl Encoder {
    /// Create a new H.264 encoder. `bitrate_bps` is the target average
    /// bitrate; `fps` sets the frame-duration hint used by VT's rate
    /// controller. The first encoded frame will always be a keyframe.
    pub fn new(width: u16, height: u16, fps: u32, bitrate_bps: u32) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<EncodedFrame>();
        // The callback receives the raw `*mut Sender` and clones it per
        // delivery — see `ffi::output_callback`. Keep the original Box
        // alive on the Encoder so the pointer stays valid.
        let tx_box = Box::new(tx);
        let tx_ptr = Box::as_ref(&tx_box) as *const mpsc::Sender<EncodedFrame> as *mut c_void;

        let session = ffi::create_session(width, height, fps, bitrate_bps, tx_ptr)?;
        Ok(Self {
            inner: session,
            rx,
            _tx_ctx: tx_box,
            width,
            height,
            next_pts: 0,
            fps,
        })
    }

    /// Submit a BGRA frame for encoding. `stride` is in bytes per row
    /// of the source buffer — VideoToolbox is told the source pixel
    /// format is `kCVPixelFormatType_32BGRA`, so each pixel is 4 bytes
    /// in B, G, R, A order, matching what `capture.rs` produces.
    ///
    /// `force_keyframe` requests an IDR via VT's frame-property dict.
    /// Use it after dropping frames (e.g. recovering from EGFX
    /// backpressure) so the next encoded output restarts the H.264
    /// reference chain cleanly — otherwise P-frames after a drop refer
    /// to data the client never received and a region of the surface
    /// freezes.
    ///
    /// Returns immediately; encoded output arrives asynchronously via
    /// `drain()`. VT calls our output callback on its own thread, so
    /// the channel decouples producer and consumer cleanly.
    pub fn encode_bgra(&mut self, bgra: &[u8], stride: usize, force_keyframe: bool) -> Result<()> {
        let expected = stride
            .checked_mul(self.height.into())
            .ok_or_else(|| anyhow!("stride*height overflows usize"))?;
        if bgra.len() < expected {
            bail!(
                "BGRA buffer too small: have {}, need {} (stride {} * height {})",
                bgra.len(),
                expected,
                stride,
                self.height
            );
        }
        let pts = self.next_pts;
        self.next_pts = self.next_pts.wrapping_add(1);
        ffi::encode_frame(
            &self.inner,
            bgra,
            self.width,
            self.height,
            stride,
            pts,
            self.fps,
            force_keyframe,
        )
    }

    /// Pull every encoded frame the callback has produced so far.
    /// Non-blocking; returns an empty vec if nothing is ready.
    pub fn drain(&mut self) -> Vec<EncodedFrame> {
        let mut out = Vec::new();
        while let Ok(frame) = self.rx.try_recv() {
            out.push(frame);
        }
        out
    }

    /// Block until every frame submitted so far has been encoded and
    /// delivered to the output callback. The live pipeline uses the
    /// non-blocking `drain()`; `flush` is kept for shutdown / deterministic
    /// tests (and exercised by the encoder round-trip test).
    #[allow(dead_code)]
    pub fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        ffi::complete_frames(&self.inner)?;
        Ok(self.drain())
    }
}

// VT's compression session is thread-safe according to Apple's docs
// (frames can be submitted from any thread). The Receiver is !Sync but
// that's fine — we only call `drain()` from the owning thread.
unsafe impl Send for Encoder {}

mod ffi {
    use super::EncodedFrame;
    use anyhow::{anyhow, bail, Result};
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::mpsc;

    pub(super) type OSStatus = i32;
    pub(super) type Boolean = u8;
    pub(super) type CFTypeRef = *const c_void;
    pub(super) type CFAllocatorRef = CFTypeRef;
    pub(super) type CFStringRef = CFTypeRef;
    pub(super) type CFNumberRef = CFTypeRef;
    pub(super) type CFBooleanRef = CFTypeRef;
    pub(super) type CFDictionaryRef = CFTypeRef;
    pub(super) type CFArrayRef = CFTypeRef;
    pub(super) type CVPixelBufferRef = CFTypeRef;
    pub(super) type CVImageBufferRef = CFTypeRef;
    pub(super) type CMSampleBufferRef = CFTypeRef;
    pub(super) type CMBlockBufferRef = CFTypeRef;
    pub(super) type CMFormatDescriptionRef = CFTypeRef;
    pub(super) type VTCompressionSessionRef = CFTypeRef;
    pub(super) type VTEncodeInfoFlags = u32;
    pub(super) type OSType = u32;

    // Four-char codes are big-endian-packed on macOS.
    // 'B' 'G' 'R' 'A' = 0x42 0x47 0x52 0x41
    pub(super) const K_CV_PIXEL_FORMAT_TYPE_32_BGRA: OSType = 0x4247_5241; // 'BGRA'
                                                                           // 'a' 'v' 'c' '1' = 0x61 0x76 0x63 0x31
    pub(super) const K_CM_VIDEO_CODEC_TYPE_H264: OSType = 0x6176_6331; // 'avc1'

    #[repr(C)]
    #[derive(Copy, Clone, Debug)]
    pub(super) struct CMTime {
        pub value: i64,
        pub timescale: i32,
        pub flags: u32,
        pub epoch: i64,
    }
    pub(super) const K_CM_TIME_FLAGS_VALID: u32 = 1;

    pub(super) const KCF_NUMBER_INT32_TYPE: i32 = 3;

    /// Opaque stand-in for `CFDictionaryKeyCallBacks` /
    /// `CFDictionaryValueCallBacks`. We never read these structs — we
    /// only take the address of the linker-resolved `kCFType*` globals
    /// and pass it to `CFDictionaryCreate`. Size is padded large enough
    /// to outlive any future struct changes; reads would be UB but we
    /// don't do any.
    #[repr(C)]
    pub(super) struct CFCallbacksOpaque {
        _padding: [u8; 64],
    }

    /// Output callback type per `<VideoToolbox/VTCompressionSession.h>`:
    /// `void (*VTCompressionOutputCallback)(void *outputCallbackRefCon,
    ///   void *sourceFrameRefCon, OSStatus status, VTEncodeInfoFlags,
    ///   CMSampleBufferRef sampleBuffer);`
    pub(super) type VTCompressionOutputCallback = unsafe extern "C" fn(
        output_callback_ref_con: *mut c_void,
        source_frame_ref_con: *mut c_void,
        status: OSStatus,
        info_flags: VTEncodeInfoFlags,
        sample_buffer: CMSampleBufferRef,
    );

    // Each framework needs its own `#[link]`; clippy's duplicated_attributes
    // lint objects to the repeated `kind = "framework"`, which is unavoidable.
    #[allow(clippy::duplicated_attributes)]
    #[link(name = "CoreFoundation", kind = "framework")]
    #[link(name = "CoreVideo", kind = "framework")]
    #[link(name = "CoreMedia", kind = "framework")]
    #[link(name = "VideoToolbox", kind = "framework")]
    extern "C" {
        // CoreFoundation memory mgmt.
        pub(super) fn CFRelease(cf: CFTypeRef);
        pub(super) fn CFNumberCreate(
            allocator: CFAllocatorRef,
            the_type: i32,
            value_ptr: *const c_void,
        ) -> CFNumberRef;

        // Constant string handles for VT property keys + profile levels.
        // These are global symbols, dereferenced for the actual CFStringRef.
        pub(super) static kVTCompressionPropertyKey_RealTime: CFStringRef;
        pub(super) static kVTCompressionPropertyKey_AverageBitRate: CFStringRef;
        pub(super) static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFStringRef;
        pub(super) static kVTCompressionPropertyKey_ProfileLevel: CFStringRef;
        pub(super) static kVTCompressionPropertyKey_AllowFrameReordering: CFStringRef;
        pub(super) static kVTProfileLevel_H264_Baseline_AutoLevel: CFStringRef;
        pub(super) static kVTEncodeFrameOptionKey_ForceKeyFrame: CFStringRef;
        pub(super) static kCFBooleanTrue: CFBooleanRef;
        pub(super) static kCFBooleanFalse: CFBooleanRef;

        // CFDictionary creation for VT frame-properties payloads.
        // `CFDictionaryKeyCallBacks` / `CFDictionaryValueCallBacks` are
        // opaque structs here — we never read their contents, only take
        // the address of the global "CF type" instances and hand it to
        // CFDictionaryCreate. The byte sizes don't matter for that.
        pub(super) static kCFTypeDictionaryKeyCallBacks: CFCallbacksOpaque;
        pub(super) static kCFTypeDictionaryValueCallBacks: CFCallbacksOpaque;
        pub(super) fn CFDictionaryCreate(
            allocator: CFAllocatorRef,
            keys: *const *const c_void,
            values: *const *const c_void,
            num_values: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> CFDictionaryRef;

        // CVPixelBuffer.
        pub(super) fn CVPixelBufferCreate(
            allocator: CFAllocatorRef,
            width: usize,
            height: usize,
            pixel_format_type: OSType,
            pixel_buffer_attributes: CFDictionaryRef,
            pixel_buffer_out: *mut CVPixelBufferRef,
        ) -> i32;
        pub(super) fn CVPixelBufferLockBaseAddress(
            pixel_buffer: CVPixelBufferRef,
            lock_flags: u64,
        ) -> i32;
        pub(super) fn CVPixelBufferUnlockBaseAddress(
            pixel_buffer: CVPixelBufferRef,
            unlock_flags: u64,
        ) -> i32;
        pub(super) fn CVPixelBufferGetBaseAddress(pixel_buffer: CVPixelBufferRef) -> *mut c_void;
        pub(super) fn CVPixelBufferGetBytesPerRow(pixel_buffer: CVPixelBufferRef) -> usize;

        // CMSampleBuffer accessors used in the output callback.
        pub(super) fn CMSampleBufferGetDataBuffer(sbuf: CMSampleBufferRef) -> CMBlockBufferRef;
        pub(super) fn CMSampleBufferGetFormatDescription(
            sbuf: CMSampleBufferRef,
        ) -> CMFormatDescriptionRef;
        pub(super) fn CMSampleBufferGetSampleAttachmentsArray(
            sbuf: CMSampleBufferRef,
            create_if_necessary: Boolean,
        ) -> CFArrayRef;
        pub(super) fn CMSampleBufferGetPresentationTimeStamp(sbuf: CMSampleBufferRef) -> CMTime;
        pub(super) fn CMBlockBufferGetDataLength(bbuf: CMBlockBufferRef) -> usize;
        pub(super) fn CMBlockBufferCopyDataBytes(
            bbuf: CMBlockBufferRef,
            offset: usize,
            length: usize,
            destination: *mut c_void,
        ) -> OSStatus;
        pub(super) fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            fmt: CMFormatDescriptionRef,
            param_set_index: usize,
            param_set_pointer_out: *mut *const u8,
            param_set_size_out: *mut usize,
            param_set_count_out: *mut usize,
            nal_unit_header_length_out: *mut i32,
        ) -> OSStatus;

        // CFArray (used to test the "NotSync" keyframe attachment).
        pub(super) fn CFArrayGetCount(arr: CFArrayRef) -> isize;
        pub(super) fn CFArrayGetValueAtIndex(arr: CFArrayRef, idx: isize) -> CFTypeRef;
        pub(super) fn CFDictionaryGetValue(
            dict: CFDictionaryRef,
            key: *const c_void,
        ) -> *const c_void;
        pub(super) static kCMSampleAttachmentKey_NotSync: CFStringRef;

        // VTCompressionSession.
        pub(super) fn VTCompressionSessionCreate(
            allocator: CFAllocatorRef,
            width: i32,
            height: i32,
            codec_type: OSType,
            encoder_specification: CFDictionaryRef,
            source_image_buffer_attributes: CFDictionaryRef,
            compressed_data_allocator: CFAllocatorRef,
            output_callback: Option<VTCompressionOutputCallback>,
            output_callback_ref_con: *mut c_void,
            compression_session_out: *mut VTCompressionSessionRef,
        ) -> OSStatus;
        pub(super) fn VTSessionSetProperty(
            session: VTCompressionSessionRef,
            property_key: CFStringRef,
            property_value: CFTypeRef,
        ) -> OSStatus;
        pub(super) fn VTCompressionSessionPrepareToEncodeFrames(
            session: VTCompressionSessionRef,
        ) -> OSStatus;
        pub(super) fn VTCompressionSessionEncodeFrame(
            session: VTCompressionSessionRef,
            image_buffer: CVImageBufferRef,
            presentation_timestamp: CMTime,
            duration: CMTime,
            frame_properties: CFDictionaryRef,
            source_frame_ref_con: *mut c_void,
            info_flags_out: *mut VTEncodeInfoFlags,
        ) -> OSStatus;
        #[allow(dead_code)] // only reached via Encoder::flush (shutdown / tests)
        pub(super) fn VTCompressionSessionCompleteFrames(
            session: VTCompressionSessionRef,
            complete_until_presentation_timestamp: CMTime,
        ) -> OSStatus;
        pub(super) fn VTCompressionSessionInvalidate(session: VTCompressionSessionRef);
    }

    /// RAII guard for the `VTCompressionSession`. `CFRelease` is the
    /// counterpart to `VTCompressionSessionCreate`'s implicit retain.
    pub(super) struct SessionGuard {
        pub(super) session: VTCompressionSessionRef,
    }

    impl Drop for SessionGuard {
        fn drop(&mut self) {
            unsafe {
                if !self.session.is_null() {
                    VTCompressionSessionInvalidate(self.session);
                    CFRelease(self.session);
                }
            }
        }
    }
    // The session itself is documented as thread-safe.
    unsafe impl Send for SessionGuard {}

    pub(super) fn create_session(
        width: u16,
        height: u16,
        fps: u32,
        bitrate_bps: u32,
        tx_ctx: *mut c_void,
    ) -> Result<SessionGuard> {
        let mut session: VTCompressionSessionRef = ptr::null();
        let status = unsafe {
            VTCompressionSessionCreate(
                ptr::null(),
                i32::from(width),
                i32::from(height),
                K_CM_VIDEO_CODEC_TYPE_H264,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                Some(output_callback),
                tx_ctx,
                &mut session,
            )
        };
        if status != 0 || session.is_null() {
            bail!("VTCompressionSessionCreate failed: OSStatus {status}");
        }
        let guard = SessionGuard { session };

        // Real-time low-latency profile. Disable frame reordering so
        // every emitted frame is immediately decodable in order — RDP
        // does not tolerate B-frames or reorder delay.
        unsafe {
            set_bool(session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue)?;
            set_bool(
                session,
                kVTCompressionPropertyKey_AllowFrameReordering,
                kCFBooleanFalse,
            )?;
            set_string(
                session,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_Baseline_AutoLevel,
            )?;
            set_i32(
                session,
                kVTCompressionPropertyKey_AverageBitRate,
                bitrate_bps as i32,
            )?;
            // Force a keyframe at most every `fps * 2` frames (~2s). The
            // RDP client also has cap-bound keyframe expectations; tune
            // in Phase 3.
            set_i32(
                session,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                (fps * 2) as i32,
            )?;
        }

        let prepared = unsafe { VTCompressionSessionPrepareToEncodeFrames(session) };
        if prepared != 0 {
            bail!("VTCompressionSessionPrepareToEncodeFrames failed: OSStatus {prepared}");
        }
        Ok(guard)
    }

    unsafe fn set_bool(
        session: VTCompressionSessionRef,
        key: CFStringRef,
        value: CFBooleanRef,
    ) -> Result<()> {
        let status = VTSessionSetProperty(session, key, value);
        if status != 0 {
            bail!("VTSessionSetProperty(bool) failed: OSStatus {status}");
        }
        Ok(())
    }

    unsafe fn set_string(
        session: VTCompressionSessionRef,
        key: CFStringRef,
        value: CFStringRef,
    ) -> Result<()> {
        let status = VTSessionSetProperty(session, key, value);
        if status != 0 {
            bail!("VTSessionSetProperty(str) failed: OSStatus {status}");
        }
        Ok(())
    }

    unsafe fn set_i32(
        session: VTCompressionSessionRef,
        key: CFStringRef,
        value: i32,
    ) -> Result<()> {
        let number = CFNumberCreate(
            ptr::null(),
            KCF_NUMBER_INT32_TYPE,
            &value as *const i32 as *const c_void,
        );
        if number.is_null() {
            bail!("CFNumberCreate returned null");
        }
        let status = VTSessionSetProperty(session, key, number);
        CFRelease(number);
        if status != 0 {
            bail!("VTSessionSetProperty(i32 {value}) failed: OSStatus {status}");
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // internal FFI helper; grouping into a struct adds no clarity
    pub(super) fn encode_frame(
        guard: &SessionGuard,
        bgra: &[u8],
        width: u16,
        height: u16,
        stride: usize,
        pts: i64,
        fps: u32,
        force_keyframe: bool,
    ) -> Result<()> {
        let mut pbuf: CVPixelBufferRef = ptr::null();
        let status = unsafe {
            CVPixelBufferCreate(
                ptr::null(),
                width.into(),
                height.into(),
                K_CV_PIXEL_FORMAT_TYPE_32_BGRA,
                ptr::null(),
                &mut pbuf,
            )
        };
        if status != 0 || pbuf.is_null() {
            bail!("CVPixelBufferCreate failed: {status}");
        }

        // Pixel buffer ownership: VTCompressionSessionEncodeFrame
        // retains the CVPixelBuffer for the duration of the encode, so
        // we release our reference right after submit. The actual
        // CFRelease lives in the cleanup path below regardless of
        // success/failure so we don't leak on errors.
        let result = unsafe {
            CVPixelBufferLockBaseAddress(pbuf, 0);
            let dst = CVPixelBufferGetBaseAddress(pbuf) as *mut u8;
            let dst_stride = CVPixelBufferGetBytesPerRow(pbuf);
            let row_bytes = usize::from(width) * 4;
            // CVPixelBuffer may have a different stride than the source
            // (alignment to 64 bytes is common). Copy row by row.
            for row in 0..usize::from(height) {
                let src_offset = row * stride;
                let dst_offset = row * dst_stride;
                ptr::copy_nonoverlapping(
                    bgra.as_ptr().add(src_offset),
                    dst.add(dst_offset),
                    row_bytes,
                );
            }
            CVPixelBufferUnlockBaseAddress(pbuf, 0);

            let presentation = CMTime {
                value: pts,
                timescale: i32::try_from(fps).unwrap_or(30),
                flags: K_CM_TIME_FLAGS_VALID,
                epoch: 0,
            };
            let duration = CMTime {
                value: 1,
                timescale: i32::try_from(fps).unwrap_or(30),
                flags: K_CM_TIME_FLAGS_VALID,
                epoch: 0,
            };
            let frame_props: CFDictionaryRef = if force_keyframe {
                let key = kVTEncodeFrameOptionKey_ForceKeyFrame;
                let value = kCFBooleanTrue;
                CFDictionaryCreate(
                    ptr::null(),
                    &key as *const *const c_void,
                    &value as *const *const c_void,
                    1,
                    &kCFTypeDictionaryKeyCallBacks as *const _ as *const c_void,
                    &kCFTypeDictionaryValueCallBacks as *const _ as *const c_void,
                )
            } else {
                ptr::null()
            };

            let mut info_flags: VTEncodeInfoFlags = 0;
            let encode_status = VTCompressionSessionEncodeFrame(
                guard.session,
                pbuf,
                presentation,
                duration,
                frame_props,
                ptr::null_mut(),
                &mut info_flags,
            );
            if !frame_props.is_null() {
                CFRelease(frame_props);
            }
            if encode_status != 0 {
                Err(anyhow!(
                    "VTCompressionSessionEncodeFrame failed: OSStatus {encode_status}"
                ))
            } else {
                Ok(())
            }
        };
        unsafe { CFRelease(pbuf) };
        result
    }

    #[allow(dead_code)] // only reached via Encoder::flush (shutdown / tests)
    pub(super) fn complete_frames(guard: &SessionGuard) -> Result<()> {
        // Passing an invalid CMTime tells VT "complete *all* pending frames".
        // This is the documented sentinel value for "no deadline".
        let invalid = CMTime {
            value: 0,
            timescale: 0,
            flags: 0,
            epoch: 0,
        };
        let status = unsafe { VTCompressionSessionCompleteFrames(guard.session, invalid) };
        if status != 0 {
            bail!("VTCompressionSessionCompleteFrames failed: OSStatus {status}");
        }
        Ok(())
    }

    /// VT output callback. Runs on a VT-internal thread; gets a pointer
    /// back to the `mpsc::Sender<EncodedFrame>` we stashed in
    /// `outputCallbackRefCon`. We clone the sender per delivery so its
    /// lifetime is tied to the Encoder, not the callback.
    unsafe extern "C" fn output_callback(
        output_callback_ref_con: *mut c_void,
        _source_frame_ref_con: *mut c_void,
        status: OSStatus,
        _info_flags: VTEncodeInfoFlags,
        sample_buffer: CMSampleBufferRef,
    ) {
        if status != 0 || sample_buffer.is_null() || output_callback_ref_con.is_null() {
            // TODO(phase-2-step-2): surface this via the channel as an
            // error variant so the EGFX bridge can react (drop session,
            // request keyframe, etc.). Silently dropping is fine for the
            // scaffold only.
            return;
        }
        let tx = &*(output_callback_ref_con as *const mpsc::Sender<EncodedFrame>);
        let Ok(frame) = extract_frame(sample_buffer) else {
            return;
        };
        let _ = tx.send(frame);
    }

    unsafe fn extract_frame(sbuf: CMSampleBufferRef) -> Result<EncodedFrame> {
        let bbuf = CMSampleBufferGetDataBuffer(sbuf);
        if bbuf.is_null() {
            bail!("CMSampleBufferGetDataBuffer returned null");
        }
        let len = CMBlockBufferGetDataLength(bbuf);
        let mut data = vec![0u8; len];
        let copy_status =
            CMBlockBufferCopyDataBytes(bbuf, 0, len, data.as_mut_ptr() as *mut c_void);
        if copy_status != 0 {
            bail!("CMBlockBufferCopyDataBytes failed: {copy_status}");
        }

        // Keyframe = the sample is NOT marked as "NotSync" in the
        // sample-attachments array. Per Apple docs, if the attachments
        // array is missing or empty, the sample is a sync (keyframe).
        let mut is_keyframe = true;
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sbuf, 0);
        if !attachments.is_null() && CFArrayGetCount(attachments) > 0 {
            let dict = CFArrayGetValueAtIndex(attachments, 0) as CFDictionaryRef;
            if !dict.is_null() {
                let not_sync = CFDictionaryGetValue(dict, kCMSampleAttachmentKey_NotSync);
                if !not_sync.is_null() {
                    // A present "NotSync" attachment means this is a
                    // non-keyframe. (Apple's API encodes the negation.)
                    is_keyframe = not_sync == kCFBooleanTrue;
                    is_keyframe = !is_keyframe;
                }
            }
        }

        // SPS/PPS come from the format description on keyframes.
        let mut parameter_sets = Vec::new();
        if is_keyframe {
            let fmt = CMSampleBufferGetFormatDescription(sbuf);
            if !fmt.is_null() {
                // Index 0 = SPS, 1 = PPS for H.264 (per Apple). We pull
                // until we run out, so the code still works if a future
                // SDK exposes more (e.g. SPS-Ext).
                let mut index = 0usize;
                loop {
                    let mut ptr_out: *const u8 = ptr::null();
                    let mut size_out: usize = 0;
                    let mut count_out: usize = 0;
                    let st = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        fmt,
                        index,
                        &mut ptr_out,
                        &mut size_out,
                        &mut count_out,
                        ptr::null_mut(),
                    );
                    if st != 0 || ptr_out.is_null() || size_out == 0 {
                        break;
                    }
                    let slice = std::slice::from_raw_parts(ptr_out, size_out);
                    parameter_sets.push(slice.to_vec());
                    index += 1;
                    if count_out > 0 && index >= count_out {
                        break;
                    }
                }
            }
        }

        let pts = CMSampleBufferGetPresentationTimeStamp(sbuf).value;
        Ok(EncodedFrame {
            data,
            is_keyframe,
            pts,
            parameter_sets,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip sanity test. Creates a session, encodes a single
    /// solid-color BGRA frame, drains the channel, and asserts that
    /// the first emitted frame is a keyframe carrying SPS+PPS. Doesn't
    /// validate the bitstream content beyond "non-empty" — that needs
    /// a real H.264 decoder, which is overkill for the scaffold.
    #[test]
    fn encodes_a_keyframe() -> Result<()> {
        let w: u16 = 320;
        let h: u16 = 240;
        let stride = usize::from(w) * 4;
        let mut frame = vec![0u8; stride * usize::from(h)];
        for px in frame.chunks_exact_mut(4) {
            px[0] = 0x33; // B
            px[1] = 0x77; // G
            px[2] = 0xcc; // R
            px[3] = 0xff; // A
        }

        let mut enc = Encoder::new(w, h, 30, 4_000_000)?;
        enc.encode_bgra(&frame, stride, false)?;
        let frames = enc.flush()?;
        assert!(!frames.is_empty(), "expected at least one encoded frame");
        let first = &frames[0];
        assert!(first.is_keyframe, "first frame should be a keyframe");
        assert!(
            !first.data.is_empty(),
            "encoded payload should be non-empty"
        );
        assert!(
            !first.parameter_sets.is_empty(),
            "keyframe should carry SPS/PPS parameter sets"
        );
        Ok(())
    }
}
