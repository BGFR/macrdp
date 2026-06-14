import AppKit

// macrdp Controller: a menu-bar app that controls the macrdp LaunchAgent
// (label com.clintcan.macrdp, installed by packaging/install-launchagent.sh)
// and toggles flags in config.env. It is a *controller* — quitting it leaves
// the server running under launchd. It needs no TCC grants of its own (it only
// runs `launchctl`, opens URLs, and edits files in the user's own Library);
// the Screen Recording / Accessibility grants belong to the macrdp binary.

final class AppController: NSObject, NSApplicationDelegate, NSMenuDelegate {
    let statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)

    /// The server's LaunchAgent label, derived from this controller's own bundle
    /// id by stripping the ".controller" suffix — so whatever BUNDLE_PREFIX the
    /// app was built with, the controller drives the matching agent. Falls back
    /// to the default prefix for unbundled `swift run` during development.
    let label: String = {
        if let bid = Bundle.main.bundleIdentifier, bid.hasSuffix(".controller") {
            return String(bid.dropLast(".controller".count))
        }
        return "com.clintcan.macrdp"
    }()

    var uid: String { String(getuid()) }
    var domain: String { "gui/\(uid)" }
    var service: String { "gui/\(uid)/\(label)" }

    var home: URL { FileManager.default.homeDirectoryForCurrentUser }
    var configURL: URL { home.appendingPathComponent("Library/Application Support/macrdp/config.env") }
    var logURL: URL { home.appendingPathComponent("Library/Logs/macrdp.log") }
    var plistURL: URL { home.appendingPathComponent("Library/LaunchAgents/\(label).plist") }

    var timer: Timer?

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory) // menu-bar only, no Dock icon
        if let button = statusItem.button {
            button.image = NSImage(systemSymbolName: "display", accessibilityDescription: "macrdp")
            button.image?.isTemplate = true
        }
        let menu = NSMenu()
        menu.delegate = self          // menuNeedsUpdate rebuilds on every open
        statusItem.menu = menu
        rebuildMenu()
        refreshGlyph()
        timer = Timer.scheduledTimer(withTimeInterval: 5.0, repeats: true) { [weak self] _ in
            self?.refreshGlyph()
        }
    }

    // MARK: - Agent state

    /// (loaded, pid). loaded=false means the agent isn't bootstrapped at all;
    /// pid=nil while loaded means installed but not currently running.
    func agentState() -> (loaded: Bool, pid: Int?) {
        let out = run("/bin/launchctl", ["print", service])
        guard out.code == 0 else { return (false, nil) }
        if let r = out.stdout.range(of: #"pid = (\d+)"#, options: .regularExpression) {
            let pid = out.stdout[r].split(separator: "=").last
                .flatMap { Int($0.trimmingCharacters(in: .whitespaces)) }
            return (true, pid)
        }
        return (true, nil)
    }

    func refreshGlyph() {
        let st = agentState()
        statusItem.button?.toolTip = st.pid != nil
            ? "macrdp: running (pid \(st.pid!))"
            : (st.loaded ? "macrdp: stopped" : "macrdp: not installed")
    }

    // MARK: - Menu

    func menuNeedsUpdate(_ menu: NSMenu) { rebuildMenu() }

    func rebuildMenu() {
        guard let menu = statusItem.menu else { return }
        menu.removeAllItems()

        let st = agentState()
        let header: String
        if !st.loaded { header = "macrdp — not installed" }
        else if let pid = st.pid { header = "macrdp — running (pid \(pid))" }
        else { header = "macrdp — stopped" }
        let h = NSMenuItem(title: header, action: nil, keyEquivalent: "")
        h.isEnabled = false
        menu.addItem(h)
        menu.addItem(.separator())

        let running = st.pid != nil
        if running {
            menu.addItem(item("Stop", #selector(stop)))
            menu.addItem(item("Restart", #selector(restart)))
        } else {
            let start = item("Start", #selector(start))
            start.isEnabled = st.loaded || FileManager.default.fileExists(atPath: plistURL.path)
            menu.addItem(start)
        }
        if !st.loaded && !FileManager.default.fileExists(atPath: plistURL.path) {
            let hint = NSMenuItem(title: "Run packaging/install-launchagent.sh first",
                                  action: nil, keyEquivalent: "")
            hint.isEnabled = false
            menu.addItem(hint)
        }
        menu.addItem(.separator())

        let cfg = readConfig()
        let opts = NSMenu()
        opts.addItem(toggle("H.264 video", key: "ENABLE_H264", cfg: cfg, sel: #selector(toggleH264)))
        opts.addItem(toggle("AAC audio", key: "ENABLE_AAC", cfg: cfg, sel: #selector(toggleAAC)))
        opts.addItem(toggle("HiDPI capture", key: "HIDPI", cfg: cfg, sel: #selector(toggleHiDPI)))
        let bind = cfg["BIND"] ?? "127.0.0.1:3390"
        let bindItem = NSMenuItem(title: "Bind: \(bind)", action: nil, keyEquivalent: "")
        bindItem.isEnabled = false
        opts.addItem(.separator())
        opts.addItem(bindItem)
        let optsItem = NSMenuItem(title: "Options", action: nil, keyEquivalent: "")
        optsItem.submenu = opts
        menu.addItem(optsItem)
        menu.addItem(item("Edit config…", #selector(editConfig)))
        menu.addItem(.separator())

        menu.addItem(item("Open Logs", #selector(openLogs)))
        let perm = NSMenu()
        perm.addItem(item("Screen Recording…", #selector(openScreenRecording)))
        perm.addItem(item("Accessibility…", #selector(openAccessibility)))
        let permItem = NSMenuItem(title: "Permissions", action: nil, keyEquivalent: "")
        permItem.submenu = perm
        menu.addItem(permItem)
        menu.addItem(.separator())

        menu.addItem(item("Quit Controller", #selector(quit)))
    }

    func item(_ title: String, _ sel: Selector) -> NSMenuItem {
        let i = NSMenuItem(title: title, action: sel, keyEquivalent: "")
        i.target = self
        return i
    }

    func toggle(_ title: String, key: String, cfg: [String: String], sel: Selector) -> NSMenuItem {
        let i = item(title, sel)
        i.state = (cfg[key] == "1") ? .on : .off
        return i
    }

    // MARK: - Actions

    @objc func start() {
        ensureLoaded()
        _ = run("/bin/launchctl", ["kickstart", "-k", service])
        refreshGlyph()
    }

    @objc func stop() {
        _ = run("/bin/launchctl", ["bootout", service])
        refreshGlyph()
    }

    @objc func restart() { start() }

    func ensureLoaded() {
        if !agentState().loaded && FileManager.default.fileExists(atPath: plistURL.path) {
            _ = run("/bin/launchctl", ["bootstrap", domain, plistURL.path])
            _ = run("/bin/launchctl", ["enable", service])
        }
    }

    @objc func toggleH264() { flip("ENABLE_H264") }
    @objc func toggleAAC() { flip("ENABLE_AAC") }
    @objc func toggleHiDPI() { flip("HIDPI") }

    func flip(_ key: String) {
        let cfg = readConfig()
        let next = (cfg[key] == "1") ? "0" : "1"
        writeConfig(key: key, value: next)
        // Apply live if the server is running.
        if agentState().pid != nil {
            _ = run("/bin/launchctl", ["kickstart", "-k", service])
        }
    }

    @objc func editConfig() { ensureConfigExists(); NSWorkspace.shared.open(configURL) }
    @objc func openLogs() {
        if !FileManager.default.fileExists(atPath: logURL.path) {
            FileManager.default.createFile(atPath: logURL.path, contents: Data())
        }
        NSWorkspace.shared.open(logURL)
    }
    @objc func openScreenRecording() {
        openURL("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
    }
    @objc func openAccessibility() {
        openURL("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
    }
    @objc func quit() { NSApp.terminate(nil) }

    func openURL(_ s: String) { if let u = URL(string: s) { NSWorkspace.shared.open(u) } }

    // MARK: - config.env IO

    func readConfig() -> [String: String] {
        guard let text = try? String(contentsOf: configURL, encoding: .utf8) else { return [:] }
        var d: [String: String] = [:]
        for raw in text.split(separator: "\n") {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.isEmpty || line.hasPrefix("#") { continue }
            guard let eq = line.firstIndex(of: "=") else { continue }
            let k = String(line[..<eq]).trimmingCharacters(in: .whitespaces)
            var v = String(line[line.index(after: eq)...]).trimmingCharacters(in: .whitespaces)
            v = v.trimmingCharacters(in: CharacterSet(charactersIn: "\""))
            d[k] = v
        }
        return d
    }

    func writeConfig(key: String, value: String) {
        ensureConfigExists()
        guard let text = try? String(contentsOf: configURL, encoding: .utf8) else { return }
        var lines = text.components(separatedBy: "\n")
        var found = false
        for (i, raw) in lines.enumerated() {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.hasPrefix("#") { continue }
            guard let eq = line.firstIndex(of: "=") else { continue }
            let k = String(line[..<eq]).trimmingCharacters(in: .whitespaces)
            if k == key { lines[i] = "\(key)=\(value)"; found = true; break }
        }
        if !found { lines.append("\(key)=\(value)") }
        try? lines.joined(separator: "\n").write(to: configURL, atomically: true, encoding: .utf8)
    }

    func ensureConfigExists() {
        let fm = FileManager.default
        try? fm.createDirectory(at: configURL.deletingLastPathComponent(),
                                withIntermediateDirectories: true)
        if !fm.fileExists(atPath: configURL.path) {
            let defaults = """
            BIND="127.0.0.1:3390"
            USE_KEYCHAIN=1
            ENABLE_H264=0
            ENABLE_AAC=0
            HIDPI=0
            EXTRA_FLAGS=""
            """
            try? defaults.write(to: configURL, atomically: true, encoding: .utf8)
        }
    }

    // MARK: - Process helper

    func run(_ path: String, _ args: [String]) -> (code: Int32, stdout: String) {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: path)
        p.arguments = args
        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = pipe
        do { try p.run() } catch { return (-1, "") }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        return (p.terminationStatus, String(data: data, encoding: .utf8) ?? "")
    }
}

let app = NSApplication.shared
let controller = AppController()
app.delegate = controller
app.run()
