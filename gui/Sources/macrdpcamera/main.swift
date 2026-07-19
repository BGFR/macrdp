// macrdp Camera — a CoreMediaIO Camera **system extension** (macOS 12.3+) that
// presents a virtual camera ("macrdp Camera") selectable in Photo Booth / Zoom /
// FaceTime / Teams. This is camera-redirection **Phase 3** — the payoff phase that
// surfaces the redirected webcam (decoded to CVPixelBuffers by Phases 1+2) as a
// real macOS capture device.
//
// PHASE 3a (this file, as it stands): the device exposes a single **source**
// stream that emits a STATIC test pattern (a white stripe sweeping down a gray
// field) on a timer. It has NO sink and NO connection to macrdp yet — the point of
// 3a is purely to bring the *device* up and prove the whole
// signing/activation/CMIO-wiring path works before any real pixels flow. GREEN for
// 3a = "macrdp Camera" appears in Photo Booth showing the sweeping stripe.
//
// PHASE 3b (next): add a second **sink** stream. macrdp's Rust process becomes the
// CMIO client and feeds the sink directly via the CoreMediaIO C client API
// (CMIOStreamCopyBufferQueue + CMSimpleQueueEnqueue); the device's consume loop
// forwards sink buffers onto the source, replacing the test pattern with the live
// redirected webcam. See ~/.claude/plans/camera-redirection-phase3.md.
//
// Structure mirrors Apple's "Creating a camera extension with Core Media I/O"
// sample (WWDC22 s10022) + Halle/SinkCam: a Provider owns one Device, the Device
// owns the stream(s) and the frame timer, and each stream has a StreamSource
// delegate. Built as a plain SwiftPM executable (no Xcode); packaging assembles the
// `.systemextension` bundle around it. The executable's entry point hands the
// provider to CMIOExtensionProvider.startService and runs the CFRunLoop.

import CoreMediaIO
import Foundation
import IOKit.audio
import os.log

private let kFrameRate = 30
private let kWidth: Int32 = 1280
private let kHeight: Int32 = 720
private let logger = OSLog(subsystem: "com.clintcan.macrdp.camera", category: "extension")

// MARK: - Provider (one virtual device)

final class MacrdpCameraProviderSource: NSObject, CMIOExtensionProviderSource {
    private(set) var provider: CMIOExtensionProvider!
    private var deviceSource: MacrdpCameraDeviceSource!

    init(clientQueue: DispatchQueue?) {
        super.init()
        provider = CMIOExtensionProvider(source: self, clientQueue: clientQueue)
        deviceSource = MacrdpCameraDeviceSource(localizedName: "macrdp Camera")
        do {
            try provider.addDevice(deviceSource.device)
        } catch {
            os_log(.error, log: logger, "failed to add device: %{public}@", error.localizedDescription)
            fatalError("failed to add device: \(error.localizedDescription)")
        }
        os_log(.info, log: logger, "macrdp Camera provider started")
    }

    // A client (a capturing app) connected/disconnected. Phase 3c validates the
    // sink client here; the source direction accepts everyone.
    func connect(to client: CMIOExtensionClient) throws {}
    func disconnect(from client: CMIOExtensionClient) {}

    var availableProperties: Set<CMIOExtensionProperty> {
        [.providerManufacturer]
    }

    func providerProperties(forProperties properties: Set<CMIOExtensionProperty>) throws
        -> CMIOExtensionProviderProperties
    {
        let props = CMIOExtensionProviderProperties(dictionary: [:])
        if properties.contains(.providerManufacturer) {
            props.manufacturer = "macrdp"
        }
        return props
    }

    func setProviderProperties(_ providerProperties: CMIOExtensionProviderProperties) throws {}
}

// MARK: - Device (owns the stream + the frame timer)

final class MacrdpCameraDeviceSource: NSObject, CMIOExtensionDeviceSource {
    private(set) var device: CMIOExtensionDevice!
    private var _streamSource: MacrdpCameraStreamSource!
    private var _streamingCounter: UInt32 = 0
    private var _timer: DispatchSourceTimer?
    private let _timerQueue = DispatchQueue(
        label: "com.clintcan.macrdp.camera.timer", qos: .userInteractive)
    private var _videoDescription: CMFormatDescription!
    private var _bufferPool: CVPixelBufferPool!
    private var _bufferAuxAttributes: NSDictionary!
    private var _stripeRow: UInt32 = 0

    init(localizedName: String) {
        super.init()
        // A STABLE device UUID (not a random one per launch): consuming apps and
        // macOS remember the device by id, and a fresh id every launch reads as a
        // brand-new camera. Derived once from the bundle identity.
        let deviceID = UUID(uuidString: "6F1B2C3D-4E5A-6B7C-8D9E-A0B1C2D3E4F0")!
        device = CMIOExtensionDevice(
            localizedName: localizedName, deviceID: deviceID, legacyDeviceID: nil, source: self)

        let dims = CMVideoDimensions(width: kWidth, height: kHeight)
        CMVideoFormatDescriptionCreate(
            allocator: kCFAllocatorDefault,
            codecType: kCVPixelFormatType_32BGRA,
            width: dims.width, height: dims.height,
            extensions: nil, formatDescriptionOut: &_videoDescription)

        let pixelBufferAttributes: NSDictionary = [
            kCVPixelBufferWidthKey: dims.width,
            kCVPixelBufferHeightKey: dims.height,
            kCVPixelBufferPixelFormatTypeKey: _videoDescription.mediaSubType,
            kCVPixelBufferIOSurfacePropertiesKey: [String: Any]() as NSDictionary,
        ]
        CVPixelBufferPoolCreate(
            kCFAllocatorDefault, nil, pixelBufferAttributes, &_bufferPool)

        let videoStreamFormat = CMIOExtensionStreamFormat(
            formatDescription: _videoDescription,
            maxFrameDuration: CMTime(value: 1, timescale: Int32(kFrameRate)),
            minFrameDuration: CMTime(value: 1, timescale: Int32(kFrameRate)),
            validFrameDurations: nil)
        _bufferAuxAttributes = [kCVPixelBufferPoolAllocationThresholdKey: 5]

        _streamSource = MacrdpCameraStreamSource(
            localizedName: "macrdp Camera.Video",
            streamID: UUID(),
            streamFormat: videoStreamFormat,
            device: device)
        do {
            try device.addStream(_streamSource.stream)
        } catch {
            fatalError("failed to add stream: \(error.localizedDescription)")
        }
    }

    var availableProperties: Set<CMIOExtensionProperty> {
        [.deviceTransportType, .deviceModel]
    }

    func deviceProperties(forProperties properties: Set<CMIOExtensionProperty>) throws
        -> CMIOExtensionDeviceProperties
    {
        let props = CMIOExtensionDeviceProperties(dictionary: [:])
        if properties.contains(.deviceTransportType) {
            props.transportType = kIOAudioDeviceTransportTypeVirtual
        }
        if properties.contains(.deviceModel) {
            props.model = "macrdp Camera"
        }
        return props
    }

    func setDeviceProperties(_ deviceProperties: CMIOExtensionDeviceProperties) throws {}

    // A source-stream consumer started. Ref-count so overlapping consumers share
    // one timer; start the test-pattern generator on the first.
    func startStreaming() {
        guard _bufferPool != nil else { return }
        _streamingCounter += 1
        if _timer != nil { return }
        let timer = DispatchSource.makeTimerSource(flags: .strict, queue: _timerQueue)
        timer.schedule(
            deadline: .now(), repeating: 1.0 / Double(kFrameRate), leeway: .seconds(0))
        timer.setEventHandler { [weak self] in self?.emitTestFrame() }
        _timer = timer
        timer.resume()
        os_log(.info, log: logger, "streaming started (test pattern)")
    }

    func stopStreaming() {
        if _streamingCounter > 1 {
            _streamingCounter -= 1
        } else {
            _streamingCounter = 0
            _timer?.cancel()
            _timer = nil
            os_log(.info, log: logger, "streaming stopped")
        }
    }

    // Draw one test-pattern frame (gray field, a white stripe sweeping down) and
    // send it on the source stream. Phase 3b replaces this body with a forward of
    // the sink's latest buffer.
    private func emitTestFrame() {
        var pixelBuffer: CVPixelBuffer?
        let err = CVPixelBufferPoolCreatePixelBufferWithAuxAttributes(
            kCFAllocatorDefault, _bufferPool,
            _bufferAuxAttributes as CFDictionary?, &pixelBuffer)
        guard err == kCVReturnSuccess, let pb = pixelBuffer else { return }

        CVPixelBufferLockBaseAddress(pb, [])
        if let base = CVPixelBufferGetBaseAddress(pb) {
            let width = CVPixelBufferGetWidth(pb)
            let height = CVPixelBufferGetHeight(pb)
            let rowBytes = CVPixelBufferGetBytesPerRow(pb)
            // Gray background.
            memset(base, 0x40, rowBytes * height)
            // A 24px white stripe at the current row (wrapping).
            let stripe = Int(_stripeRow) % height
            let top = base.advanced(by: stripe * rowBytes)
            for r in 0..<min(24, height - stripe) {
                memset(top.advanced(by: r * rowBytes), 0xFF, width * 4)
            }
        }
        CVPixelBufferUnlockBaseAddress(pb, [])
        _stripeRow = (_stripeRow + 4) % UInt32(kHeight)

        var sbuf: CMSampleBuffer?
        var timing = CMSampleTimingInfo(
            duration: CMTime(value: 1, timescale: Int32(kFrameRate)),
            presentationTimeStamp: CMClockGetTime(CMClockGetHostTimeClock()),
            decodeTimeStamp: .invalid)
        var fmt: CMFormatDescription?
        CMVideoFormatDescriptionCreateForImageBuffer(
            allocator: kCFAllocatorDefault, imageBuffer: pb, formatDescriptionOut: &fmt)
        guard
            CMSampleBufferCreateForImageBuffer(
                allocator: kCFAllocatorDefault, imageBuffer: pb, dataReady: true,
                makeDataReadyCallback: nil, refcon: nil, formatDescription: fmt!,
                sampleTiming: &timing, sampleBufferOut: &sbuf) == noErr,
            let sampleBuffer = sbuf
        else { return }

        _streamSource.stream.send(
            sampleBuffer, discontinuity: [],
            hostTimeInNanoseconds: UInt64(timing.presentationTimeStamp.seconds * Double(NSEC_PER_SEC)))
    }
}

// MARK: - Stream source (the .source stream apps select)

final class MacrdpCameraStreamSource: NSObject, CMIOExtensionStreamSource {
    private(set) var stream: CMIOExtensionStream!
    let device: CMIOExtensionDevice
    private let _streamFormat: CMIOExtensionStreamFormat
    private var _activeFormatIndex = 0

    init(
        localizedName: String, streamID: UUID, streamFormat: CMIOExtensionStreamFormat,
        device: CMIOExtensionDevice
    ) {
        self.device = device
        self._streamFormat = streamFormat
        super.init()
        stream = CMIOExtensionStream(
            localizedName: localizedName, streamID: streamID, direction: .source,
            clockType: .hostTime, source: self)
    }

    var formats: [CMIOExtensionStreamFormat] { [_streamFormat] }

    var availableProperties: Set<CMIOExtensionProperty> {
        [.streamActiveFormatIndex, .streamFrameDuration]
    }

    func streamProperties(forProperties properties: Set<CMIOExtensionProperty>) throws
        -> CMIOExtensionStreamProperties
    {
        let props = CMIOExtensionStreamProperties(dictionary: [:])
        if properties.contains(.streamActiveFormatIndex) {
            props.activeFormatIndex = _activeFormatIndex
        }
        if properties.contains(.streamFrameDuration) {
            props.frameDuration = CMTime(value: 1, timescale: Int32(kFrameRate))
        }
        return props
    }

    func setStreamProperties(_ streamProperties: CMIOExtensionStreamProperties) throws {
        if let idx = streamProperties.activeFormatIndex {
            _activeFormatIndex = idx
        }
    }

    func authorizedToStartStream(for client: CMIOExtensionClient) -> Bool { true }

    func startStream() throws {
        guard let deviceSource = device.source as? MacrdpCameraDeviceSource else {
            fatalError("unexpected device source type")
        }
        deviceSource.startStreaming()
    }

    func stopStream() throws {
        guard let deviceSource = device.source as? MacrdpCameraDeviceSource else {
            fatalError("unexpected device source type")
        }
        deviceSource.stopStreaming()
    }
}

// MARK: - Entry point

let providerSource = MacrdpCameraProviderSource(clientQueue: nil)
CMIOExtensionProvider.startService(provider: providerSource.provider)
CFRunLoopRun()
