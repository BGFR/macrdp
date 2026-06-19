// swift-tools-version:5.9
import PackageDescription

// macrdp Controller — a menu-bar (tray) front-end that drives the macrdp
// LaunchAgent and config.env set up by packaging/. Built with plain SwiftPM
// (no .xcodeproj) so it stays CLI-buildable; make-tray-app.sh wraps the
// resulting executable into macrdpController.app.
// `macrdphud` is a second executable: the app-switcher HUD overlay helper that
// macrdp spawns and drives over loopback to draw a visual Cmd+Tab switcher the
// remote client sees. make-hud-helper.sh embeds it inside macrdp.app.
let package = Package(
    name: "macrdptray",
    platforms: [.macOS(.v13)],
    targets: [
        .executableTarget(name: "macrdptray", path: "Sources/macrdptray"),
        .executableTarget(name: "macrdphud", path: "Sources/macrdphud"),
    ]
)
