// macrdp app-switcher HUD helper.
//
// A tiny AppKit process that draws a non-activating overlay panel showing the
// macrdp app switcher (Cmd+Tab / Option+Tab). macrdp captures the whole display
// via ScreenCaptureKit with NO window exclusions, so this real on-screen panel is
// captured for free and the remote RDP client sees it — at zero per-frame cost to
// macrdp's capture loop (unlike the old in-frame composited overlay).
//
// It must be a separate process because AppKit UI needs the main thread + a pumped
// runloop, and macrdp's main thread is owned by tokio with no AppKit runloop.
//
// IPC: listens on 127.0.0.1:$MACRDP_HUD_PORT (default 40243). macrdp connects and
// pushes framed commands (opcode u8, big-endian lengths):
//   SHOW(1):    [display_id:u32][cursor:u16][count:u16] then count×([pid:i32][name_len:u16][name_utf8])
//   ADVANCE(2): [cursor:u16]
//   HIDE(3):    (no payload)
// Best-effort: macrdp fire-and-forgets; if we're down the switch still works.
//
// SPIKE NOTE: this first cut renders a name row (no app icons yet) to validate the
// real-window-capture approach end-to-end. Icons land once the spike gate passes.

import AppKit
import Darwin
import UniformTypeIdentifiers

let HUD_PORT: UInt16 = {
    if let s = ProcessInfo.processInfo.environment["MACRDP_HUD_PORT"], let p = UInt16(s) {
        return p
    }
    return 40243
}()

// Parent (macrdp) pid to watch; if set and the parent dies, we exit so no stray
// helper lingers. macrdp passes MACRDP_HUD_PARENT.
let PARENT_PID: pid_t? = ProcessInfo.processInfo.environment["MACRDP_HUD_PARENT"].flatMap { Int32($0) }

struct AppEntry {
    let pid: pid_t
    let name: String
}

enum HudCommand {
    case show(displayID: CGDirectDisplayID, cursor: Int, apps: [AppEntry])
    case advance(cursor: Int)
    case hide
}

// MARK: - Overlay view (icon row, native Cmd+Tab look)

struct HudItem {
    let name: String
    let icon: NSImage
}

final class HudView: NSView {
    var items: [HudItem] = []
    var cursor: Int = 0

    static let icon: CGFloat = 72        // icon edge
    static let cellPad: CGFloat = 10     // padding around each icon (also the highlight inset)
    static let gap: CGFloat = 4
    static let outer: CGFloat = 16       // panel margin
    static let nameH: CGFloat = 26       // selected-name label strip below the row

    static var cell: CGFloat { icon + cellPad * 2 }

    static func size(for count: Int) -> NSSize {
        let n = max(count, 1)
        let w = outer * 2 + CGFloat(n) * cell + CGFloat(n - 1) * gap
        return NSSize(width: w, height: outer * 2 + cell + nameH)
    }

    override var isFlipped: Bool { false } // bottom-left origin

    override func draw(_ dirtyRect: NSRect) {
        let n = items.count
        guard n > 0 else { return }
        let rowY = Self.outer + Self.nameH // icons sit above the name strip
        for (i, item) in items.enumerated() {
            let cx = Self.outer + CGFloat(i) * (Self.cell + Self.gap)
            let cellRect = NSRect(x: cx, y: rowY, width: Self.cell, height: Self.cell)
            if i == cursor {
                let hl = NSBezierPath(roundedRect: cellRect, xRadius: 14, yRadius: 14)
                NSColor(white: 1.0, alpha: 0.25).setFill()
                hl.fill()
            }
            let iconRect = cellRect.insetBy(dx: Self.cellPad, dy: Self.cellPad)
            item.icon.draw(in: iconRect, from: .zero, operation: .sourceOver, fraction: 1.0)
        }
        // Selected app's name, centered across the whole panel.
        if cursor >= 0, cursor < n {
            let style = NSMutableParagraphStyle()
            style.alignment = .center
            style.lineBreakMode = .byTruncatingTail
            let attrs: [NSAttributedString.Key: Any] = [
                .foregroundColor: NSColor.white,
                .font: NSFont.systemFont(ofSize: 14, weight: .medium),
                .paragraphStyle: style,
            ]
            let strip = NSRect(x: Self.outer, y: Self.outer - 2,
                               width: bounds.width - Self.outer * 2, height: Self.nameH)
            items[cursor].name.draw(in: strip, withAttributes: attrs)
        }
    }
}

// MARK: - Non-activating panel

final class HudPanel: NSPanel {
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
}

// MARK: - Controller

final class HudController {
    private var panel: HudPanel?
    private let view = HudView()
    // Cache resolved icons per pid. NSRunningApplication.icon is flaky on repeated
    // calls (returns nil intermittently → blank cells across sessions); resolving
    // from the bundle on disk is deterministic, and caching makes it stick.
    private var iconCache: [pid_t: NSImage] = [:]
    private lazy var genericIcon: NSImage = NSWorkspace.shared.icon(for: .applicationBundle)

    private func icon(forPid pid: pid_t) -> NSImage {
        if let cached = iconCache[pid] { return cached }
        var img: NSImage?
        if let app = NSRunningApplication(processIdentifier: pid) {
            if let url = app.bundleURL {
                img = NSWorkspace.shared.icon(forFile: url.path) // reliable, non-nil for a real path
            }
            if img == nil { img = app.icon } // fallback for bundle-less procs
        }
        let resolved = img ?? genericIcon
        iconCache[pid] = resolved
        return resolved
    }

    func apply(_ cmd: HudCommand) {
        switch cmd {
        case let .show(displayID, cursor, apps):
            show(displayID: displayID, cursor: cursor, apps: apps)
        case let .advance(cursor):
            view.cursor = cursor
            view.needsDisplay = true
        case .hide:
            panel?.orderOut(nil)
        }
    }

    private func ensurePanel() -> HudPanel {
        if let p = panel { return p }
        let p = HudPanel(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 80),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false)
        p.level = .popUpMenu
        p.isOpaque = false
        p.backgroundColor = .clear
        p.hasShadow = false
        p.isFloatingPanel = true
        p.hidesOnDeactivate = false
        p.ignoresMouseEvents = true
        p.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .ignoresCycle, .stationary]

        // Rounded translucent HUD background.
        let blur = NSVisualEffectView()
        blur.material = .hudWindow
        blur.state = .active
        blur.blendingMode = .behindWindow
        blur.wantsLayer = true
        blur.layer?.cornerRadius = 16
        blur.layer?.masksToBounds = true
        blur.translatesAutoresizingMaskIntoConstraints = false
        view.translatesAutoresizingMaskIntoConstraints = false

        let container = NSView()
        container.addSubview(blur)
        container.addSubview(view)
        NSLayoutConstraint.activate([
            blur.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            blur.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            blur.topAnchor.constraint(equalTo: container.topAnchor),
            blur.bottomAnchor.constraint(equalTo: container.bottomAnchor),
            view.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            view.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            view.topAnchor.constraint(equalTo: container.topAnchor),
            view.bottomAnchor.constraint(equalTo: container.bottomAnchor),
        ])
        p.contentView = container
        panel = p
        return p
    }

    private func screen(for displayID: CGDirectDisplayID) -> NSScreen? {
        let key = NSDeviceDescriptionKey("NSScreenNumber")
        return NSScreen.screens.first {
            ($0.deviceDescription[key] as? NSNumber)?.uint32Value == displayID
        }
    }

    private func show(displayID: CGDirectDisplayID, cursor: Int, apps: [AppEntry]) {
        let p = ensurePanel()
        // Resolve each app's icon (main thread) via the cache → bundle on disk.
        let items = apps.map { HudItem(name: $0.name, icon: icon(forPid: $0.pid)) }
        view.items = items
        view.cursor = min(max(cursor, 0), max(items.count - 1, 0))
        view.needsDisplay = true

        let size = HudView.size(for: items.count)
        // NSScreen.frame is already global Cocoa (bottom-left) coords — no
        // CG→Cocoa conversion needed. Fall back to the main screen.
        let scr = screen(for: displayID) ?? NSScreen.main ?? NSScreen.screens.first
        let f = scr?.frame ?? NSRect(x: 0, y: 0, width: 1440, height: 900)
        let origin = NSPoint(x: f.midX - size.width / 2, y: f.midY - size.height / 2)
        p.setFrame(NSRect(origin: origin, size: size), display: true)
        // orderFrontRegardless, never makeKey — must not steal focus from the
        // app macrdp just AX-activated.
        p.orderFrontRegardless()
    }
}

// MARK: - Loopback IPC server (BSD sockets on a background thread)

final class IpcServer {
    private let onCommand: (HudCommand) -> Void

    init(onCommand: @escaping (HudCommand) -> Void) {
        self.onCommand = onCommand
    }

    func start() {
        Thread.detachNewThread { [weak self] in self?.run() }
    }

    private func run() {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else { FileHandle.standardError.write("hud: socket() failed\n".data(using: .utf8)!); return }
        var yes: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &yes, socklen_t(MemoryLayout<Int32>.size))

        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = HUD_PORT.bigEndian
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        let bound = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard bound == 0 else {
            FileHandle.standardError.write("hud: bind(\(HUD_PORT)) failed: \(String(cString: strerror(errno)))\n".data(using: .utf8)!)
            close(fd)
            return
        }
        guard listen(fd, 4) == 0 else { close(fd); return }
        FileHandle.standardError.write("hud: listening on 127.0.0.1:\(HUD_PORT)\n".data(using: .utf8)!)

        while true {
            let conn = accept(fd, nil, nil)
            if conn < 0 { continue }
            serve(conn)
            close(conn)
        }
    }

    private func readFull(_ fd: Int32, _ count: Int) -> Data? {
        var buf = Data(count: count)
        var got = 0
        let ok = buf.withUnsafeMutableBytes { (raw: UnsafeMutableRawBufferPointer) -> Bool in
            let base = raw.baseAddress!
            while got < count {
                let n = recv(fd, base + got, count - got, 0)
                if n <= 0 { return false }
                got += n
            }
            return true
        }
        return ok ? buf : nil
    }

    private func be16(_ d: Data, _ off: Int) -> Int { Int(d[off]) << 8 | Int(d[off + 1]) }
    private func be32(_ d: Data, _ off: Int) -> UInt32 {
        (UInt32(d[off]) << 24) | (UInt32(d[off + 1]) << 16) | (UInt32(d[off + 2]) << 8) | UInt32(d[off + 3])
    }
    private func i32(_ d: Data, _ off: Int) -> Int32 { Int32(bitPattern: be32(d, off)) }

    private func serve(_ fd: Int32) {
        while true {
            guard let op = readFull(fd, 1) else { return }
            switch op[op.startIndex] {
            case 1: // SHOW
                guard let head = readFull(fd, 8) else { return }
                let displayID = be32(head, 0)
                let cursor = be16(head, 4)
                let count = be16(head, 6)
                var apps: [AppEntry] = []
                apps.reserveCapacity(count)
                for _ in 0..<count {
                    guard let ph = readFull(fd, 6) else { return }
                    let pid = i32(ph, 0)
                    let nlen = be16(ph, 4)
                    let name: String
                    if nlen > 0 {
                        guard let nb = readFull(fd, nlen) else { return }
                        name = String(data: nb, encoding: .utf8) ?? "?"
                    } else {
                        name = "?"
                    }
                    apps.append(AppEntry(pid: pid, name: name))
                }
                emit(.show(displayID: displayID, cursor: cursor, apps: apps))
            case 2: // ADVANCE
                guard let b = readFull(fd, 2) else { return }
                emit(.advance(cursor: be16(b, 0)))
            case 3: // HIDE
                emit(.hide)
            default:
                return // unknown opcode → resync by dropping the connection
            }
        }
    }

    private func emit(_ cmd: HudCommand) {
        DispatchQueue.main.async { self.onCommand(cmd) }
    }
}

// MARK: - Parent-death watch

func watchParent(_ pid: pid_t) {
    Thread.detachNewThread {
        while true {
            if kill(pid, 0) != 0 { exit(0) } // parent gone
            Thread.sleep(forTimeInterval: 2.0)
        }
    }
}

// MARK: - Entry

let app = NSApplication.shared
app.setActivationPolicy(.accessory) // no Dock icon; safe runloop. (.prohibited is a post-spike option)
let controller = HudController()
let server = IpcServer { controller.apply($0) }
server.start()
if let pp = PARENT_PID { watchParent(pp) }
app.run()
