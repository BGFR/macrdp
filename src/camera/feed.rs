//! Camera sink feed (Phase 3b) — the CoreMediaIO **client** that pushes decoded
//! webcam frames into the macrdp Camera system extension's **sink** stream.
//!
//! macrdp is itself a CMIO client here: it finds the "macrdp Camera" virtual device
//! the extension registered, opens the device's *sink* stream (the one a producer
//! writes to), and enqueues each decoded `CVPixelBuffer` — wrapped in a
//! `CMSampleBuffer` — onto the sink's `CMSimpleQueue`. The extension's consume loop
//! (see `gui/Sources/macrdpcamera/main.swift`) pulls each buffer and re-sends it on
//! its source stream, so apps (Photo Booth / Zoom) see the live redirected webcam.
//!
//! There is no AVFoundation API for the sink direction — the CoreMediaIO C client
//! API is mandatory — which is why this lives in Rust FFI next to the decoder rather
//! than in the Swift extension. The decoded `CVPixelBuffer` is IOSurface-backed
//! (VideoToolbox output), so CMIO ships it to the extension process zero-copy.
//!
//! Sequence + gotchas confirmed against Apple's "Creating a camera extension with
//! Core Media I/O" sample (ldenoue/cameraextension `ViewController.swift`): match the
//! device by `kCMIODevicePropertyDeviceUID`, and per frame **check
//! `GetCount < GetCapacity`** then **`CFRetain` before enqueue** (a `CMSimpleQueue`
//! stores raw pointers and does NOT retain — the extension releases on dequeue; skip
//! the retain → use-after-free, enqueue-when-full → error).
//!
//! **Picking the sink stream is the subtle part** (live-debugged 2026-07-20). Do NOT
//! choose it by `kCMIOStreamPropertyDirection`: measured against our own extension the
//! SOURCE reports direction=1 and the SINK direction=0, inverted from the header's
//! "0 = output / 1 = input". And starting the WRONG stream fails silently —
//! `CMIODeviceStartStream` returns success while the extension's sink handlers never
//! fire, so nothing drains the queue and every frame is dropped as "queue full". We
//! select by stream NAME (`*Sink*`), which is exact because macrdp owns both ends,
//! falling back to index [1] (the extension registers source first, sink second).

use std::ffi::c_void;
use std::os::raw::c_char;
use std::ptr;

use anyhow::{anyhow, Result};

use super::MACRDP_CAMERA_DEVICE_UID;

type OsStatus = i32;
type CfTypeRef = *const c_void;
type CmFormatDescriptionRef = CfTypeRef;
type CmSampleBufferRef = CfTypeRef;
type CvImageBufferRef = CfTypeRef;
type CmSimpleQueueRef = CfTypeRef;
type CmClockRef = CfTypeRef;
type CmioObjectId = u32;

/// `CMTime` — must match the layout in `decode.rs` / `videotoolbox.rs`.
#[repr(C)]
#[derive(Clone, Copy)]
struct CmTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

/// `kCMTimeInvalid` (flags == 0 ⇒ invalid).
const CM_TIME_INVALID: CmTime = CmTime {
    value: 0,
    timescale: 0,
    flags: 0,
    epoch: 0,
};

#[repr(C)]
#[derive(Clone, Copy)]
struct CmSampleTimingInfo {
    duration: CmTime,
    presentation_time_stamp: CmTime,
    decode_time_stamp: CmTime,
}

/// `CMIOObjectPropertyAddress`.
#[repr(C)]
struct CmioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

// FourCC selectors (big-endian ASCII), from <CoreMediaIO/CMIOHardware*.h>.
const K_CMIO_OBJECT_SYSTEM_OBJECT: CmioObjectId = 1;
const K_CMIO_HARDWARE_PROPERTY_DEVICES: u32 = u32::from_be_bytes(*b"dev#");
const K_CMIO_OBJECT_PROPERTY_SCOPE_GLOBAL: u32 = u32::from_be_bytes(*b"glob");
const K_CMIO_OBJECT_PROPERTY_ELEMENT_MAIN: u32 = 0;
const K_CMIO_DEVICE_PROPERTY_DEVICE_UID: u32 = u32::from_be_bytes(*b"uid ");
const K_CMIO_DEVICE_PROPERTY_STREAMS: u32 = u32::from_be_bytes(*b"stm#");
// kCMIOStreamPropertyDirection ('sdir'), logged for diagnostics only.
//
// DO NOT pick the sink by this value. Measured live against our own extension
// (which registers source-then-sink): the SOURCE reports direction=1 and the SINK
// reports direction=0 — inverted from the "0 = output / 1 = input" reading in the
// headers. Starting the wrong stream silently "succeeds" (the producer's
// CMIODeviceStartStream returns 0) while the extension's sink handlers never fire,
// so nothing ever drains the queue. We select by stream NAME instead, which is
// unambiguous because macrdp owns both ends.
const K_CMIO_STREAM_PROPERTY_DIRECTION: u32 = u32::from_be_bytes(*b"sdir");
/// `kCMIOObjectPropertyName` ('lnam') — the stream's localized name.
const K_CMIO_OBJECT_PROPERTY_NAME: u32 = u32::from_be_bytes(*b"lnam");
/// Substring identifying the extension's sink stream ("macrdp Camera.Video.Sink").
const SINK_STREAM_NAME_MARKER: &str = "sink";

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// The queue-altered callback CMIO fires when the extension dequeues a buffer (the
/// sink has room again). We flow-control with an explicit GetCount/GetCapacity guard
/// instead, so this is a no-op — but `CMIOStreamCopyBufferQueue` wants a real proc.
extern "C" fn queue_altered(_stream: u32, _token: *mut c_void, _refcon: *mut c_void) {}

#[allow(clippy::duplicated_attributes)]
#[link(name = "CoreFoundation", kind = "framework")]
#[link(name = "CoreMedia", kind = "framework")]
#[link(name = "CoreMediaIO", kind = "framework")]
extern "C" {
    fn CFRetain(cf: CfTypeRef) -> CfTypeRef;
    fn CFRelease(cf: CfTypeRef);
    fn CFStringGetCString(
        the_string: CfTypeRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;

    fn CMIOObjectGetPropertyDataSize(
        object_id: CmioObjectId,
        address: *const CmioObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const c_void,
        data_size: *mut u32,
    ) -> OsStatus;
    fn CMIOObjectGetPropertyData(
        object_id: CmioObjectId,
        address: *const CmioObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const c_void,
        data_size: u32,
        data_used: *mut u32,
        data: *mut c_void,
    ) -> OsStatus;
    fn CMIOStreamCopyBufferQueue(
        stream_id: CmioObjectId,
        queue_altered_proc: extern "C" fn(u32, *mut c_void, *mut c_void),
        queue_altered_refcon: *mut c_void,
        queue_out: *mut CmSimpleQueueRef,
    ) -> OsStatus;
    fn CMIODeviceStartStream(device_id: CmioObjectId, stream_id: CmioObjectId) -> OsStatus;
    fn CMIODeviceStopStream(device_id: CmioObjectId, stream_id: CmioObjectId) -> OsStatus;

    fn CMSimpleQueueGetCount(queue: CmSimpleQueueRef) -> i32;
    fn CMSimpleQueueGetCapacity(queue: CmSimpleQueueRef) -> i32;
    fn CMSimpleQueueEnqueue(queue: CmSimpleQueueRef, element: *const c_void) -> OsStatus;

    fn CMClockGetHostTimeClock() -> CmClockRef;
    fn CMClockGetTime(clock: CmClockRef) -> CmTime;

    fn CMVideoFormatDescriptionCreateForImageBuffer(
        allocator: CfTypeRef,
        image_buffer: CvImageBufferRef,
        format_description_out: *mut CmFormatDescriptionRef,
    ) -> OsStatus;
    fn CMSampleBufferCreateForImageBuffer(
        allocator: CfTypeRef,
        image_buffer: CvImageBufferRef,
        data_ready: u8,
        make_data_ready_callback: *const c_void,
        make_data_ready_refcon: *mut c_void,
        format_description: CmFormatDescriptionRef,
        sample_timing: *const CmSampleTimingInfo,
        sample_buffer_out: *mut CmSampleBufferRef,
    ) -> OsStatus;
}

/// An open feed to the macrdp Camera extension's sink stream.
pub struct CameraFeed {
    device_id: CmioObjectId,
    sink_stream_id: CmioObjectId,
    queue: CmSimpleQueueRef,
    enqueued: u64,
    dropped_full: u64,
}

// SAFETY: only touched from the single decoder thread (the DVC device processor).
unsafe impl Send for CameraFeed {}

impl CameraFeed {
    /// Discover the macrdp Camera virtual device, open its sink stream, and start
    /// it. `Err` if the extension isn't installed/active (no matching device) — the
    /// caller degrades gracefully (decode continues without presenting a camera).
    pub fn new() -> Result<Self> {
        // SAFETY: standard CMIO property-query FFI; all buffers sized from the
        // size-query call before the data call.
        unsafe {
            let devices = get_object_array(
                K_CMIO_OBJECT_SYSTEM_OBJECT,
                K_CMIO_HARDWARE_PROPERTY_DEVICES,
            )?;
            let device_id = devices
                .into_iter()
                .find(|&d| {
                    cf_string_property(d, K_CMIO_DEVICE_PROPERTY_DEVICE_UID)
                        .map(|u| u.eq_ignore_ascii_case(MACRDP_CAMERA_DEVICE_UID))
                        .unwrap_or(false)
                })
                .ok_or_else(|| anyhow!("macrdp Camera device not found (extension not active?)"))?;

            let streams = get_object_array(device_id, K_CMIO_DEVICE_PROPERTY_STREAMS)?;
            if streams.len() < 2 {
                return Err(anyhow!(
                    "macrdp Camera exposes {} stream(s); expected a source + a sink",
                    streams.len()
                ));
            }
            // Pick the sink by NAME (we own the extension, so the name is exact).
            // Direction is logged for diagnostics only — see the const comment for
            // why it must not be used to choose.
            for (i, &s) in streams.iter().enumerate() {
                tracing::info!(
                    index = i,
                    stream_id = s,
                    name = ?cf_string_property(s, K_CMIO_OBJECT_PROPERTY_NAME),
                    direction = ?stream_direction(s),
                    "camera Phase-3b: enumerated stream"
                );
            }
            let sink_stream_id = streams
                .iter()
                .copied()
                .find(|&s| {
                    cf_string_property(s, K_CMIO_OBJECT_PROPERTY_NAME)
                        .map(|n| n.to_ascii_lowercase().contains(SINK_STREAM_NAME_MARKER))
                        .unwrap_or(false)
                })
                .unwrap_or_else(|| {
                    // The extension registers source first, sink second.
                    tracing::warn!(
                        "camera Phase-3b: no stream named '*Sink*' — falling back to streams[1]"
                    );
                    streams[1]
                });

            let mut queue: CmSimpleQueueRef = ptr::null();
            let st = CMIOStreamCopyBufferQueue(
                sink_stream_id,
                queue_altered,
                ptr::null_mut(),
                &mut queue,
            );
            if st != 0 || queue.is_null() {
                return Err(anyhow!("CMIOStreamCopyBufferQueue OSStatus {st}"));
            }
            let st = CMIODeviceStartStream(device_id, sink_stream_id);
            if st != 0 {
                CFRelease(queue);
                return Err(anyhow!("CMIODeviceStartStream OSStatus {st}"));
            }
            tracing::info!(
                device_id,
                sink_stream_id,
                "camera Phase-3b: feeding the macrdp Camera sink stream"
            );
            Ok(Self {
                device_id,
                sink_stream_id,
                queue,
                enqueued: 0,
                dropped_full: 0,
            })
        }
    }

    /// Enqueue one decoded frame onto the sink. Best-effort: drops the frame if the
    /// sink queue is full (the extension hasn't drained yet) rather than blocking the
    /// decoder. Never panics on a CMIO error — logs periodically.
    pub fn enqueue(&mut self, image_buffer: CvImageBufferRef) {
        if image_buffer.is_null() {
            return;
        }
        // SAFETY: valid IOSurface-backed CVImageBuffer from the VT decode callback.
        unsafe {
            // Full-queue guard (TRAP 1): enqueuing into a full CMSimpleQueue errors.
            if CMSimpleQueueGetCount(self.queue) >= CMSimpleQueueGetCapacity(self.queue) {
                self.dropped_full += 1;
                self.log_stats_periodically();
                return;
            }

            let mut fmt: CmFormatDescriptionRef = ptr::null();
            if CMVideoFormatDescriptionCreateForImageBuffer(ptr::null(), image_buffer, &mut fmt)
                != 0
                || fmt.is_null()
            {
                return;
            }
            let now = CMClockGetTime(CMClockGetHostTimeClock());
            let timing = CmSampleTimingInfo {
                duration: CM_TIME_INVALID,
                presentation_time_stamp: now,
                decode_time_stamp: CM_TIME_INVALID,
            };
            let mut sample: CmSampleBufferRef = ptr::null();
            let st = CMSampleBufferCreateForImageBuffer(
                ptr::null(),
                image_buffer,
                1, // dataReady
                ptr::null(),
                ptr::null_mut(),
                fmt,
                &timing,
                &mut sample,
            );
            CFRelease(fmt); // the sample buffer retains it
            if st != 0 || sample.is_null() {
                return;
            }
            // TRAP 2: retain before enqueue — CMSimpleQueue stores a raw pointer and
            // does NOT retain; the extension releases it on dequeue. We drop our own
            // reference (the retained one lives in the queue now).
            CFRetain(sample);
            let st = CMSimpleQueueEnqueue(self.queue, sample);
            if st != 0 {
                // Enqueue failed — reclaim the retain so we don't leak.
                CFRelease(sample);
            } else {
                self.enqueued += 1;
            }
            CFRelease(sample); // balance our create (+1); queue holds its own retain
            self.log_stats_periodically();
        }
    }

    /// Log enqueue/drop counts every ~90 frames (~3 s at 30 fps) so the extension's
    /// consumption is observable from the macrdp side (the extension's own os_log is
    /// invisible under the CMIO role account). A low `dropped_full` means the
    /// extension is draining the sink; a high one means it isn't.
    fn log_stats_periodically(&self) {
        let total = self.enqueued + self.dropped_full;
        if total > 0 && total.is_multiple_of(90) {
            tracing::info!(
                enqueued = self.enqueued,
                dropped_full = self.dropped_full,
                "camera Phase-3b: sink feed stats (low dropped_full = extension draining)"
            );
        }
    }
}

impl Drop for CameraFeed {
    fn drop(&mut self) {
        // SAFETY: ids/queue are valid for the feed's lifetime.
        unsafe {
            CMIODeviceStopStream(self.device_id, self.sink_stream_id);
            if !self.queue.is_null() {
                CFRelease(self.queue);
            }
        }
        tracing::info!(
            enqueued = self.enqueued,
            dropped_full = self.dropped_full,
            "camera Phase-3b: sink feed closed"
        );
    }
}

/// Read a CMIO object-array property (`kCMIOHardwarePropertyDevices` /
/// `kCMIODevicePropertyStreams`) as a `Vec<CMIOObjectID>`.
///
/// SAFETY: `object_id` must be a live CMIO object; `selector` an array property.
unsafe fn get_object_array(object_id: CmioObjectId, selector: u32) -> Result<Vec<CmioObjectId>> {
    let address = CmioObjectPropertyAddress {
        selector,
        scope: K_CMIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        element: K_CMIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };
    let mut size: u32 = 0;
    let st = CMIOObjectGetPropertyDataSize(object_id, &address, 0, ptr::null(), &mut size);
    if st != 0 {
        return Err(anyhow!("CMIOObjectGetPropertyDataSize OSStatus {st}"));
    }
    let count = size as usize / std::mem::size_of::<CmioObjectId>();
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut ids = vec![0u32; count];
    let mut used: u32 = 0;
    let st = CMIOObjectGetPropertyData(
        object_id,
        &address,
        0,
        ptr::null(),
        size,
        &mut used,
        ids.as_mut_ptr() as *mut c_void,
    );
    if st != 0 {
        return Err(anyhow!("CMIOObjectGetPropertyData OSStatus {st}"));
    }
    Ok(ids)
}

/// Read a stream's `kCMIOStreamPropertyDirection` (0 = output/source, 1 = input/sink).
///
/// SAFETY: `stream_id` must be a live CMIO stream object.
unsafe fn stream_direction(stream_id: CmioObjectId) -> Option<u32> {
    let address = CmioObjectPropertyAddress {
        selector: K_CMIO_STREAM_PROPERTY_DIRECTION,
        scope: K_CMIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        element: K_CMIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };
    let mut size: u32 = 0;
    if CMIOObjectGetPropertyDataSize(stream_id, &address, 0, ptr::null(), &mut size) != 0 {
        return None;
    }
    let mut dir: u32 = 0;
    let mut used: u32 = 0;
    if CMIOObjectGetPropertyData(
        stream_id,
        &address,
        0,
        ptr::null(),
        size,
        &mut used,
        &mut dir as *mut u32 as *mut c_void,
    ) != 0
    {
        return None;
    }
    Some(dir)
}

/// Read a CFString-valued CMIO property (device UID, stream name, …) as a `String`.
///
/// SAFETY: `object_id` must be a live CMIO object and `selector` a CFString property.
unsafe fn cf_string_property(object_id: CmioObjectId, selector: u32) -> Option<String> {
    let device_id = object_id;
    let address = CmioObjectPropertyAddress {
        selector,
        scope: K_CMIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        element: K_CMIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };
    let mut size: u32 = 0;
    if CMIOObjectGetPropertyDataSize(device_id, &address, 0, ptr::null(), &mut size) != 0 {
        return None;
    }
    // The property is a single CFStringRef.
    let mut cfstr: CfTypeRef = ptr::null();
    let mut used: u32 = 0;
    if CMIOObjectGetPropertyData(
        device_id,
        &address,
        0,
        ptr::null(),
        size,
        &mut used,
        &mut cfstr as *mut CfTypeRef as *mut c_void,
    ) != 0
        || cfstr.is_null()
    {
        return None;
    }
    let mut buf = [0 as c_char; 256];
    let ok = CFStringGetCString(
        cfstr,
        buf.as_mut_ptr(),
        buf.len() as isize,
        K_CF_STRING_ENCODING_UTF8,
    );
    CFRelease(cfstr); // Get*PropertyData returns a +1 CFString for a CFType property
    if ok == 0 {
        return None;
    }
    let cstr = std::ffi::CStr::from_ptr(buf.as_ptr());
    Some(cstr.to_string_lossy().into_owned())
}
