import AppKit
import Foundation
import Sparkle

/// Updates, with two back ends picked once at launch (SPEC 12A); the Settings group, the toggles and the
/// menu bar row are shared and never branch on which one runs.
///
/// - Sparkle: only when the build carries an EdDSA public key and a feed (`SUPublicEDKey` + `SUFeedURL`,
///   a future Developer ID build). Sparkle drives everything, as before.
/// - GitHub: every other build. The core's `Updates` decides everything (when to check, what is on offer, when to
///   install by itself, the installer command, the texts); this runs its timer, its checks off the main thread and the
///   installer it hands over, and keeps its model for the views.
///
/// Never both: the other back end is not started, not even constructed.
@MainActor @Observable final class UpdateCenter {
    static let shared = UpdateCenter()

    enum Backend { case sparkle, github }

    let backend: Backend
    /// Release builds check and install by themselves; development builds only when asked.
    let releaseBuild: Bool
    /// What Settings › General › Updates draws (GitHub back end), from the core.
    private(set) var model: UpdatesModel?

    private let repoURL: String?
    private var updates: Updates?
    private var sparkle: SPUStandardUpdaterController?
    private let relaunch = Relaunch()
    private var schedule: Task<Void, Never>?
    private var refresher: Task<Void, Never>?

    /// Called just before an update quits the app (Sparkle's relaunch): quitting is allowed then.
    var willRelaunch: () -> Void {
        get { relaunch.run }
        set { relaunch.run = newValue }
    }

    private init() {
        let info = Bundle.main
        let key = (info.object(forInfoDictionaryKey: "SUPublicEDKey") as? String)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        let feed = (info.object(forInfoDictionaryKey: "SUFeedURL") as? String) ?? ""
        backend = !key.isEmpty && !feed.isEmpty ? .sparkle : .github
        releaseBuild = (info.object(forInfoDictionaryKey: "SeedwellReleaseBuild") as? Bool) ?? false
        // SEEDWELL_UPDATE_REPO (a loopback server in tests) is read by the core.
        repoURL = info.object(forInfoDictionaryKey: "SeedwellRepoURL") as? String
    }

    var currentVersion: String {
        updates?.currentVersion() ?? (Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String) ?? "0.0.0"
    }

    private static var nowMs: UInt64 { UInt64(Date().timeIntervalSince1970 * 1000) }

    // MARK: Launch

    /// Called once at launch.
    func start(core: Core) {
        switch backend {
        case .sparkle:
            guard sparkle == nil else { return }
            sparkle = SPUStandardUpdaterController(startingUpdater: true, updaterDelegate: relaunch, userDriverDelegate: nil)
        case .github:
            guard updates == nil else { return }
            let v = ProcessInfo.processInfo.operatingSystemVersion
            let u = Updates(core: core, releaseBuild: releaseBuild, repoUrl: repoURL,
                            systemVersion: "\(v.majorVersion).\(v.minorVersion).\(v.patchVersion)", nowMs: Self.nowMs)
            updates = u
            importEarlier(u)
            refresh()
            reschedule()
        }
    }

    /// Before 0.1.10 this app kept its update state in its user defaults: handed to the core once (it takes it only
    /// while it has no state of its own), so an update installed by that version is reported by this one.
    private func importEarlier(_ u: Updates) {
        let d = UserDefaults.standard
        let keys = ["update.lastCheck", "update.lastVersion", "update.lastNotes", "update.pending"]
        guard keys.contains(where: { d.object(forKey: $0) != nil }) else { return }
        let last = (d.object(forKey: keys[0]) as? Date).map { UInt64(max(0, $0.timeIntervalSince1970) * 1000) }
        u.importEarlier(lastCheckMs: last, offered: d.string(forKey: keys[1]), notes: d.string(forKey: keys[2]), pending: d.string(forKey: keys[3]))
        keys.forEach(d.removeObject(forKey:))
    }

    /// Settings loaded or changed (the core reads the two switches itself): the schedule and a waiting install follow.
    func settingsChanged() {
        guard backend == .github else { return }
        refresh()
        reschedule()
        considerAutoInstall()
    }

    // MARK: Shared settings bindings (the Sparkle back end keeps its own copy of the two switches)

    var checkAutomatically: Bool {
        get { sparkle?.updater.automaticallyChecksForUpdates ?? false }
        set { sparkle?.updater.automaticallyChecksForUpdates = newValue }
    }

    var installAutomatically: Bool {
        get { sparkle?.updater.automaticallyDownloadsUpdates ?? false }
        set { sparkle?.updater.automaticallyDownloadsUpdates = newValue }
    }

    // MARK: What the UI shows

    /// The version on offer from the GitHub back end (nil with Sparkle, which shows its own window).
    var availableVersion: String? { model?.availableVersion }

    var canCheck: Bool { backend == .sparkle ? (sparkle?.updater.canCheckForUpdates ?? false) : (model?.checkNow.enabled ?? false) }

    func checkNow() {
        switch backend {
        case .sparkle: sparkle?.checkForUpdates(nil)
        case .github: Task { await check(silent: false) }
        }
    }

    /// The model again now, then as often as it asks (every second while checking or installing, else a minute).
    private func refresh() {
        guard let updates else { return }
        let m = updates.model(nowMs: Self.nowMs)
        if m != model { model = m }
        refresher?.cancel()
        refresher = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(Int(m.refreshMs)))
            guard !Task.isCancelled else { return }
            self?.refresh()
        }
    }

    /// The timer for the next automatic check, at the time the core names (none: no automatic check).
    private func reschedule() {
        schedule?.cancel()
        guard let ms = updates?.nextDueMs(nowMs: Self.nowMs) else { return }
        schedule = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(Int(min(ms, UInt64(Int32.max)))))
            guard !Task.isCancelled, let self else { return }
            if self.updates?.nextDueMs(nowMs: Self.nowMs) == 0 { await self.check(silent: true) } else { self.reschedule() }
        }
    }

    /// One check, off the main thread (it blocks up to 15 s); the model shows "Checking…" meanwhile.
    private func check(silent: Bool) async {
        guard let updates else { return }
        let now = Self.nowMs
        let run = Task.detached { updates.check(silent: silent, nowMs: now) }
        try? await Task.sleep(for: .milliseconds(50))
        refresh()
        await run.value
        refresh()
        reschedule()
        considerAutoInstall()
    }

    // MARK: GitHub back end: installing

    /// The menu bar refreshed (on engine events): a waiting automatic install may start now that nothing downloads.
    func noteActivity() {
        guard model?.availableVersion != nil else { return }
        considerAutoInstall()
    }

    private func considerAutoInstall() {
        if let updates, model?.availableVersion != nil, updates.shouldInstall() { install() }
    }

    /// Starts the installer the core names, in a shell of its own session; it quits this app, replaces the bundle and
    /// reopens it. The core reports a failure (it could not start, or the app still runs after 90 s).
    func install() {
        guard let updates, let c = updates.install(installDir: Bundle.main.bundleURL.deletingLastPathComponent().path, nowMs: Self.nowMs) else { return }
        let log = URL(fileURLWithPath: c.log)
        do {
            try FileManager.default.createDirectory(at: log.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Spawn.detached(argv: c.argv, env: c.env.map { ($0.name, $0.value) }, log: log)
        } catch {
            updates.installFailed()
        }
        refresh()
    }

    func revealLog() {
        guard let path = updates?.logPath() else { return }
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
    }
}

/// Sparkle asks to quit the app to install an update; that quit has to go through.
private final class Relaunch: NSObject, SPUUpdaterDelegate, @unchecked Sendable {
    nonisolated(unsafe) var run: () -> Void = {}

    func updaterWillRelaunchApplication(_ updater: SPUUpdater) {
        MainActor.assumeIsolated { run() }
    }

    func updater(_ updater: SPUUpdater, willInstallUpdate item: SUAppcastItem) {
        MainActor.assumeIsolated { run() }
    }
}

// MARK: Detached process

enum Spawn {
    struct Failure: Error { let code: Int32 }

    /// posix_spawn in a new session (POSIX_SPAWN_SETSID): the child is not in this app's process group and
    /// is not touched when the app exits. stdin /dev/null, stdout and stderr appended to `log`; `env` is added to
    /// this app's environment.
    static func detached(argv: [String], env extra: [(String, String)] = [], log: URL) throws {
        var actions: posix_spawn_file_actions_t?
        posix_spawn_file_actions_init(&actions)
        defer { posix_spawn_file_actions_destroy(&actions) }
        posix_spawn_file_actions_addopen(&actions, 0, "/dev/null", O_RDONLY, 0)
        posix_spawn_file_actions_addopen(&actions, 1, log.path, O_WRONLY | O_CREAT | O_APPEND, 0o644)
        posix_spawn_file_actions_adddup2(&actions, 1, 2)
        var attr: posix_spawnattr_t?
        posix_spawnattr_init(&attr)
        defer { posix_spawnattr_destroy(&attr) }
        posix_spawnattr_setflags(&attr, Int16(POSIX_SPAWN_SETSID))
        var args: [UnsafeMutablePointer<CChar>?] = argv.map { strdup($0) } + [nil]
        var vars = ProcessInfo.processInfo.environment
        for (k, v) in extra { vars[k] = v }
        var env: [UnsafeMutablePointer<CChar>?] = vars.map { strdup("\($0.key)=\($0.value)") } + [nil]
        defer { for p in args { free(p) }; for p in env { free(p) } }
        var pid: pid_t = 0
        let rc = posix_spawn(&pid, argv[0], &actions, &attr, &args, &env)
        if rc != 0 { throw Failure(code: rc) }
    }
}
