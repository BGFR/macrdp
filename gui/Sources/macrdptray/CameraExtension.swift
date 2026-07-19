import AppKit
import Foundation
import SystemExtensions
import os.log

// Camera-redirection Phase 3 — activate / deactivate the "macrdp Camera"
// CoreMediaIO **system extension** from the menu-bar controller.
//
// `OSSystemExtensionRequest` requires that THIS controller app (a) embeds the
// extension bundle at `Contents/Library/SystemExtensions/macrdp-camera.systemextension`
// and (b) carries the `com.apple.developer.system-extension.install` entitlement
// (self-serviceable in the Apple Developer portal; written into the controller's
// provisioning profile). The user approves the install once in System Settings →
// Privacy & Security ("System software from … was blocked … Allow"). Once active,
// macrdp.app (the Rust server) feeds the extension's sink stream with the decoded
// redirected webcam — a separate CMIO client, so it needs neither the extension
// nor this entitlement. See ~/.claude/plans/camera-redirection-phase3.md.
//
// The extension activates/deactivates independently of the launchd server: it
// stays installed across server restarts and auto-uninstalls when this controller
// app is deleted.

private let camLog = OSLog(subsystem: "com.clintcan.macrdp.controller", category: "camera-ext")

final class CameraExtensionManager: NSObject, OSSystemExtensionRequestDelegate {
    static let shared = CameraExtensionManager()

    /// MUST match the embedded extension bundle's `CFBundleIdentifier`
    /// (packaging/make-camera-extension.sh + camera-Info.plist). The extension id is
    /// a **child of this controller app's id** — macOS enforces that an embedded
    /// system extension is prefixed by the host app's id — so it's simply
    /// `<controller-id>.camera`, correct for any BUNDLE_PREFIX.
    var extensionIdentifier: String {
        if let bid = Bundle.main.bundleIdentifier {
            return bid + ".camera"
        }
        return "com.clintcan.macrdp.controller.camera"
    }

    private var onResult: ((Result<String, Error>) -> Void)?

    func activate(_ completion: @escaping (Result<String, Error>) -> Void) {
        onResult = completion
        os_log(.info, log: camLog, "requesting activation of %{public}@", extensionIdentifier)
        let req = OSSystemExtensionRequest.activationRequest(
            forExtensionWithIdentifier: extensionIdentifier, queue: .main)
        req.delegate = self
        OSSystemExtensionManager.shared.submitRequest(req)
    }

    func deactivate(_ completion: @escaping (Result<String, Error>) -> Void) {
        onResult = completion
        os_log(.info, log: camLog, "requesting deactivation of %{public}@", extensionIdentifier)
        let req = OSSystemExtensionRequest.deactivationRequest(
            forExtensionWithIdentifier: extensionIdentifier, queue: .main)
        req.delegate = self
        OSSystemExtensionManager.shared.submitRequest(req)
    }

    // MARK: OSSystemExtensionRequestDelegate

    func request(
        _ request: OSSystemExtensionRequest,
        actionForReplacingExtension existing: OSSystemExtensionProperties,
        withExtension ext: OSSystemExtensionProperties
    ) -> OSSystemExtensionRequest.ReplacementAction {
        os_log(
            .info, log: camLog, "replacing extension %{public}@ -> %{public}@",
            existing.bundleShortVersion, ext.bundleShortVersion)
        // Always take the copy bundled in this app (covers upgrades + reinstalls).
        return .replace
    }

    func requestNeedsUserApproval(_ request: OSSystemExtensionRequest) {
        os_log(.info, log: camLog, "activation needs user approval")
        onResult?(.success("needs-approval"))
        onResult = nil
    }

    func request(
        _ request: OSSystemExtensionRequest,
        didFinishWithResult result: OSSystemExtensionRequest.Result
    ) {
        os_log(.info, log: camLog, "request finished: %{public}d", result.rawValue)
        onResult?(.success(result == .willCompleteAfterReboot ? "reboot-required" : "active"))
        onResult = nil
    }

    func request(_ request: OSSystemExtensionRequest, didFailWithError error: Error) {
        os_log(.error, log: camLog, "request failed: %{public}@", error.localizedDescription)
        onResult?(.failure(error))
        onResult = nil
    }
}

// The menu action lives on AppController (Swift lets a class be extended across
// files in the same module) so main.swift only needs the one menu-item line.
extension AppController {
    @objc func enableCameraRedirection() {
        CameraExtensionManager.shared.activate { [weak self] result in
            DispatchQueue.main.async { self?.presentCameraResult(result) }
        }
    }

    @objc func disableCameraRedirection() {
        CameraExtensionManager.shared.deactivate { [weak self] result in
            DispatchQueue.main.async { self?.presentCameraResult(result) }
        }
    }

    private func presentCameraResult(_ result: Result<String, Error>) {
        let alert = NSAlert()
        switch result {
        case .success("needs-approval"):
            alert.messageText = "Approve “macrdp Camera”"
            alert.informativeText =
                "macOS blocked the camera extension pending your approval. Open System "
                + "Settings → Privacy & Security, scroll to the message about system software "
                + "from your developer identity, and click Allow. Then reconnect a client with a "
                + "redirected webcam."
            alert.addButton(withTitle: "Open Privacy & Security")
            alert.addButton(withTitle: "Later")
            if alert.runModal() == .alertFirstButtonReturn {
                if let url = URL(
                    string: "x-apple.systempreferences:com.apple.preference.security?Privacy")
                {
                    NSWorkspace.shared.open(url)
                }
            }
        case .success("reboot-required"):
            alert.messageText = "Restart to finish"
            alert.informativeText =
                "The macrdp Camera extension will finish installing after you restart the Mac."
            alert.runModal()
        case .success:
            alert.messageText = "macrdp Camera enabled"
            alert.informativeText =
                "“macrdp Camera” is now available in Photo Booth / Zoom / FaceTime. It shows a "
                + "test pattern until a client redirects a webcam."
            alert.runModal()
        case .failure(let error):
            alert.alertStyle = .warning
            alert.messageText = "Couldn’t enable macrdp Camera"
            alert.informativeText = error.localizedDescription
            alert.runModal()
        }
    }
}
