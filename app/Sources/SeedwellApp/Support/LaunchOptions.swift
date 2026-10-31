import Foundation

/// Launch arguments. `--state-dir` and `--home-root` exist so that development and QA runs never
/// touch the real home folder or the real state (they must be given together).
struct LaunchOptions {
    var mock = false
    var stateDir: String?
    var homeRoot: String?
    var renderSnapshots: String?
    var showWindow = false
    /// Start in the menu bar only, even when launched like an app (tools/install-local.sh relaunching).
    var background = false
    var showSettings: String?
    /// QA only: a file the app polls for scene commands (see `QAControl`).
    var qaControl: String?
    /// QA only: allow trackers on loopback (a local mock tracker).
    var allowLoopbackTrackers = false
    var showWindowSize: CGSize?

    init(_ args: [String]) {
        func value(_ flag: String) -> String? {
            args.firstIndex(of: flag).flatMap { args.indices.contains($0 + 1) ? args[$0 + 1] : nil }
        }
        mock = args.contains("--mock") || args.contains("--render-snapshots")
        stateDir = value("--state-dir")
        homeRoot = value("--home-root")
        renderSnapshots = value("--render-snapshots")
        showWindow = args.contains("--show-window")
        background = args.contains("--background")
        showSettings = args.contains("--show-settings") ? (value("--show-settings") ?? "") : nil
        qaControl = value("--qa-control")
        allowLoopbackTrackers = args.contains("--qa-loopback-trackers")
        if let s = value("--window-size") {
            let p = s.split(separator: "x").compactMap { Double($0) }
            if p.count == 2 { showWindowSize = CGSize(width: p[0], height: p[1]) }
        }
    }

    /// The arguments an installer reopens this app with (SPEC 12A): the menu bar only, and an isolated run's own
    /// folders, so a test copy comes back on its own state.
    var relaunchArgs: [String] {
        var a = ["--background"]
        if let stateDir, let homeRoot { a += ["--state-dir", stateDir, "--home-root", homeRoot] }
        if let qaControl { a += ["--qa-control", qaControl] }
        if allowLoopbackTrackers { a.append("--qa-loopback-trackers") }
        return a
    }

    /// True when the run is isolated from the user's real folders.
    var isolated: Bool { stateDir != nil && homeRoot != nil }

    /// A QA or development argument was given without its partner: refuse to start rather than
    /// risk the real `~/Downloads`.
    var configurationError: String? {
        if mock { return nil }
        if (stateDir == nil) != (homeRoot == nil) {
            return "--state-dir and --home-root must be given together."
        }
        if allowLoopbackTrackers && !isolated { return "--qa-loopback-trackers needs --state-dir and --home-root." }
        // QA commands read the windows (chats among them): never on the user's own Seedwell, only an isolated copy.
        if qaControl != nil && !isolated { return "--qa-control needs --state-dir and --home-root." }
        return nil
    }

    /// Redirects `~` for everything in this process (the Rust core expands `~` from `$HOME`) and
    /// switches on the engine's real-home guard, which refuses folders inside the real home.
    func applyHomeRoot() {
        guard let homeRoot else { return }
        try? FileManager.default.createDirectory(atPath: homeRoot, withIntermediateDirectories: true)
        setenv("HOME", homeRoot, 1)
        setenv("CFFIXED_USER_HOME", homeRoot, 1)
    }

    /// The state directory: the isolated one, a temp one for mock runs, or the app's own.
    func resolvedStateDir() -> String {
        if let stateDir { return stateDir }
        if mock {
            return NSTemporaryDirectory() + "seedwell-mock-\(getpid())"
        }
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first?
            .appendingPathComponent(AppInfo.name).path
        return support ?? NSTemporaryDirectory() + AppInfo.name
    }
}
