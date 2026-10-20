import Foundation
import Observation

enum SettingsTab: String, CaseIterable, Identifiable {
    case general, transfers, folders, rules, network, agents, advanced
    var id: String { rawValue }
    var title: String { rawValue.capitalized }
    var symbol: String {
        switch self {
        case .general: "gearshape"
        case .transfers: "arrow.up.arrow.down"
        case .folders: "folder"
        case .rules: "list.bullet"
        case .network: "network"
        case .agents: "sparkle"
        case .advanced: "command"
        }
    }
}

/// The Settings window's state: typed settings from the core (`config.toml`), rules and tracker
/// profiles, saved straight back. Changes are written a moment after the last edit; the engine
/// picks the file up by itself. External edits of the files arrive through `reload()`.
@MainActor @Observable final class SettingsModel {
    let core: Core
    /// Every tab's settings; edits are saved (debounced).
    var app: AppSettings {
        didSet { if !loading, app != oldValue { scheduleSave(old: oldValue) } }
    }
    /// Ordered rules and the Default rule; rule edits save through `saveRules()`.
    var rulesDoc: RulesDoc
    var profiles: [ProfileRecord]
    private(set) var problems: [String] = []
    private(set) var warnings: [String] = []
    private(set) var restartRequired: [String] = []
    private(set) var volumeWarnings: [VolumeWarning] = []
    /// Set when the configured listening port was taken at start and the engine listens elsewhere for now.
    private(set) var portFallback: PortFallbackInfo?
    private(set) var activity: [AgentActivity] = []
    /// The port the engine listens on now: the manual one, or the random one picked at this start.
    private(set) var activePort: UInt16 = 0
    /// Whether peers on the Internet can reach the port: a peer that connected in, or the router's answer.
    private(set) var reachability: ReachabilityInfo?
    @ObservationIgnored private var portWatch: Task<Void, Never>?
    /// Called after a save changed the finished location: the old and new value.
    @ObservationIgnored var finishedLocationChanged: ((_ old: String, _ new: String, _ category: Category?) -> Void)?
    /// Set by the app delegate; reacts to settings the shell itself implements (launch at login, ...).
    @ObservationIgnored var applied: ((AppSettings) -> Void)?
    @ObservationIgnored private var loading = false
    @ObservationIgnored private var saveTask: Task<Void, Never>?
    @ObservationIgnored private var lastSaved: AppSettings

    init(core: Core) {
        self.core = core
        let s = core.settingsLoad()
        app = s
        lastSaved = s
        rulesDoc = core.rulesLoad()
        profiles = core.profilesLoad()
        volumeWarnings = core.volumeWarnings()
        refreshPort()
    }

    /// Re-reads everything from disk (a file was edited or the engine wrote a value back).
    func reload() {
        let s = core.settingsLoad()
        loading = true
        app = s
        loading = false
        lastSaved = s
        rulesDoc = core.rulesLoad()
        profiles = core.profilesLoad()
        volumeWarnings = core.volumeWarnings()
        refreshPort()
        applied?(s)
    }

    /// Saves the whole rule list. Returns false (and sets `problems`) when it does not validate.
    @discardableResult func saveRules(_ doc: RulesDoc? = nil) -> Bool {
        do {
            rulesDoc = try core.rulesSave(doc: doc ?? rulesDoc)
            problems = []
            return true
        } catch let CoreError.Invalid(p) {
            problems = p
        } catch {
            problems = ["\(error)"]
        }
        return false
    }

    @discardableResult func saveProfiles(_ list: [ProfileRecord]? = nil) -> Bool {
        do {
            profiles = try core.profilesSave(profiles: list ?? profiles)
            problems = []
            return true
        } catch let CoreError.Invalid(p) {
            problems = p
        } catch {
            problems = ["\(error)"]
        }
        return false
    }

    func clearProblems() { problems = [] }

    /// The different-disk warnings of a tracker's folders as the editor has them (SPEC 7A.4).
    func trackerVolumeWarnings(_ p: ProfileRecord) -> [VolumeWarning] { core.trackerVolumeWarnings(profile: p) }

    /// Reads the port in use now and whether the manual one could not be bound. A change of the
    /// setting is applied by the engine within a moment (it rebinds live), so this asks again for a
    /// few seconds after a save; it stops as soon as the answer matches the setting.
    func refreshPort() {
        activePort = core.listenPort()
        portFallback = core.portFallback()
        reachability = core.reachability()
    }

    /// While the Network tab is open: the router's answer and the first peer that connects in can
    /// come at any moment, so the status is read again every two seconds.
    func watchReachability() async {
        while !Task.isCancelled {
            reachability = core.reachability()
            try? await Task.sleep(for: .seconds(2))
        }
    }

    /// The setting was saved: watch the engine move to it.
    func watchPortChange() {
        portWatch?.cancel()
        portWatch = Task { [weak self] in
            for _ in 0..<20 {
                try? await Task.sleep(for: .milliseconds(300))
                guard !Task.isCancelled, let self else { return }
                self.refreshPort()
                if self.portSettled { return }
            }
        }
    }

    /// True when the engine runs what the setting asks for: the manual port, or (automatic) any port with no complaint.
    var portSettled: Bool {
        let want = app.network.listenPort
        if want == 0 { return portFallback == nil && activePort != 0 }
        return activePort == want || (portFallback?.wanted == want)
    }

    func reloadActivity() { activity = core.agentActivity(limit: 40) }

    func noteReloaded(warnings w: [String], restart: [String]) {
        warnings = w
        if !restart.isEmpty { restartRequired = restart }
    }

    func noteRejected(_ error: String) { problems = [error] }

    private func scheduleSave(old: AppSettings) {
        saveTask?.cancel()
        saveTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(450))
            guard !Task.isCancelled else { return }
            self?.save()
        }
    }

    /// Writes now (also called when the window closes).
    func save() {
        saveTask?.cancel()
        let s = app
        guard s != lastSaved else { return }
        do {
            try core.settingsSave(settings: s)
            problems = []
            let old = lastSaved
            lastSaved = s
            volumeWarnings = core.volumeWarnings()
            applied?(s)
            if old.network.listenPort != s.network.listenPort { watchPortChange() }
            if old.folders.downloadDir != s.folders.downloadDir {
                finishedLocationChanged?(old.folders.downloadDir, s.folders.downloadDir, nil)
            }
            for (a, b) in zip(old.folders.categories, s.folders.categories) where a.finishedDir != b.finishedDir {
                finishedLocationChanged?(a.finishedDir ?? old.folders.downloadDir, b.finishedDir ?? s.folders.downloadDir, b.category)
            }
        } catch let CoreError.Invalid(problems) {
            self.problems = problems
        } catch {
            problems = ["\(error)"]
        }
    }

    // MARK: Convenience bindings

    func limitMBps(_ bytes: UInt64) -> Double? { bytes == 0 ? nil : Double(bytes) / 1_000_000 }
    func bytes(_ mbps: Double?) -> UInt64 { mbps.map { UInt64(max(0, $0) * 1_000_000) } ?? 0 }
}
