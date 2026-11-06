import AppKit
import FinderSync
import Foundation
import Observation

/// The Settings window's state: typed settings from the core (`config.toml`), rules and tracker
/// profiles, saved straight back. Changes are written a moment after the last edit; the engine
/// picks the file up by itself. External edits of the files arrive through `reload()`.
///
/// What the window shows and what a change does are the core's (`settingsViewModel`, `settingsApply`): this keeps the
/// state it hands back, reads what the model needs from the core and the system, saves, and runs the commands.
@MainActor @Observable final class SettingsModel {
    let core: Core
    /// Every tab's settings; edits are saved (debounced).
    var app: AppSettings {
        didSet { if !loading, app != oldValue { scheduleSave(old: oldValue) } }
    }
    /// Ordered rules and the Default rule; rule edits save through `saveRules()`.
    var rulesDoc: RulesDoc
    var profiles: [ProfileRecord]
    /// The window's own state (selection, open content types, notes), as the core last returned it.
    private(set) var view = settingsViewState()
    private(set) var problems: [String] = []
    private(set) var warnings: [String] = []
    private(set) var restartRequired: [String] = []
    /// What loading the settings files found: a bad value and what is used instead (the core's sentences).
    private(set) var fileNotes: [String] = []
    /// The banner under Settings while there are problems or a restart is due (the core's words).
    var banner: SettingsBanner? { settingsBanner(problems: problems, notes: fileNotes, restartRequired: restartRequired, inSettings: true) }
    private(set) var volumeWarnings: [VolumeWarning] = []
    /// Set when the configured listening port was taken at start and the engine listens elsewhere for now.
    private(set) var portFallback: PortFallbackInfo?
    private(set) var activity: [AgentActivity] = []
    /// Agents' requests for access waiting for the user (Settings › Agents › Requests).
    private(set) var requests: [AgentRequestRow] = []
    /// The port the engine listens on now: the manual one, or the random one picked at this start.
    private(set) var activePort: UInt16 = 0
    /// Whether peers on the Internet can reach the port: a peer that connected in, or the router's answer.
    private(set) var reachability: ReachabilityInfo?
    /// What General, Agents and Folder Sync show of this Mac: the players, the command-line tool and skill, the Finder
    /// extensions (read when their tab opens, and after a change).
    private(set) var playChoices: [PlayWithChoice] = []
    /// The command-line tool and the skill as the core found them, and the version line (`refreshSettingsHostFacts`, off the main thread).
    private(set) var hostFacts: SettingsHostFacts
    private(set) var finderOn = false
    private(set) var shareMenuOn = false
    @ObservationIgnored private var portWatch: Task<Void, Never>?
    /// Called after a save changed the finished location: the old and new value.
    @ObservationIgnored var finishedLocationChanged: ((_ old: String, _ new: String, _ category: Category?) -> Void)?
    /// Set by the app delegate; reacts to settings the shell itself implements (launch at login, ...).
    @ObservationIgnored var applied: ((AppSettings) -> Void)?
    @ObservationIgnored private var loading = false
    @ObservationIgnored private var saveTask: Task<Void, Never>?
    @ObservationIgnored private var lastSaved: AppSettings
    @ObservationIgnored private let clock = MacClock()

    init(core: Core) {
        self.core = core
        let s = core.settingsLoad()
        app = s
        lastSaved = s
        rulesDoc = core.rulesLoad()
        profiles = core.profilesLoad()
        fileNotes = core.settingsProblems()
        hostFacts = core.settingsHostFacts()
        refreshPort()
        refreshVolumeWarnings()
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
        fileNotes = core.settingsProblems()
        refreshPort()
        refreshVolumeWarnings()
        if !ruleCounts.isEmpty { recountRules() }
        applied?(s)
    }

    /// The different-disk warnings of the saved folders: the core looks at each folder's disk (which may be slow or
    /// gone) off the main thread.
    private func refreshVolumeWarnings() {
        let core = self.core
        CoreCalls.run({ await core.volumeWarnings() }) { [weak self] w in
            if self?.volumeWarnings != w { self?.volumeWarnings = w }
        }
    }

    // MARK: The model

    private var state: SettingsState { SettingsState(settings: app, rules: rulesDoc, sync: sync, view: view) }

    /// The window showing `tab`, from the core.
    func page(_ tab: SettingsTab) -> SettingsPageModel {
        let center = UpdateCenter.shared
        let updates: SettingsUpdates = switch center.backend {
        case .github: center.model.map { .core(model: $0) } ?? .off
        case .sparkle: .sparkle(check: center.checkAutomatically, install: center.installAutomatically, canCheck: center.canCheck,
                                version: center.currentVersion)
        }
        let live = SettingsLive(
            problems: problems, notes: fileNotes, restartRequired: restartRequired, volumeWarnings: volumeWarnings,
            activePort: activePort, portFallback: portFallback, reachability: reachability,
            syncReachability: syncReachability, syncPortMovingTo: syncPortMovingTo, rendezvous: syncRendezvous != nil,
            diagnostics: tab == .advanced ? core.diagnostics()
                : DiagnosticsInfo(wakeups: 0, connections: 0, rechecks: 0, hashFailures: 0, peersBanned: 0, promotions: 0, demotions: 0),
            activity: activity, requests: requests, updates: updates,
            playWith: playChoices,
            cli: SettingsCli(home: NSHomeDirectory(), bundled: CommandLineTool.bundledPath()),
            host: hostFacts,
            finderOn: finderOn, shareMenuOn: shareMenuOn, stateDir: core.stateDir())
        return settingsViewModel(input: SettingsViewInput(tab: tab, state: state, live: live), host: hostWindowsAll(),
                                 features: hostFeaturesAll(), clock: clock)
    }

    /// A change from a control: the core's rule, then the saves it names.
    func apply(_ change: SettingsChange) {
        // The Sparkle updater's own switches.
        if case let .set(key, .flag(on)) = change, key == .sparkleCheck || key == .sparkleInstall {
            if key == .sparkleCheck { UpdateCenter.shared.checkAutomatically = on } else { UpdateCenter.shared.installAutomatically = on }
            return
        }
        let r = settingsApply(state: state, change: change)
        view = r.state.view
        if r.state.sync != sync { sync = r.state.sync }
        if r.state.settings != app {
            let player = app.general.playWith
            app = r.state.settings
            if app.general.playWith != player { playChoices = core.playWithChoices(selection: app.general.playWith) }
        }
        if r.saves.contains(.rules) { saveRules(r.state.rules) }
        if r.saveNow { save() }
        if let port = r.syncPort { setSyncPort(port) }
        if let q = r.ask { ask(q) }
    }

    /// A command from a control: a change, or what the system does (pickers, the clipboard, Finder, the updater, the
    /// command-line tool).
    func run(_ command: SettingsCommand) {
        let center = UpdateCenter.shared
        switch command {
        case .apply(let change): apply(change)
        case .rules(let change): rules(change)
        case .showTrackerTorrents: break // the window's navigation opens it (RulesTab)
        case let .chooseFolder(key, start):
            Task { if let p = await FolderPicker.choose(start: start) { apply(.set(key: key, value: .typed(text: p))) } }
        case .chooseWatchFolder:
            Task { if let p = await FolderPicker.choose(start: "~") { apply(.addWatchFolder(path: p)) } }
        case .checkForUpdates: center.checkNow()
        case .installUpdate: center.install()
        case .showUpdateLog: center.revealLog()
        case .installBuild: center.chooseBuild()
        case .stopWatchingBuilds: center.stopWatching()
        case .openUrl(let url): if let u = URL(string: url) { NSWorkspace.shared.open(u) }
        case .copyCliPath(let text):
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(text, forType: .string)
            apply(.copied(on: true))
            Task { [weak self] in
                try? await Task.sleep(for: .milliseconds(1500))
                self?.apply(.copied(on: false))
            }
        case .ask(let question): ask(question)
        case let .linkCli(dir, target):
            do {
                let link = try CommandLineTool.link(into: dir, target: target)
                let pathEnv = ProcessInfo.processInfo.environment["PATH"] ?? ""
                apply(.cliLinked(link: link, dir: dir, onPath: CommandLineTool.isOnPath(dir, pathEnv: pathEnv)))
            } catch CommandLineTool.LinkError.exists(let path) {
                apply(.cliLinkExists(path: path))
            } catch {
                apply(.cliLinkFailed(dir: dir, target: target))
            }
            refreshCli()
        case let .installSkill(dir, force):
            Task { @MainActor in
                let r = await CommandLineTool.run(["skill", "install", "--dir", dir] + (force ? ["--force"] : []))
                apply(r.status == 0 ? .skillInstalled(dir: dir) : .skillFailed(dir: dir, error: r.err))
                refreshCli()
            }
        case .removeSkill(let dir):
            Task { @MainActor in
                let r = await CommandLineTool.run(["skill", "uninstall", "--dir", dir])
                apply(r.status == 0 ? .skillRemoved(dir: dir) : .skillRemoveFailed(dir: dir, error: r.err))
                refreshCli()
            }
        case .showInFileManager(let path): NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
        case .viewTrackerLog: break // the window's navigation opens it (SettingsView)
        case .showTrackerLog: TrackerLogFiles.revealInFinder(core: core)
        case .clearTrackerLog: let core = self.core; CoreCalls.inOrder { await core.trackerLogClear() }
        case .manageFinderExtension: FIFinderSyncController.showExtensionManagementInterface()
        case .manageShareMenu: ShareMenu.openSettings()
        case .showFinderSetup: FinderSetup.show()
        case let .decideAgentRequest(id, approve): AgentRequests.decide(core: core, id: id, approve: approve, notify: false)
        }
    }

    /// Asks the core's question; the choice picked runs its command.
    private func ask(_ q: SettingsQuestion) {
        let a = NSAlert()
        a.messageText = q.title
        a.informativeText = q.text
        for c in q.choices { a.addButton(withTitle: c.title) }
        a.addButton(withTitle: q.cancelTitle)
        let n = a.runModal().rawValue - NSApplication.ModalResponse.alertFirstButtonReturn.rawValue
        if n >= 0, n < q.choices.count { run(q.choices[n].command) }
    }

    // MARK: The rules editor

    /// Settings › Rules' own state, as the core last returned it, and what it reads from the core.
    private(set) var rulesState = rulesEditorState()
    private(set) var rulePreview = RulePreview(affected: 0, changed: 0, changeNote: "")
    private(set) var ruleCounts: [RuleCount] = []

    private var rulesInput: RulesEditorInput {
        RulesEditorInput(state: rulesState, rules: rulesDoc, profiles: profiles, preview: rulePreview, counts: ruleCounts, problems: problems)
    }

    /// Settings › Rules, from the core.
    func rulesPage() -> RulesEditorModel { rulesEditorModel(input: rulesInput) }

    /// A change in the rules editor: the core's rule, then the save it asks for.
    func rules(_ change: RulesChange) {
        let r = rulesEditorApply(input: rulesInput, change: change)
        rulesState = r.state
        if r.clearProblems { clearProblems() }
        switch r.effect {
        case .saveRules(let doc):
            saveRules(doc) { [weak self] in
                guard let self else { return }
                rules(.rulesSaved(rules: rulesDoc))
                recountRules()
            }
        case .saveProfiles(let list):
            saveProfiles(list) { [weak self] in
                guard let self else { return }
                rules(.profilesSaved(profiles: profiles))
            }
        case nil:
            break
        }
        refreshRulePreview()
    }

    /// What saving the rule shown would change (the footer).
    func refreshRulePreview() {
        let q = rulesEditorPreviewQuery(state: rulesState, rules: rulesDoc)
        let p = core.rulePreview(doc: q.rules, ruleId: q.ruleId)
        if p != rulePreview { rulePreview = p }
    }

    /// How many torrents each rule governs (the list).
    func recountRules() {
        ruleCounts = rulesDoc.rules.map { RuleCount(ruleId: $0.id, count: core.rulePreview(doc: rulesDoc, ruleId: $0.id).affected) }
            + [RuleCount(ruleId: "*", count: core.rulePreview(doc: rulesDoc, ruleId: "*").affected)]
        refreshRulePreview()
    }

    // MARK: Reading what the model shows

    /// A tab opened: what it shows of this Mac, read once.
    func opened(_ tab: SettingsTab) {
        switch tab {
        case .general: playChoices = core.playWithChoices(selection: app.general.playWith); hostFacts = core.settingsHostFacts()
        case .network: refreshPort()
        case .folderSync: reloadSync(); refreshFinder()
        case .agents: reloadActivity(); reloadRequests(); refreshCli()
        default: break
        }
    }

    /// While a tab is shown, every `refresh_ms` of its model: what moves on by itself.
    func refreshLive(_ tab: SettingsTab) async {
        switch tab {
        case .network: reachability = core.reachability()
        case .folderSync: await refreshSync()
        case .agents: reloadRequests()
        default: break
        }
    }

    /// The Finder extensions are switched in System Settings: seen when the app comes back.
    func refreshFinder() {
        shareMenuOn = ShareMenu.enabled
        // Asking whether the Finder extension is on can wait on the system for a long time (it never answered in a
        // snapshot run, which hung the whole render on the main thread): ask off the main thread.
        Task.detached(priority: .utility) { [weak self] in
            let on = FIFinderSyncController.isExtensionEnabled
            await MainActor.run { if self?.finderOn != on { self?.finderOn = on } }
        }
    }

    /// What the core finds of the command-line tool and the skill, looked at off the main thread.
    private func refreshCli() {
        let core = self.core
        CoreCalls.run({ await core.refreshSettingsHostFacts() }) { [weak self] f in
            if self?.hostFacts != f { self?.hostFacts = f }
        }
    }

    // MARK: Saving

    /// Saves the whole rule list (the file is written off the main thread); `saved` runs once it is. When it does not
    /// validate, `problems` says why.
    func saveRules(_ doc: RulesDoc? = nil, saved: @escaping @MainActor () -> Void = {}) {
        let (core, doc) = (self.core, doc ?? rulesDoc)
        CoreCalls.inOrder({ await catching { try await core.rulesSave(doc: doc) } }) { [weak self] r in
            guard let self else { return }
            switch r {
            case .success(let d):
                rulesDoc = d
                problems = []
                saved()
            case .failure(let error): noteSaveFailed(error)
            }
        }
    }

    func saveProfiles(_ list: [ProfileRecord]? = nil, saved: @escaping @MainActor () -> Void = {}) {
        let (core, list) = (self.core, list ?? profiles)
        CoreCalls.inOrder({ await catching { try await core.profilesSave(profiles: list) } }) { [weak self] r in
            guard let self else { return }
            switch r {
            case .success(let p):
                profiles = p
                problems = []
                saved()
            case .failure(let error): noteSaveFailed(error)
            }
        }
    }

    private func noteSaveFailed(_ error: Error) {
        if case let CoreError.Invalid(p) = error { problems = p } else { problems = ["\(error)"] }
    }

    func clearProblems() { problems = [] }


    /// Reads the port in use now and whether the manual one could not be bound. A change of the
    /// setting is applied by the engine within a moment (it rebinds live), so this asks again for a
    /// few seconds after a save; it stops as soon as the answer matches the setting.
    func refreshPort() {
        activePort = core.listenPort()
        portFallback = core.portFallback()
        reachability = core.reachability()
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

    func reloadRequests() {
        let rows = core.agentRequests(nowMs: Date().unixMs).rows
        if rows != requests { requests = rows }
    }

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

    /// Writes now (also called when the window closes). The file is written off the main thread, after the commands
    /// given before it.
    func save() {
        saveTask?.cancel()
        let s = app
        guard s != lastSaved, s != saving else { return }
        saving = s
        let core = self.core
        CoreCalls.inOrder({ await catching { try await core.settingsSave(settings: s) } }) { [weak self] r in
            guard let self else { return }
            if saving == s { saving = nil }
            switch r {
            case .failure(let error): noteSaveFailed(error)
            case .success:
                problems = []
                let old = lastSaved
                lastSaved = s
                refreshVolumeWarnings()
                applied?(s)
                if old.network.listenPort != s.network.listenPort { watchPortChange() }
                if old.folders.downloadDir != s.folders.downloadDir {
                    finishedLocationChanged?(old.folders.downloadDir, s.folders.downloadDir, nil)
                }
                for (a, b) in zip(old.folders.categories, s.folders.categories) where a.finishedDir != b.finishedDir {
                    finishedLocationChanged?(a.finishedDir ?? old.folders.downloadDir, b.finishedDir ?? s.folders.downloadDir, b.category)
                }
            }
        }
    }

    /// The settings being written now, if any.
    @ObservationIgnored private var saving: AppSettings?

    /// Quit: the settings not saved yet (`nil`: none), for the shutdown to write before the engine stops. Nothing more is
    /// saved from here.
    func takeUnsaved() -> AppSettings? {
        saveTask?.cancel()
        let s = app
        // A save started from here may not have reached the core yet (the main thread no longer runs once the quit
        // waits): written again, the same file.
        guard s != lastSaved else { return nil }
        lastSaved = s
        return s
    }

    // MARK: Folder sync (its own settings, in sync.toml)

    /// Settings › Folder Sync, as the sync service has them; edits are saved a moment after the last one.
    var sync: SyncSettings? {
        didSet { if !loading, let s = sync, s != oldValue { scheduleSyncSave() } }
    }
    /// Whether the router opened the sync port (nil while sync is not running).
    private(set) var syncReachability: ReachabilityInfo?
    /// A new sync port was set and sync is starting again on it.
    private(set) var syncPortMovingTo: UInt16?
    /// Settings › Folder Sync's rendezvous section, as the core has it (refreshed with the rest while the tab is open).
    private(set) var syncRendezvous: RendezvousView?
    /// Settings › Network's torrent proxy (the app's one `ProxyCenter`, wired by the app delegate; nil: not drawn).
    @ObservationIgnored var proxy: ProxyCenter?
    @ObservationIgnored private var syncSaveTask: Task<Void, Never>?

    func reloadSync() {
        loading = true
        sync = core.syncSettings()
        loading = false
    }

    /// Moves folder sync to `port` now (off the main thread, after the changes given before it). A port another app
    /// has is refused at once (the field says why); otherwise sync starts again on it and `refreshSync` sees it arrive.
    func setSyncPort(_ port: UInt16) {
        let core = self.core
        CoreCalls.inOrder({ await catching { try await core.setSyncPort(port: port) } }) { [weak self] r in
            guard let self else { return }
            switch r {
            case .success: if port != self.sync?.port { self.syncPortMovingTo = port }
            case .failure(let error): self.apply(.syncPortRefused(message: SyncText.message(error)))
            }
        }
    }

    /// While Settings › Folder Sync is open: the port in use (a new one arrives a few seconds after it is set) and
    /// the router's answer for it.
    private func refreshSync() async {
        syncReachability = core.syncReachability()
        syncRendezvous = core.syncRendezvous()
        let port = core.syncSettings().port
        if port != 0, port != sync?.port {
            loading = true
            sync?.port = port
            loading = false
        }
        if let want = syncPortMovingTo, port == want || core.syncSnapshot().problem != nil { syncPortMovingTo = nil }
    }

    /// Starts a rendezvous action (it runs in the background; `syncRendezvous` shows how it goes). Returns why it was
    /// refused at once.
    func rendezvous(_ action: RendezvousAction) async -> String? {
        let core = self.core
        let r = await catching {
            switch action {
            case .setUp(let token): try await core.syncRendezvousSetUp(token: token)
            case .use(let address): try await core.syncRendezvousUse(url: address)
            case .test: try await core.syncRendezvousTest()
            case .turnOff: try await core.syncRendezvousTurnOff()
            }
        }
        switch r {
        case .success:
            syncRendezvous = core.syncRendezvous()
            return nil
        case .failure(let error):
            return SyncText.message(error)
        }
    }

    private func scheduleSyncSave() {
        syncSaveTask?.cancel()
        syncSaveTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(450))
            guard !Task.isCancelled, let self, let s = self.sync else { return }
            let core = self.core
            switch await catching({ try await core.setSyncSettings(settings: s) }) {
            case .success: self.problems = []
            case .failure(let error):
                self.problems = [SyncText.message(error)]
                self.reloadSync()
            }
        }
    }
}
