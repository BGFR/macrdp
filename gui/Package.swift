// swift-tools-version:5.9
import PackageDescription

// macrdp Controller — a menu-bar (tray) front-end that drives the macrdp
// LaunchAgent and config.env set up by packaging/. Built with plain SwiftPM
// (no .xcodeproj) so it stays CLI-buildable; make-tray-app.sh wraps the
// resulting executable into macrdpController.app.
let package = Package(
    name: "macrdptray",
    platforms: [.macOS(.v13)],
    targets: [
        .executableTarget(name: "macrdptray", path: "Sources/macrdptray")
    ]
)
