import AppKit
import Foundation
import Sparkle

/// Updates, with two back ends picked once at launch (SPEC 12A); the Settings group, the toggles and the
/// menu bar row are shared and never branch on which one runs.
///
/// - Sparkle: only when the build carries an EdDSA public key and a feed (`SUPublicEDKey` + `SUFeedURL`,
///   a future Developer ID build). Sparkle drives everything, as before.
/// - GitHub: every other build. Reads the `latest.json` asset of the latest GitHub release once a day and
///   installs through the curl installer (install.sh).
///
/// Never both: the other back end is not started, not even constructed.
@MainActor @Observable final class UpdateCenter {
    static let shared = UpdateCenter()

    enum Backend { case sparkle, github }

    enum Status: Equatable {
        case idle
        case checking
        case upToDate(checked: Date?)
        case available(version: String, notes: URL?)
        case noReleases
        case checkFailed(String)
        case installing(String)
        case installFailed(String)
        case updated(String)
    }

    let backend: Backend
    /// Release builds check and install by themselves; development builds only when asked.
    let releaseBuild: Bool
    private(set) var status: Status = .idle

    /// The two settings (`config.toml` `[updates]`), pushed in by the app on load and on every change.
    private(set) var prefs = UpdatePreferences(check: true, autoInstall: false)

    private let repoURL: String
    private let current: SemVer
    private let currentString: String
    private var sparkle: SPUStandardUpdaterController?
    private let relaunch = Relaunch()
    private var scheduler: NSBackgroundActivityScheduler?
    private var firstCheck: Task<Void, Never>?
    private var watchdog: Task<Void, Never>?
    private var busy = false
    private var checking = false
    private var installing = false
    @ObservationIgnored private let defaults = UserDefaults.standard

    private enum Key {
        static let lastCheck = "update.lastCheck"
        static let lastVersion = "update.lastVersion"
        static let lastNotes = "update.lastNotes"
        static let pending = "update.pending"
    }

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
        // SEEDWELL_UPDATE_REPO (read here, once): a base URL serving /releases/latest/download/latest.json and
        // /releases/download/v<ver>/install.sh, for tests on loopback. Unset in normal use.
        let envRepo = ProcessInfo.processInfo.environment["SEEDWELL_UPDATE_REPO"]
        repoURL = (envRepo?.isEmpty == false ? envRepo : nil)
            ?? (info.object(forInfoDictionaryKey: "SeedwellRepoURL") as? String)
            ?? "https://github.com/kkollsga/seedwell"
        currentString = (info.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String) ?? "0.0.0"
        current = SemVer(plain: currentString) ?? SemVer(0, 0, 0)
    }

    var currentVersion: String { currentString }

    // MARK: Launch

    /// Called once at launch.
    func start() {
        switch backend {
        case .sparkle:
            guard sparkle == nil else { return }
            let c = SPUStandardUpdaterController(startingUpdater: true, updaterDelegate: relaunch, userDriverDelegate: nil)
            sparkle = c
        case .github:
            restore()
            applySchedule()
            // Due, or an update found earlier is still on offer: check again (it may have been withdrawn), which
            // also starts a waiting automatic install. About a minute after launch, not during it.
            if automaticChecking,
               UpdateSchedule.isDue(lastCheck: defaults.object(forKey: Key.lastCheck) as? Date, now: Date()) || availableVersion != nil {
                firstCheck = Task { [weak self] in
                    try? await Task.sleep(for: .seconds(60))
                    guard !Task.isCancelled else { return }
                    await self?.check(silent: true)
                }
            }
        }
    }

    /// Settings loaded or changed.
    func apply(_ p: UpdatePreferences) {
        guard p != prefs else { return }
        prefs = p
        if backend == .github { applySchedule(); considerAutoInstall() }
    }

    private var automaticChecking: Bool { releaseBuild && prefs.check }

    // MARK: Shared settings bindings (the Sparkle back end keeps its own copy of the two switches)

    // With the GitHub back end the switches live in `config.toml` (the view binds to SettingsModel); the setters
    // below only matter for Sparkle.
    var checkAutomatically: Bool {
        get { access(keyPath: \.prefs); return backend == .sparkle ? (sparkle?.updater.automaticallyChecksForUpdates ?? false) : prefs.check }
        set { withMutation(keyPath: \.prefs) { sparkle?.updater.automaticallyChecksForUpdates = newValue } }
    }
    var installAutomatically: Bool {
        get { access(keyPath: \.prefs); return backend == .sparkle ? (sparkle?.updater.automaticallyDownloadsUpdates ?? false) : prefs.autoInstall }
        set { withMutation(keyPath: \.prefs) { sparkle?.updater.automaticallyDownloadsUpdates = newValue } }
    }
    /// The switches do something: always with Sparkle, in release builds with the GitHub back end.
    var switchesActive: Bool { backend == .sparkle ? sparkle != nil : releaseBuild }

    // MARK: What the UI shows

    /// The version on offer from the GitHub back end (nil with Sparkle, which shows its own window).
    var availableVersion: String? {
        if backend == .github, case .available(let v, _) = status { return v }
        return nil
    }

    var canCheck: Bool { backend == .sparkle ? (sparkle?.updater.canCheckForUpdates ?? false) : !checking && !installing }

    func checkNow() {
        switch backend {
        case .sparkle: sparkle?.checkForUpdates(nil)
        case .github: Task { await check(silent: false) }
        }
    }

    // MARK: GitHub back end: checking

    private func restore() {
        let pending = defaults.string(forKey: Key.pending)
        if let pending {
            defaults.removeObject(forKey: Key.pending)
            if pending == currentString {
                status = .updated(pending)
                defaults.removeObject(forKey: Key.lastVersion)
                return
            }
            status = .installFailed(pending)
            return
        }
        let last = defaults.object(forKey: Key.lastCheck) as? Date
        if let v = defaults.string(forKey: Key.lastVersion), let sv = SemVer(plain: v), sv > current {
            status = .available(version: v, notes: defaults.string(forKey: Key.lastNotes).flatMap(URL.init(string:)))
        } else if last != nil {
            status = .upToDate(checked: last)
        }
    }

    private func applySchedule() {
        guard automaticChecking else {
            scheduler?.invalidate(); scheduler = nil
            firstCheck?.cancel(); firstCheck = nil
            return
        }
        guard scheduler == nil else { return }
        let s = NSBackgroundActivityScheduler(identifier: "io.github.kkollsga.Seedwell.update-check")
        s.repeats = true
        s.interval = UpdateSchedule.interval
        s.tolerance = 6 * 3600
        s.qualityOfService = .utility
        s.schedule { [weak self] completion in
            Task { @MainActor in
                await self?.check(silent: true)
                completion(.finished)
            }
        }
        scheduler = s
    }

    /// One request. `silent`: a background check, which keeps the previous status on failure.
    private func check(silent: Bool) async {
        guard backend == .github, !checking, !installing else { return }
        checking = true
        let before = status
        if !silent { status = .checking }
        let outcome = await UpdateFetcher.fetch(repo: repoURL)
        checking = false
        let now = Date()
        switch outcome {
        case .release(let r):
            defaults.set(now, forKey: Key.lastCheck)
            if r.isOffered(current: current, macOS: Self.macOSVersion) {
                defaults.set(r.version.description, forKey: Key.lastVersion)
                defaults.set(r.notesURL?.absoluteString, forKey: Key.lastNotes)
                status = .available(version: r.version.description, notes: r.notesURL)
                considerAutoInstall()
            } else {
                defaults.removeObject(forKey: Key.lastVersion)
                defaults.removeObject(forKey: Key.lastNotes)
                status = .upToDate(checked: now)
            }
        case .none:
            defaults.set(now, forKey: Key.lastCheck)
            defaults.removeObject(forKey: Key.lastVersion)
            status = .noReleases
        case .failed(let why):
            status = silent && !isFailure(before) ? before : .checkFailed(why)
        }
    }

    private func isFailure(_ s: Status) -> Bool { if case .checkFailed = s { true } else { false } }

    private static var macOSVersion: SemVer {
        let v = ProcessInfo.processInfo.operatingSystemVersion
        return SemVer(v.majorVersion, v.minorVersion, v.patchVersion)
    }

    // MARK: GitHub back end: installing

    /// The app tells the updater whether anything is downloading or being moved (event-driven: the menu bar
    /// refreshes on engine events). A drop to idle may start a waiting automatic install.
    func noteActivity(busy now: Bool) {
        guard now != busy else { return }
        busy = now
        if !now { considerAutoInstall() }
    }

    private func considerAutoInstall() {
        guard backend == .github, case .available = status else { return }
        if AutoInstall.shouldInstall(releaseBuild: releaseBuild, checking: prefs.check, autoInstall: prefs.autoInstall,
                                     available: true, busy: busy, installing: installing) { install() }
    }

    /// Runs the curl installer in a shell of its own session; it quits this app, replaces the bundle and reopens it.
    func install() {
        guard backend == .github, !installing, case .available(let v, _) = status else { return }
        let log = Self.logURL
        do {
            try FileManager.default.createDirectory(at: log.deletingLastPathComponent(), withIntermediateDirectories: true)
            defaults.set(v, forKey: Key.pending)
            let argv = UpdateInstall.arguments(repo: repoURL, version: v, bundleURL: Bundle.main.bundleURL)
            try Spawn.detached(argv: argv, log: log)
        } catch {
            defaults.removeObject(forKey: Key.pending)
            status = .installFailed(v)
            return
        }
        installing = true
        status = .installing(v)
        // The installer quits this app within seconds. Still here after 90 s: it did not work.
        watchdog = Task { [weak self] in
            try? await Task.sleep(for: .seconds(90))
            guard !Task.isCancelled, let self else { return }
            self.installing = false
            self.defaults.removeObject(forKey: Key.pending)
            self.status = .installFailed(v)
        }
    }

    /// `~/Library/Logs/Seedwell/update.log`; under `--home-root` in an isolated (QA) run.
    static var logURL: URL {
        let home = LaunchOptions(CommandLine.arguments).homeRoot.map { URL(fileURLWithPath: $0) }
            ?? FileManager.default.homeDirectoryForCurrentUser
        return home.appendingPathComponent("Library/Logs/Seedwell/update.log")
    }

    func revealLog() {
        NSWorkspace.shared.activateFileViewerSelecting([Self.logURL])
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

// MARK: Network

enum UpdateFetchOutcome: Equatable {
    case release(LatestRelease)
    /// 404: no release has been published yet.
    case none
    case failed(String)
}

enum UpdateFetcher {
    /// One GET of `latest.json` through an ephemeral session (no cookies, no cache, no credentials),
    /// 15 s, no retries; the session is gone when this returns.
    static func fetch(repo: String) async -> UpdateFetchOutcome {
        guard let url = UpdateInstall.latestURL(repo: repo) else { return .failed("bad address") }
        let cfg = URLSessionConfiguration.ephemeral
        cfg.timeoutIntervalForRequest = 15
        cfg.timeoutIntervalForResource = 15
        cfg.waitsForConnectivity = false
        cfg.requestCachePolicy = .reloadIgnoringLocalCacheData
        let session = URLSession(configuration: cfg)
        defer { session.finishTasksAndInvalidate() }
        var req = URLRequest(url: url)
        let v = (Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String) ?? "0"
        req.setValue("Seedwell/\(v)", forHTTPHeaderField: "User-Agent")
        req.setValue("application/json", forHTTPHeaderField: "Accept")
        do {
            let (data, resp) = try await session.data(for: req)
            guard let http = resp as? HTTPURLResponse else { return .failed("no answer") }
            switch http.statusCode {
            case 200: return LatestRelease.parse(data).map { .release($0) } ?? .failed("unreadable release information")
            case 404: return .none
            default: return .failed("server answered \(http.statusCode)")
            }
        } catch let e as URLError {
            switch e.code {
            case .notConnectedToInternet, .networkConnectionLost: return .failed("no internet connection")
            case .timedOut: return .failed("timed out")
            case .cannotFindHost, .dnsLookupFailed: return .failed("server not found")
            default: return .failed("network error")
            }
        } catch {
            return .failed("network error")
        }
    }
}

// MARK: Detached process

enum Spawn {
    struct Failure: Error { let code: Int32 }

    /// posix_spawn in a new session (POSIX_SPAWN_SETSID): the child is not in this app's process group and
    /// is not touched when the app exits. stdin /dev/null, stdout and stderr appended to `log`.
    static func detached(argv: [String], log: URL) throws {
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
        var env: [UnsafeMutablePointer<CChar>?] = ProcessInfo.processInfo.environment.map { strdup("\($0.key)=\($0.value)") } + [nil]
        defer { for p in args { free(p) }; for p in env { free(p) } }
        var pid: pid_t = 0
        let rc = posix_spawn(&pid, argv[0], &actions, &attr, &args, &env)
        if rc != 0 { throw Failure(code: rc) }
    }
}
