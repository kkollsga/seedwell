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
    /// First launch only: the port Transmission uses, offered as ours (Settings > Network).
    private(set) var transmissionPortOffer: UInt16?
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
        portFallback = core.portFallback()
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

    /// Reads the remembered offer; it is dropped once the listening port already is that port.
    func refreshTransmissionOffer() {
        let dir = core.stateDir()
        guard let port = TransmissionPort.pending(stateDir: dir) else { transmissionPortOffer = nil; return }
        if app.network.listenPort == port {
            TransmissionPort.clear(stateDir: dir)
            transmissionPortOffer = nil
        } else {
            transmissionPortOffer = port
        }
    }

    /// "Use it": Transmission's port becomes ours (saved with the other settings).
    func useTransmissionPort() {
        guard let port = transmissionPortOffer else { return }
        app.network.listenPort = port
        TransmissionPort.clear(stateDir: core.stateDir())
        transmissionPortOffer = nil
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
