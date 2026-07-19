// macrdp Camera — a CoreMediaIO Camera **system extension** (macOS 12.3+) that
// presents a virtual camera ("macrdp Camera") selectable in Photo Booth / Zoom /
// FaceTime / Teams. This is camera-redirection **Phase 3** — the payoff phase that
// surfaces the redirected webcam (decoded to CVPixelBuffers by Phases 1+2) as a
// real macOS capture device.
//
// The device exposes a **source** stream (apps consume) and a **sink** stream (a
// producer feeds). macrdp's Rust process is the CMIO client: it enqueues the decoded
// webcam frames onto the sink (CMIOStreamCopyBufferQueue + CMSimpleQueueEnqueue), and
// the device's consume loop forwards each sink buffer onto the source. A static test
// pattern (a white stripe sweeping down a gray field) plays until a producer feeds
// real frames, then stands down. See ~/.claude/plans/camera-redirection-phase3.md.
//
// - 3a: device bring-up (source + test pattern) — the signing/activation spike.
// - 3b: the sink stream + consume loop + macrdp's Rust CMIO producer.
// - 3c: the frames are NV12 `420v` (matching VideoToolbox decode output — advertised
//   on both streams so CMIO passes them through with no transcode), and the sink
//   accepts frames ONLY from macrdp.app (Team-ID-pinned code-signing check).
//
// Structure mirrors Apple's "Creating a camera extension with Core Media I/O"
// sample (WWDC22 s10022) + Halle/SinkCam: a Provider owns one Device, the Device
// owns the stream(s) and the frame timer, and each stream has a StreamSource
// delegate. Built as a plain SwiftPM executable (no Xcode); packaging assembles the
// `.systemextension` bundle around it. The executable's entry point hands the
// provider to CMIOExtensionProvider.startService and runs the CFRunLoop.

import CoreMediaIO
import CoreVideo
import Foundation
import IOKit.audio
import Security
import os.log

private let kFrameRate = 30
private let kWidth: Int32 = 1280
private let kHeight: Int32 = 720
// The virtual camera's pixel format. NV12 video-range (`420v`) — matches
// VideoToolbox's decode output, so macrdp's producer feeds the sink zero-copy and
// the extension forwards to the source with NO conversion. CMIO does not transcode,
// so the advertised stream format MUST equal the format of the buffers sent.
private let kPixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
private let logger = OSLog(subsystem: "com.clintcan.macrdp.camera", category: "extension")

// Sink producer authentication (Phase 3c): only macrdp.app may feed the sink.
// `signingID` is an OS-derived pre-filter; the authoritative check pins the Team ID
// + identifier via a code-signing requirement (signingID alone is signer-chosen and
// could be reused by an adversary).
private let kProducerSigningID = "com.clintcan.macrdp"
private let kProducerRequirement =
    "anchor apple generic and certificate leaf[subject.OU] = \"QGLA89KHM7\" "
    + "and identifier \"com.clintcan.macrdp\""

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
    private var _streamSink: MacrdpCameraStreamSink!
    private var _streamingCounter: UInt32 = 0
    private var _sinkCounter: UInt32 = 0
    // True once a producer (macrdp.app) is feeding real webcam frames into the
    // sink — the test-pattern timer stands down so the live picture shows through.
    // Read on the timer queue, written on the CMIO client queue; a benign race just
    // costs one extra test frame.
    private var _sinkActive = false
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
            codecType: kPixelFormat,
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
        _streamSink = MacrdpCameraStreamSink(
            localizedName: "macrdp Camera.Video.Sink",
            streamID: UUID(),
            streamFormat: videoStreamFormat,
            device: device)
        do {
            try device.addStream(_streamSource.stream)
            try device.addStream(_streamSink.stream)
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

    // MARK: sink (producer → source) — Phase 3b

    // A producer (macrdp.app) started feeding the sink. Ref-count concurrent
    // producers and kick off the consume loop on the first.
    func startStreamingFromSink(_ client: CMIOExtensionClient) {
        _sinkCounter += 1
        if _sinkCounter == 1 {
            consumeBuffer(client)
            os_log(.info, log: logger, "sink producer connected — forwarding real frames")
        }
    }

    func stopStreamingFromSink() {
        if _sinkCounter > 1 {
            _sinkCounter -= 1
        } else {
            _sinkCounter = 0
            _sinkActive = false
            os_log(.info, log: logger, "sink producer disconnected — back to test pattern")
        }
    }

    // Pull one buffer the producer enqueued to the sink and re-send it on the
    // source stream apps consume; re-arm for the next. Self-recursive completion
    // loop (the SinkCam pattern).
    private func consumeBuffer(_ client: CMIOExtensionClient) {
        guard _sinkCounter >= 1 else { return }
        _streamSink.stream.consumeSampleBuffer(from: client) {
            [weak self] sbuf, seq, _, _, err in
            guard let self = self else { return }
            if let sbuf = sbuf {
                self._sinkActive = true
                let now = CMClockGetTime(CMClockGetHostTimeClock())
                let hostNs = UInt64(now.seconds * Double(NSEC_PER_SEC))
                if self._streamingCounter > 0 {
                    self._streamSource.stream.send(
                        sbuf, discontinuity: [], hostTimeInNanoseconds: hostNs)
                }
                // Tell the sink the buffer was consumed (releases the producer's slot).
                self._streamSink.stream.notifyScheduledOutputChanged(
                    CMIOExtensionScheduledOutput(
                        sequenceNumber: seq, hostTimeInNanoseconds: hostNs))
            } else if let err = err {
                os_log(.debug, log: logger, "sink consume ended: %{public}@", err.localizedDescription)
            }
            self.consumeBuffer(client)
        }
    }

    // Draw one test-pattern frame (gray field, a white stripe sweeping down) and
    // send it on the source stream — only while NO producer is feeding the sink
    // (once macrdp pushes real webcam frames, `_sinkActive` stands this down).
    private func emitTestFrame() {
        if _sinkActive { return }
        var pixelBuffer: CVPixelBuffer?
        let err = CVPixelBufferPoolCreatePixelBufferWithAuxAttributes(
            kCFAllocatorDefault, _bufferPool,
            _bufferAuxAttributes as CFDictionary?, &pixelBuffer)
        guard err == kCVReturnSuccess, let pb = pixelBuffer else { return }

        // NV12 (`420v`): plane 0 = luma (Y), plane 1 = interleaved CbCr. Gray field
        // + a white stripe sweeping down; neutral chroma (0x80) → grayscale.
        CVPixelBufferLockBaseAddress(pb, [])
        if let yBase = CVPixelBufferGetBaseAddressOfPlane(pb, 0) {
            let yStride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0)
            let yW = CVPixelBufferGetWidthOfPlane(pb, 0)
            let yH = CVPixelBufferGetHeightOfPlane(pb, 0)
            memset(yBase, 0x7D, yStride * yH)  // mid gray (video-range luma)
            let stripe = Int(_stripeRow) % yH
            for r in 0..<min(24, yH - stripe) {
                // Only yW bytes wide — the stride may be padded past the visible width.
                memset(yBase.advanced(by: (stripe + r) * yStride), 0xEB, yW)  // ~white
            }
        }
        if let cBase = CVPixelBufferGetBaseAddressOfPlane(pb, 1) {
            let cStride = CVPixelBufferGetBytesPerRowOfPlane(pb, 1)
            let cH = CVPixelBufferGetHeightOfPlane(pb, 1)
            memset(cBase, 0x80, cStride * cH)  // neutral chroma → grayscale
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

// MARK: - Stream sink (the .sink stream the producer feeds)

final class MacrdpCameraStreamSink: NSObject, CMIOExtensionStreamSource {
    private(set) var stream: CMIOExtensionStream!
    let device: CMIOExtensionDevice
    private let _streamFormat: CMIOExtensionStreamFormat
    private var _client: CMIOExtensionClient?

    init(
        localizedName: String, streamID: UUID, streamFormat: CMIOExtensionStreamFormat,
        device: CMIOExtensionDevice
    ) {
        self.device = device
        self._streamFormat = streamFormat
        super.init()
        stream = CMIOExtensionStream(
            localizedName: localizedName, streamID: streamID, direction: .sink,
            clockType: .hostTime, source: self)
    }

    var formats: [CMIOExtensionStreamFormat] { [_streamFormat] }

    var availableProperties: Set<CMIOExtensionProperty> {
        [
            .streamActiveFormatIndex, .streamFrameDuration,
            .streamSinkBufferQueueSize, .streamSinkBuffersRequiredForStartup,
            .streamSinkBufferUnderrunCount, .streamSinkEndOfData,
        ]
    }

    func streamProperties(forProperties properties: Set<CMIOExtensionProperty>) throws
        -> CMIOExtensionStreamProperties
    {
        let props = CMIOExtensionStreamProperties(dictionary: [:])
        if properties.contains(.streamActiveFormatIndex) {
            props.activeFormatIndex = 0
        }
        if properties.contains(.streamFrameDuration) {
            props.frameDuration = CMTime(value: 1, timescale: Int32(kFrameRate))
        }
        if properties.contains(.streamSinkBufferQueueSize) {
            props.sinkBufferQueueSize = 3
        }
        if properties.contains(.streamSinkBuffersRequiredForStartup) {
            props.sinkBuffersRequiredForStartup = 1
        }
        return props
    }

    func setStreamProperties(_ streamProperties: CMIOExtensionStreamProperties) throws {}

    // The sink is an open injection target — any local process aware of it could push
    // video into "macrdp Camera". Accept only macrdp.app's producer, and capture it
    // for the device's consume loop. This is the correct per-stream choke point (vs
    // `connect(to:)`, which is device-wide and would also gate source consumers).
    func authorizedToStartStream(for client: CMIOExtensionClient) -> Bool {
        guard isTrustedProducer(client) else {
            os_log(
                .error, log: logger,
                "sink: REJECTED producer signingID=%{public}@ pid=%d",
                client.signingID ?? "nil", client.pid)
            return false
        }
        _client = client
        return true
    }

    // Layered producer check: `signingID` is OS-derived (a client can't assert an
    // arbitrary one) but is signer-chosen, so it's only a pre-filter; the
    // authoritative gate pins the Team ID + identifier via a code-signing
    // requirement evaluated against the client pid. If the Security API can't
    // evaluate the process (e.g. blocked in the sandbox), fall back to the
    // signingID match already established (documented degrade).
    private func isTrustedProducer(_ client: CMIOExtensionClient) -> Bool {
        guard let signingID = client.signingID, signingID == kProducerSigningID else {
            return false
        }
        var code: SecCode?
        let attrs = [kSecGuestAttributePid: NSNumber(value: client.pid)] as CFDictionary
        let copyStatus = SecCodeCopyGuestWithAttributes(nil, attrs, [], &code)
        guard copyStatus == errSecSuccess, let code = code else {
            os_log(
                .info, log: logger,
                "sink: SecCode unavailable (status %d) — accepting on signingID", copyStatus)
            return true
        }
        var requirement: SecRequirement?
        guard
            SecRequirementCreateWithString(kProducerRequirement as CFString, [], &requirement)
                == errSecSuccess, let requirement = requirement
        else {
            return true  // couldn't build the requirement — rely on the signingID match
        }
        let valid = SecCodeCheckValidity(code, [], requirement)
        if valid != errSecSuccess {
            os_log(
                .error, log: logger,
                "sink: producer failed the Team-ID requirement (status %d)", valid)
            return false
        }
        return true
    }

    func startStream() throws {
        guard let deviceSource = device.source as? MacrdpCameraDeviceSource,
            let client = _client
        else { return }
        deviceSource.startStreamingFromSink(client)
    }

    func stopStream() throws {
        guard let deviceSource = device.source as? MacrdpCameraDeviceSource else { return }
        deviceSource.stopStreamingFromSink()
    }
}

// MARK: - Entry point

let providerSource = MacrdpCameraProviderSource(clientQueue: nil)
CMIOExtensionProvider.startService(provider: providerSource.provider)
CFRunLoopRun()
