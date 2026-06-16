import AppKit

// macrdp Controller: a menu-bar app that controls the macrdp LaunchAgent
// (label com.clintcan.macrdp, installed by packaging/install-launchagent.sh)
// and toggles flags in config.env. It is a *controller* — quitting it leaves
// the server running under launchd. It needs no TCC grants of its own (it only
// runs `launchctl`, opens URLs, and edits files in the user's own Library);
// the Screen Recording / Accessibility grants belong to the macrdp binary.

final class AppController: NSObject, NSApplicationDelegate, NSMenuDelegate {
    // Lazy so the controller can be instantiated for the headless --install-agent
    // path without touching the status bar (which needs a GUI app context).
    lazy var statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)

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
        let running = st.pid != nil
        // Dim the menu-bar icon when the server isn't running so state is
        // glanceable without opening the menu.
        statusItem.button?.alphaValue = running ? 1.0 : 0.4
        statusItem.button?.toolTip = running
            ? "macrdp: running (pid \(st.pid!))"
            : (st.loaded ? "macrdp: stopped" : "macrdp: not installed")
    }

    // MARK: - Server status (parsed from the log)

    /// Last ~32 KB of the server log split into lines (oldest first).
    func logTail() -> [String] {
        guard let h = try? FileHandle(forReadingFrom: logURL) else { return [] }
        defer { try? h.close() }
        let size = (try? h.seekToEnd()) ?? 0
        let window: UInt64 = 32 * 1024
        try? h.seek(toOffset: size > window ? size - window : 0)
        let data = (try? h.readToEnd()) ?? Data()
        return (String(data: data, encoding: .utf8) ?? "")
            .split(separator: "\n", omittingEmptySubsequences: true).map(String.init)
    }

    /// Latest TCC grant state the server logged (nil = not seen in recent log).
    /// The server logs "<X> permission already granted" / "<X> permission NOT
    /// granted" at startup; we can't query another process's TCC directly.
    func permissionStatus() -> (screen: Bool?, accessibility: Bool?) {
        var screen: Bool?
        var ax: Bool?
        for line in logTail() { // later lines win → most recent startup
            if line.contains("Screen Recording permission already granted") { screen = true } else if line
                .contains("Screen Recording permission NOT granted") { screen = false }
            if line.contains("Accessibility permission already granted") { ax = true } else if line
                .contains("Accessibility permission NOT granted") { ax = false }
        }
        return (screen, ax)
    }

    /// Most recent error worth surfacing (auth failure / port in use / panic).
    func lastServerError() -> String? {
        for raw in logTail().reversed() {
            let line = raw.replacingOccurrences(
                of: "\u{1b}\\[[0-9;]*m", with: "", options: .regularExpression)
            if line.contains("authentication failed") { return "Login failed — check the account password" }
            if line.contains("Address already in use") { return "Port in use — another server is bound to :3390" }
            if line.contains("panicked") { return "Server crashed — see Open Logs" }
        }
        return nil
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
        // Surface a server error (auth/port/crash) right under the header so a
        // silent crash-loop isn't invisible.
        if st.pid == nil, let err = lastServerError() {
            let e = NSMenuItem(title: "⚠️ \(err)", action: nil, keyEquivalent: "")
            e.isEnabled = false
            menu.addItem(e)
        }
        menu.addItem(.separator())

        let running = st.pid != nil
        if running {
            menu.addItem(item("Stop", #selector(stop)))
            menu.addItem(item("Restart", #selector(restart)))
        } else {
            // Start self-installs the LaunchAgent + onboards the password on
            // first run, so it's always actionable (no Terminal step needed).
            menu.addItem(item(st.loaded ? "Start" : "Start (first run sets up)", #selector(start)))
        }
        menu.addItem(.separator())

        let cfg = readConfig()
        let opts = NSMenu()
        opts.addItem(toggle("H.264 video", key: "ENABLE_H264", cfg: cfg, sel: #selector(toggleH264)))
        opts.addItem(toggle("AAC audio", key: "ENABLE_AAC", cfg: cfg, sel: #selector(toggleAAC)))
        opts.addItem(toggle("HiDPI capture", key: "HIDPI", cfg: cfg, sel: #selector(toggleHiDPI)))
        opts.addItem(toggle(
            "Un-minimize on Cmd+Tab", key: "UNMINIMIZE", cfg: cfg, sel: #selector(toggleUnminimize)))
        opts.addItem(.separator())
        let bind = cfg["BIND"] ?? "127.0.0.1:3390"
        let net = item("Allow network connections", #selector(toggleNetwork))
        net.state = bind.hasPrefix("0.0.0.0") ? .on : .off
        opts.addItem(net)
        let bindItem = NSMenuItem(title: "Listening on: \(bind)", action: nil, keyEquivalent: "")
        bindItem.isEnabled = false
        opts.addItem(bindItem)
        let optsItem = NSMenuItem(title: "Options", action: nil, keyEquivalent: "")
        optsItem.submenu = opts
        menu.addItem(optsItem)

        // Display: headless virtual display + blank-screen + resolution. A
        // virtual display at the client's resolution is captured 1:1 (no
        // scaling) and is snappier than mirroring a non-matching panel.
        let disp = NSMenu()
        let vdOn = cfg["VIRTUAL_DISPLAY"] == "1"
        let vd = item("Virtual display (headless)", #selector(toggleVirtualDisplay))
        vd.state = vdOn ? .on : .off
        disp.addItem(vd)
        // Primary-screen handling (radio). "detach" moves your apps onto the
        // virtual display so you can see/use them remotely; "capture" just
        // blanks the panel (apps stay on it). Both need the virtual display, so
        // picking one auto-enables it.
        let curMode: String = {
            if let m = cfg["PRIMARY_MODE"], !m.isEmpty { return m }
            return cfg["CAPTURE_PRIMARY"] == "1" ? "capture" : "none" // back-compat
        }()
        let primary = NSMenu()
        for (mode, label) in [
            ("none", "Keep local screen on"),
            ("detach", "Detach — move apps to remote"),
            ("capture", "Blank — keep apps on Mac"),
        ] {
            let mi = NSMenuItem(title: label, action: #selector(setPrimaryMode(_:)), keyEquivalent: "")
            mi.target = self
            mi.representedObject = mode
            mi.state = (curMode == mode) ? .on : .off
            primary.addItem(mi)
        }
        let primaryItem = NSMenuItem(title: "Primary screen", action: nil, keyEquivalent: "")
        primaryItem.submenu = primary
        disp.addItem(primaryItem)
        disp.addItem(.separator())
        let curW = cfg["VD_WIDTH"] ?? "1920"
        let curH = cfg["VD_HEIGHT"] ?? "1080"
        let resMenu = NSMenu()
        for (w, h, label) in Self.resolutions {
            let mi = NSMenuItem(title: label, action: #selector(setResolution(_:)), keyEquivalent: "")
            mi.target = self
            mi.representedObject = "\(w)x\(h)"
            mi.state = (curW == "\(w)" && curH == "\(h)") ? .on : .off
            resMenu.addItem(mi)
        }
        let resItem = NSMenuItem(title: "Virtual display resolution", action: nil, keyEquivalent: "")
        resItem.submenu = resMenu
        disp.addItem(resItem)
        let dispItem = NSMenuItem(title: "Display", action: nil, keyEquivalent: "")
        dispItem.submenu = disp
        menu.addItem(dispItem)

        menu.addItem(item("Edit config…", #selector(editConfig)))
        let pwTitle = hasKeychainPassword() ? "Change Account Password…" : "Set Account Password…"
        menu.addItem(item(pwTitle, #selector(setPassword)))
        menu.addItem(.separator())

        menu.addItem(item("Open Logs", #selector(openLogs)))
        // Permissions with live status parsed from the server log (✓ / needs
        // grant / unknown). Clicking opens the relevant System Settings pane.
        let ps = permissionStatus()
        let perm = NSMenu()
        perm.addItem(permItem("Screen Recording", ps.screen, #selector(openScreenRecording)))
        perm.addItem(permItem("Accessibility", ps.accessibility, #selector(openAccessibility)))
        let permRoot = NSMenuItem(title: "Permissions", action: nil, keyEquivalent: "")
        permRoot.submenu = perm
        menu.addItem(permRoot)
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

    /// A permission row with live status (✓ granted / needs grant / unknown).
    /// Always clickable — opens the relevant System Settings pane.
    func permItem(_ name: String, _ granted: Bool?, _ sel: Selector) -> NSMenuItem {
        let mark: String
        switch granted {
        case .some(true): mark = "✓"
        case .some(false): mark = "✗ needs grant"
        case .none: mark = "— open to grant"
        }
        let i = item("\(name): \(mark)", sel)
        i.state = (granted == true) ? .on : .off
        return i
    }

    // MARK: - Actions

    @objc func start() {
        // Self-install on first run: locate the server app, onboard the Keychain
        // password, write + register the LaunchAgent — no Terminal step needed.
        guard let serverApp = locateServerApp() else {
            alert(style: .warning, "Can't find macrdp.app",
                  "Move both macrdp.app and macrdp Controller into /Applications "
                  + "(or ~/Applications), then click Start again.")
            return
        }
        if !hasKeychainPassword() {
            guard promptAndStorePassword() else { return } // user cancelled
        }
        ensureConfigExists()
        let firstInstall = !FileManager.default.fileExists(atPath: plistURL.path)
        if firstInstall { installLaunchAgent(serverApp: serverApp) }
        ensureLoaded()
        _ = run("/bin/launchctl", ["kickstart", "-k", service])
        refreshGlyph()
        if firstInstall { remindPermissions() }
    }

    @objc func stop() {
        _ = run("/bin/launchctl", ["bootout", service])
        refreshGlyph()
    }

    @objc func restart() { start() }

    func ensureLoaded() {
        guard !agentState().loaded, FileManager.default.fileExists(atPath: plistURL.path) else { return }
        // `launchctl bootstrap` intermittently fails with EIO ("Bootstrap
        // failed: 5: Input/output error") right after a bootout; retry a few
        // times until the service registers.
        for _ in 0..<5 {
            _ = run("/bin/launchctl", ["bootstrap", domain, plistURL.path])
            if agentState().loaded { break }
            Thread.sleep(forTimeInterval: 0.5)
        }
        _ = run("/bin/launchctl", ["enable", service])
    }

    // MARK: - Headless entry (scripted/MDM deploy + testing)

    /// Runs the install logic without the GUI. `--print-paths` is side-effect
    /// free; `--install-agent` locates the server, writes + loads the agent
    /// (assumes the Keychain password is set separately for unattended deploys).
    func runHeadless(_ args: [String]) -> Int32 {
        if args.contains("--print-paths") {
            print("label:      \(label)")
            print("bind:       \(readConfig()["BIND"] ?? "127.0.0.1:3390")")
            print("server app: \(locateServerApp()?.path ?? "NOT FOUND")")
            print("plist:      \(plistURL.path)")
            print("config:     \(configURL.path)")
            print("log:        \(logURL.path)")
            print("password:   \(hasKeychainPassword() ? "set" : "MISSING")")
            return 0
        }
        guard let serverApp = locateServerApp() else {
            FileHandle.standardError.write(Data(
                "error: macrdp.app not found next to the controller or in /Applications\n".utf8))
            return 1
        }
        ensureConfigExists()
        installLaunchAgent(serverApp: serverApp)
        ensureLoaded()
        _ = run("/bin/launchctl", ["kickstart", "-k", service])
        print("installed: \(plistURL.path) -> \(serverApp.path)")
        if !hasKeychainPassword() {
            print("note: Keychain password not set — store it with:")
            print("  security add-generic-password -U -s macrdp -a \(NSUserName()) -w '<password>'")
        }
        return 0
    }

    // MARK: - Self-install

    /// Locate the server bundle (`macrdp.app`): next to this controller first
    /// (the usual case — both dragged into the same folder), then the standard
    /// install locations.
    func locateServerApp() -> URL? {
        let candidates = [
            Bundle.main.bundleURL.deletingLastPathComponent().appendingPathComponent("macrdp.app"),
            URL(fileURLWithPath: "/Applications/macrdp.app"),
            home.appendingPathComponent("Applications/macrdp.app"),
        ]
        let fm = FileManager.default
        return candidates.first {
            fm.fileExists(atPath: $0.appendingPathComponent("Contents/Resources/macrdp-launch").path)
        }
    }

    /// Write + register the LaunchAgent plist pointing at the located server's
    /// launch wrapper. Mirrors packaging/install-launchagent.sh, in-process.
    func installLaunchAgent(serverApp: URL) {
        let launch = serverApp.appendingPathComponent("Contents/Resources/macrdp-launch").path
        let dict: [String: Any] = [
            "Label": label,
            "ProgramArguments": [launch],
            "RunAtLoad": true,
            "KeepAlive": true,
            "StandardOutPath": logURL.path,
            "StandardErrorPath": logURL.path,
            "EnvironmentVariables": ["RUST_LOG": "info"],
        ]
        let fm = FileManager.default
        try? fm.createDirectory(at: plistURL.deletingLastPathComponent(),
                                withIntermediateDirectories: true)
        try? fm.createDirectory(at: logURL.deletingLastPathComponent(),
                                withIntermediateDirectories: true)
        if let data = try? PropertyListSerialization.data(fromPropertyList: dict,
                                                          format: .xml, options: 0) {
            try? data.write(to: plistURL)
        }
    }

    // MARK: - Keychain password onboarding

    /// The server (run headless by launchd) reads its account password from the
    /// Keychain via the `security` CLI, so we write it the same way — keeping the
    /// item's access context as /usr/bin/security so no read-time prompt appears.
    func hasKeychainPassword() -> Bool {
        run("/usr/bin/security", ["find-generic-password", "-s", "macrdp", "-a", NSUserName()]).code == 0
    }

    @discardableResult
    func promptAndStorePassword() -> Bool {
        let a = NSAlert()
        a.messageText = "Enter your macOS account password"
        a.informativeText = "macrdp authenticates RDP clients against your Mac account and "
            + "starts headless via launchd, so the password is stored in your login Keychain. "
            + "It never leaves this Mac."
        a.addButton(withTitle: "Save")
        a.addButton(withTitle: "Cancel")
        let field = NSSecureTextField(frame: NSRect(x: 0, y: 0, width: 260, height: 24))
        field.placeholderString = "Account password for \(NSUserName())"
        a.accessoryView = field
        a.window.initialFirstResponder = field
        NSApp.activate(ignoringOtherApps: true)
        guard a.runModal() == .alertFirstButtonReturn, !field.stringValue.isEmpty else { return false }
        let r = run("/usr/bin/security",
                    ["add-generic-password", "-U", "-s", "macrdp", "-a", NSUserName(),
                     "-w", field.stringValue])
        if r.code != 0 {
            alert(style: .critical, "Couldn't save password", "Keychain returned an error.")
            return false
        }
        return true
    }

    @objc func setPassword() { promptAndStorePassword() }

    func remindPermissions() {
        let a = NSAlert()
        a.messageText = "Grant macrdp two permissions"
        a.informativeText = "macrdp needs Screen Recording (to share the display) and "
            + "Accessibility (to forward keyboard/mouse). Enable macrdp.app in System "
            + "Settings → Privacy & Security, then it'll work."
        a.addButton(withTitle: "Open Privacy Settings")
        a.addButton(withTitle: "Later")
        NSApp.activate(ignoringOtherApps: true)
        if a.runModal() == .alertFirstButtonReturn { openScreenRecording() }
    }

    func alert(style: NSAlert.Style, _ message: String, _ info: String) {
        let a = NSAlert()
        a.alertStyle = style
        a.messageText = message
        a.informativeText = info
        a.addButton(withTitle: "OK")
        NSApp.activate(ignoringOtherApps: true)
        _ = a.runModal()
    }

    @objc func toggleH264() { flip("ENABLE_H264") }
    @objc func toggleAAC() { flip("ENABLE_AAC") }
    @objc func toggleHiDPI() { flip("HIDPI") }
    @objc func toggleUnminimize() { flip("UNMINIMIZE") }

    func flip(_ key: String) {
        let cfg = readConfig()
        let next = (cfg[key] == "1") ? "0" : "1"
        writeConfig(key: key, value: next)
        // Apply live if the server is running.
        if agentState().pid != nil {
            _ = run("/bin/launchctl", ["kickstart", "-k", service])
        }
    }

    /// Flip BIND between loopback-only (127.0.0.1) and all-interfaces (0.0.0.0),
    /// preserving the port. Confirms before exposing to the network.
    @objc func toggleNetwork() {
        let bind = readConfig()["BIND"] ?? "127.0.0.1:3390"
        let port = bind.split(separator: ":").last.map(String.init) ?? "3390"
        let enabling = !bind.hasPrefix("0.0.0.0")
        if enabling {
            let a = NSAlert()
            a.messageText = "Allow connections from the network?"
            a.informativeText = "macrdp will listen on all interfaces (0.0.0.0:\(port)), so "
                + "other devices on your network can connect. Access still requires TLS and your "
                + "macOS account password — but only enable this on a network you trust."
            a.addButton(withTitle: "Allow")
            a.addButton(withTitle: "Cancel")
            NSApp.activate(ignoringOtherApps: true)
            guard a.runModal() == .alertFirstButtonReturn else { return }
        }
        writeConfig(key: "BIND", value: "\(enabling ? "0.0.0.0" : "127.0.0.1"):\(port)")
        applyIfRunning()
    }

    // Standard 16:9 virtual-display resolutions, highest 1440p; default 1920×1080.
    static let resolutions: [(Int, Int, String)] = [
        (1280, 720, "1280 × 720"),
        (1600, 900, "1600 × 900"),
        (1920, 1080, "1920 × 1080 (1080p)"),
        (2560, 1440, "2560 × 1440 (1440p)"),
    ]

    @objc func toggleVirtualDisplay() {
        let on = readConfig()["VIRTUAL_DISPLAY"] == "1"
        writeConfig(key: "VIRTUAL_DISPLAY", value: on ? "0" : "1")
        // Turning the virtual display OFF resets the primary-screen mode to
        // none — detach/capture both require --virtual-display.
        if on {
            writeConfig(key: "PRIMARY_MODE", value: "none")
            writeConfig(key: "CAPTURE_PRIMARY", value: "0")
        }
        applyIfRunning()
    }

    /// Set how the physical screen is handled while connected:
    /// none / detach (move apps to the virtual display) / capture (blank it).
    @objc func setPrimaryMode(_ sender: NSMenuItem) {
        guard let mode = sender.representedObject as? String else { return }
        // detach/capture require the virtual display, so enable it.
        if mode != "none" { writeConfig(key: "VIRTUAL_DISPLAY", value: "1") }
        writeConfig(key: "PRIMARY_MODE", value: mode)
        // Clear the legacy boolean so it can't conflict with PRIMARY_MODE.
        writeConfig(key: "CAPTURE_PRIMARY", value: "0")
        applyIfRunning()
    }

    @objc func setResolution(_ sender: NSMenuItem) {
        guard let s = sender.representedObject as? String else { return }
        let parts = s.split(separator: "x")
        guard parts.count == 2 else { return }
        writeConfig(key: "VD_WIDTH", value: String(parts[0]))
        writeConfig(key: "VD_HEIGHT", value: String(parts[1]))
        applyIfRunning()
    }

    /// Re-exec the agent so config.env changes take effect, if it's running.
    func applyIfRunning() {
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
        // Always end with exactly one trailing newline — a config file with no
        // final newline makes a downstream append concatenate onto the last
        // key (which silently corrupted VD_HEIGHT + a new key once).
        let body = lines.joined(separator: "\n")
        let out = body.hasSuffix("\n") ? body : body + "\n"
        try? out.write(to: configURL, atomically: true, encoding: .utf8)
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
            UNMINIMIZE=0
            VIRTUAL_DISPLAY=0
            PRIMARY_MODE=none
            VD_WIDTH=1920
            VD_HEIGHT=1080
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

// Headless entry for scripted/MDM deploy + testing (no GUI, no status bar).
let cliArgs = CommandLine.arguments
if cliArgs.contains("--install-agent") || cliArgs.contains("--print-paths") {
    exit(AppController().runHeadless(cliArgs))
}

let app = NSApplication.shared
let controller = AppController()
app.delegate = controller
app.run()
