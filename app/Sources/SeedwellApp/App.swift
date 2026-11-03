import AppKit
import IOKit.pwr_mgt
import ServiceManagement
import UserNotifications
import os

@MainActor final class AppDelegate: NSObject, NSApplicationDelegate, UNUserNotificationCenterDelegate {
    let options: LaunchOptions
    private(set) var core: Core!
    private(set) var commands: Commands!
    private(set) var settings: SettingsModel!
    private var statusMenu: StatusMenuController?
    private(set) var startingNotifier: StartingNotifier?
    /// Which core events and chat messages become notifications, and their words (the core's).
    private(set) var notices: Notices!
    private var mainWindow: MainWindowController?
    var mainWindowController: MainWindowController? { mainWindow }
    var statusMenuController: StatusMenuController? { statusMenu }
    func closeChoosers() { chooserWindows.closeAll() }
    func qaChooserChange(to c: Category) { chooserWindows.qaChange(to: c) }
    private var chooserWindows = ChooserPresenter()
    /// Folder sync's add and edit modals.
    private(set) var syncWindows = SyncWindows()
    /// The Finder extension's status file.
    private var finderStatus: FinderStatusWriter?
    /// On-demand folders' File Provider domains and the extension's server (the user's own app only).
    private var fileProvider: FileProviderHost?
    /// Files and links the system asked to open before the core had started (it starts off the main thread).
    private var openBeforeStart: [URL] = []
    var qaChooserWindow: NSWindow? { chooserWindows.qaWindow }
    private var qa: QAControl?
    private let realHome: String
    private let log = Logger(subsystem: "io.github.kkollsga.Seedwell", category: "app")
    private var quitting = false
    /// Set by the paths that really end the process: the menu bar menu's Quit row, the QA `quit`,
    /// an update about to relaunch the app (Sparkle; the curl installer sends SIGTERM, which is `quitForReal`). Every other quit request only closes the windows.
    private var terminationAllowed = false
    /// SIGTERM (installers, `kill`, launchd) quits properly: the engine shuts down and trackers get
    /// their `stopped`, instead of the default immediate exit.
    private var termSource: DispatchSourceSignal?
    /// Opened by the user (Dock, Finder, Spotlight), not at login or to open a file: show the window.
    private var userLaunch = false
    /// The Dock icon was clicked while the previous run was still quitting: start again once it is gone.
    private var reopenAfterQuit = false
    /// QA (`config`): how often the engine has read its settings files again and applied them.
    private(set) var qaConfigReloads = 0
    private var askingToMove = false
    private var powerOff = false
    private var sleepAssertion = SleepAssertion()
    private var agentsAssertion = SleepAssertion()
    private var napGuard = NapGuard()
    /// The power source, told to the core ("On battery", SPEC §8.4).
    private(set) var power: PowerWatcher?
    /// The OS network path and sleep/wake, told to the engine (SPEC 7C.4c).
    private(set) var networkWatcher: NetworkWatcher?
    /// Notices a main thread that stops answering (the app never hangs).
    private var watchdog: HangWatchdog?

    init(options: LaunchOptions) {
        self.options = options
        // The real home folder as the system sees it, whatever `--home-root` did to `$HOME`.
        realHome = getpwuid(getuid()).flatMap { String(cString: $0.pointee.pw_dir) } ?? NSHomeDirectory()
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        // A write to a connection or pipe the other end has closed must fail with EPIPE, not end the app: the core's
        // sockets and the folder sync wake-up commands write to peers that may go away at any time.
        signal(SIGPIPE, SIG_IGN)
        signal(SIGTERM, SIG_IGN)
        let term = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
        term.setEventHandler { [weak self] in MainActor.assumeIsolated { self?.quitForReal() } }
        term.resume()
        termSource = term
        if let problem = options.configurationError { return fatal(startFailureTexts().refusedTitle, problem) }
        startWatchdog()
        let launchedByUser = Self.isUserLaunch(notification)
        log.notice("launch: by user \(launchedByUser), background \(self.options.background)")
        userLaunch = launchedByUser && !options.background && options.stateDir == nil && options.qaControl == nil
        if goBackAfterFailedLaunches() { return }
        startCore(waitedSecs: 0)
    }

    /// A development build launched three times in a row without starting fine goes back to the app kept before it
    /// (SPEC 12A, Recovery): the core says so before it starts, since it may be what fails.
    private func goBackAfterFailedLaunches() -> Bool {
        guard !options.mock, options.renderSnapshots == nil,
              let c = launchGuard(stateDir: options.resolvedStateDir(), version: UpdateCenter.runningVersion,
                                  installDir: UpdateCenter.installDir, relaunch: options.relaunchArgs) else { return false }
        log.error("launch: three launches without starting fine; going back to the kept app")
        let logURL = URL(fileURLWithPath: c.log)
        try? FileManager.default.createDirectory(at: logURL.deletingLastPathComponent(), withIntermediateDirectories: true)
        if (try? Spawn.detached(argv: c.argv, env: c.env.map { ($0.name, $0.value) }, log: logURL)) != nil { exit(0) }
        return false
    }

    /// Launched like an app by the user, rather than at login (`keyAELaunchedAsLogInItem`) or to open
    /// a .torrent file or magnet link (not a default launch; those show their own window).
    private static func isUserLaunch(_ n: Notification) -> Bool {
        let isDefault = (n.userInfo?[NSApplication.launchIsDefaultUserInfoKey] as? Bool) ?? false
        let event = NSAppleEventManager.shared().currentAppleEvent
        let atLogin = event?.eventID == fourCC("oapp") && event?.paramDescriptor(forKeyword: fourCC("prdt"))?.enumCodeValue == fourCC("lgit")
        return isDefault && !atLogin
    }

    /// The previous run may still be quitting (it lingers until trackers have heard `stopped`, up to
    /// 120 s) and holds the state until then: wait for it quietly instead of failing.
    ///
    /// The core starts off the main thread (it opens the state, loads every torrent and reads the library once, which
    /// takes a while for a big library): the main thread never waits for it.
    private func startCore(waitedSecs: Int) {
        let dir = options.resolvedStateDir()
        let coreOptions = CoreOptions(
            configDir: dir,
            mock: options.mock,
            allowLoopbackTrackers: options.allowLoopbackTrackers,
            forbiddenRoot: options.isolated ? realHome : nil)
        // Folder sync's keys go in the Keychain for the app's own state, when this build may use it (a keychain
        // entitlement, probed once; see KeychainVault). An ad-hoc build, a mock, QA or other isolated run keeps them in
        // a 0600 file in its state folder, and the latter never touch the user's Keychain.
        let mayUseKeychain = !options.mock && options.stateDir == nil
        Task.detached(priority: .userInitiated) { [weak self] in
            let started = Result<Core, Error> {
                try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
                let core = mayUseKeychain && KeychainVault.available()
                    ? try Core.withSecretVault(options: coreOptions, vault: KeychainVault())
                    : try Core(options: coreOptions)
                // The first library read, which may wait for the engine: done here, so the main thread's reads show it.
                _ = core.snapshot()
                return core
            }
            await MainActor.run { self?.coreStarted(started, waitedSecs: waitedSecs) }
        }
    }

    private func coreStarted(_ started: Result<Core, Error>, waitedSecs: Int) {
        switch started {
        case .success(let c):
            core = c
        case .failure(let error):
            // The control socket says "another instance"; the state database, opened first, says it is locked.
            let busy = ["another instance", "Database already open", "Cannot acquire lock"].contains { "\(error)".contains($0) }
            if busy, waitedSecs < 150 {
                log.info("previous run still quitting; waiting (\(waitedSecs) s so far)")
                DispatchQueue.main.asyncAfter(deadline: .now() + 1) { [weak self] in
                    MainActor.assumeIsolated { self?.startCore(waitedSecs: waitedSecs + 1) }
                }
                return
            }
            return fatal(startFailureTexts().title, "\(error)", recovery: true)
        }
        commands = Commands(core: core)
        settings = SettingsModel(core: core)
        if let snapshotDir = options.renderSnapshots {
            Task { @MainActor in
                await SnapshotRenderer(core: core, commands: commands, settings: settings, dir: snapshotDir).renderAll()
                self.quitForReal()
            }
            return
        }
        start()
    }

    /// Says why the app cannot start, and ends. `recovery`: the core could not start; the window also offers going
    /// back to the app kept before this one and installing the release (SPEC 12A), neither of which touches data.
    private func fatal(_ title: String, _ text: String, recovery: Bool = false) {
        // A QA-control run shows no window the user could answer: it says why on stderr and ends.
        if QuietRun.on {
            FileHandle.standardError.write(Data("\(title): \(text)\n".utf8))
            exit(1)
        }
        NSApp.setActivationPolicy(.regular)
        let a = NSAlert()
        a.messageText = title
        a.informativeText = text
        let texts = startFailureTexts()
        a.addButton(withTitle: texts.quitTitle).toolTip = texts.quitHelp
        let actions = recovery
            ? recoveryActions(stateDir: options.resolvedStateDir(), installDir: UpdateCenter.installDir,
                              repoUrl: Bundle.main.object(forInfoDictionaryKey: "SeedwellRepoURL") as? String,
                              relaunch: options.relaunchArgs)
            : []
        for action in actions { a.addButton(withTitle: action.title).toolTip = action.help }
        NSApp.activate()
        let chosen = a.runModal().rawValue - NSApplication.ModalResponse.alertFirstButtonReturn.rawValue - 1
        if actions.indices.contains(chosen) {
            let c = actions[chosen].command
            let logURL = URL(fileURLWithPath: c.log)
            try? FileManager.default.createDirectory(at: logURL.deletingLastPathComponent(), withIntermediateDirectories: true)
            try? Spawn.detached(argv: c.argv, env: c.env.map { ($0.name, $0.value) }, log: logURL)
        }
        exit(1)
    }

    private func start() {
        UpdateCenter.shared.start(core: core, relaunchArgs: options.relaunchArgs)
        let main = MainWindowController(commands: commands, settings: settings, defaultSize: options.showWindowSize)
        mainWindow = main
        main.onOpenChange = { [weak self] in self?.refreshActivationPolicy() }
        commands.showMain = { section, info in main.show(section: section, info: info) }
        commands.showTrackerInfo = { id in main.show(section: .tracker(trackerId: id)); main.libraryModel?.trackerInfo = true }
        commands.showTracker = { id in main.show(section: .tracker(trackerId: id)) }
        commands.showAddLink = { main.show(); main.libraryModel?.showAddSheet() }
        commands.askRemoval = { rows, deleteFiles in main.libraryModel?.askRemoval(rows, deleteFiles: deleteFiles) }
        // Settings is a section of the main window: every way in opens it on the right tab.
        commands.showSettings = { tab in main.show(settings: tab, openSettings: true) }
        commands.pairDevice = { link in main.show(settings: .devices, pairLink: link) }
        commands.editTracker = { id in main.show(settings: .rules, editTracker: id) }
        commands.createTracker = { host in main.show(settings: .rules, newTrackerHost: host) }
        commands.showTorrent = { id in main.showTorrent(id) }
        commands.showChooser = { [weak self] id in self?.chooserWindows.show(id: id, commands: self?.commands) }
        let sync = commands.sync
        sync.showAdd = { [weak self, sync] prefill in self?.syncWindows.showAdd(prefill: prefill, actions: sync) }
        sync.showEdit = { [weak self, sync] id in self?.syncWindows.showEdit(id: id, actions: sync) }
        sync.showShare = { [weak self, sync] id in self?.syncWindows.showShare(id: id, actions: sync) }
        sync.showShareNew = { [weak self, sync] path in self?.syncWindows.showShare(newFolder: path, actions: sync) }
        let chat = commands.chat
        chat.changed = { main.libraryModel?.refresh() }
        chat.show = { place in main.show(section: place) }
        // A quiet QA run is never the active app; its window counts as seen.
        chat.noticeView = { NoticeView(inFront: NSApp.isActive || QuietRun.on, windowPlace: main.isOpen ? main.libraryModel?.section : nil) }
        chat.noteDirectName = { device, name in main.libraryModel?.noteDirectName(device, name) }
        notices = Notices(core: core)
        chat.start(notices)
        Notifier.withdrawStale(notices)  // what an earlier run left of requests answered or expired since
        sync.showChat = { [chat] id in chat.showChannel(id) }
        sync.showFolder = { id, ask in main.show(section: .folderSync); main.libraryModel?.showSyncFolder(id, askRemove: ask) }
        FinderSetup.show = { [weak self] in self?.syncWindows.showFinderSetup() }
        let finder = FinderStatusWriter(core: core)
        finder.onFirstFolder = { [weak self] in self?.offerFinderBadges() }
        finder.start()
        finderStatus = finder
        // On-demand folders in the Finder (SPEC 7G.11): only the user's own Seedwell serves the File Provider extension
        // and keeps its domains; a mock, QA or other isolated run never registers a domain nor takes the socket.
        if !options.mock, options.stateDir == nil, options.qaControl == nil, !QuietRun.on {
            let fp = FileProviderHost(core: core)
            fp.showSetup = { [weak self, weak fp] in if let fp { self?.syncWindows.showFileProviderSetup(host: fp) } }
            fp.onNotice = { [sync] title, text in sync.alert(title, text) }
            fp.start()
            fileProvider = fp
            sync.fileProvider = fp
        }
        let choices = Choices(core: core)
        ChoiceStore.shared.choices = choices
        ChoiceStore.shared.didResolve = { [weak self] id in
            self?.commands.arrivals.userReselected(id)  // a running torrent's files chosen again
            self?.chooserWindows.close(id)
        }
        FilesStore.shared.onTicked = { [weak self] id in self?.commands.arrivals.userReselected(id) }
        statusMenu = StatusMenuController(core: core, commands: commands, settings: settings)
        let notifier = StartingNotifier(commands: commands, choices: choices) { [weak self] in self?.statusMenu?.statusButton }
        startingNotifier = notifier
        statusMenu?.onWillOpen = { [weak notifier] in notifier?.stepAside() }
        commands.arrivals.onArrival = { [weak main] id in main?.newDownload(id) }
        commands.arrivals.note(core.snapshot())  // the baseline: what is downloading now is not new
        statusMenu?.onRefresh = { [weak self] s in
            self?.commands.arrivals.note(s)
            UpdateCenter.shared.noteActivity()
            let needs = powerNeeds(s: s)
            self?.sleepAssertion.update(downloading: needs.downloading)
            self?.napGuard.update(serving: needs.serving)
            self?.agentsAssertion.update(agents: self?.core.agentsAwake())
        }
        NSApp.mainMenu = MainMenu.make(commands: commands)
        settings.applied = { [weak self] s in self?.apply(s) }
        AgentRequests.answered = { [weak self] in
            self?.settings.reloadRequests()
            self?.statusMenu?.refreshIcon()
            if let n = self?.notices { Notifier.withdrawStale(n) }
        }
        apply(settings.app)
        main.onClose = { [weak self] in self?.settings.save() }

        if Notifier.available {
            UNUserNotificationCenter.current().delegate = self
            Notifier.requestAuthorization()
        }
        core.setListener(listener: CoreEvents { [weak self] event in
            Task { @MainActor in self?.handle(event) }
        })
        networkWatcher = NetworkWatcher(core: core)
        if !options.mock { power = PowerWatcher(core: core) }
        let ws = NSWorkspace.shared.notificationCenter
        ws.addObserver(forName: NSWorkspace.willPowerOffNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.powerOff = true; self?.core.shortenShutdown(secs: 3) }
        }
        // A drive came or went: the settings may point at it, and the engine marks the torrents on it
        // "Drive not connected" or brings them back (it looks at each save folder once, off the main thread).
        ws.addObserver(forName: NSWorkspace.didMountNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.settings.reload(); self?.core.volumesChanged() }
        }
        ws.addObserver(forName: NSWorkspace.didUnmountNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.settings.reload(); self?.core.volumesChanged() }
        }

        UpdateCenter.shared.willRelaunch = { [weak self] in self?.terminationAllowed = true }
        if let path = options.qaControl {
            PlayerLauncher.blocked = true  // QA never starts the user's players on its own
            qa = QAControl(path: path, app: self)
        }
        offerBacklog()
        // Torrents still waiting when the app last quit are held again with a fresh countdown: show them.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { [weak self] in MainActor.assumeIsolated { self?.startingNotifier?.refresh(launch: true) } }
        if options.showWindow || userLaunch { main.show() }
        if let tab = options.showSettings { main.show(settings: SettingsTab(rawValue: tab), openSettings: true) }
        let early = openBeforeStart
        openBeforeStart = []
        if !early.isEmpty { application(NSApp, open: early) }
        // Opened from its disk image or Downloads: offer the move to Applications. Only in the user's own runs, never
        // in tests, QA or a --state-dir run.
        if options.stateDir == nil, !options.mock, options.qaControl == nil, !QuietRun.on {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) { [weak self] in
                MainActor.assumeIsolated { MoveToApplications.offer { self?.quitForReal() } }
            }
        }
    }

    /// A stall of the main thread over a second is noted (the developer log's `ui-stall`, with its length and the last UI
    /// action; stderr in debug builds and test runs). A snapshot render or QA run that hangs for two seconds fails.
    private func startWatchdog() {
        let testRun = options.renderSnapshots != nil || options.qaControl != nil
        let log = self.log
        let wd = HangWatchdog(failing: testRun) { [weak self] ms, action in
            // On the main thread (the late beat itself).
            MainActor.assumeIsolated {
                let core: Core? = self?.core
                let line = core?.uiStall(stalledMs: ms, lastAction: action) ?? uiStallLine(stalledMs: ms, lastAction: action, failing: false)
                log.error("hang: \(line, privacy: .public)")
                #if DEBUG
                let toStderr = true
                #else
                let toStderr = testRun
                #endif
                if toStderr { FileHandle.standardError.write(Data("seedwell: \(line)\n".utf8)) }
            }
        }
        wd.start()
        watchdog = wd
    }

    /// Seedwell in Finder (badges and the Share menu) is offered once ever, when there is a synced folder and either
    /// extension is off (macOS lets only the user switch them on). Later, Settings › Folder Sync has the way in. Not
    /// for mock, QA or other isolated runs.
    private func offerFinderBadges() {
        let marker = URL(fileURLWithPath: core.stateDir()).appendingPathComponent("finder-badges-offered")
        guard !options.mock, options.stateDir == nil, !FileManager.default.fileExists(atPath: marker.path) else { return }
        // Whether the Finder extension is on is asked off the main thread: the system can take long to answer.
        Task.detached(priority: .utility) { [weak self] in
            let on = FinderStatusWriter.extensionEnabled
            await MainActor.run {
                guard let self, !(on && ShareMenu.enabled) else { return }
                try? "offered".write(to: marker, atomically: true, encoding: .utf8)
                self.syncWindows.showFinderSetup()
            }
        }
    }

    /// The `.torrent` files that were already in a watch folder when it joined the list, offered once ever (the core
    /// decides when, and words it).
    private func offerBacklog() {
        let core = self.core!
        // The core reads the watch folders off the main thread.
        CoreCalls.run({ await core.backlogPrompt() }) { [weak self] p in
            guard let self, let p else { return }
            let a = Self.backlogAlert(p.offer)
            NSApp.activate()
            if a.runModal() == .alertFirstButtonReturn { for f in p.files { commands.add(f) } }
        }
    }

    static func backlogAlert(_ o: BacklogOffer) -> NSAlert {
        let a = NSAlert()
        a.messageText = o.title
        a.informativeText = o.text
        a.addButton(withTitle: o.addTitle)
        a.addButton(withTitle: o.laterTitle)
        return a
    }

    /// Settings the shell implements itself.
    private func apply(_ s: AppSettings) {
        if Bundle.main.bundleIdentifier != nil, Bundle.main.bundlePath.hasSuffix(".app"), options.stateDir == nil {
            let want = s.general.launchAtLogin
            // The login item's status and its registration ask a system service, which can take long: off the main
            // thread.
            Task.detached(priority: .utility) {
                let service = SMAppService.mainApp
                if want, service.status != .enabled { try? service.register() }
                if !want, service.status == .enabled { try? service.unregister() }
            }
        }
        sleepAssertion.enabled = s.general.preventSleepWhileDownloading
        agentsAssertion.enabled = s.general.keepAwakeForAgents
        UpdateCenter.shared.settingsChanged()
        statusMenu?.refreshIcon()
    }

    private func handle(_ event: CoreEvent) {
        switch event {
        case .finished(let row):
            notice(.finished(row: row))
            statusMenu?.refreshIcon()
        case .error(let id, let message):
            log.error("core error \(id ?? "-", privacy: .public): \(message, privacy: .public)")
            notice(.error(id: id, message: message))
            statusMenu?.refreshIcon()
        case .goalReached(let row):
            notice(.goalReached(row: row))
        case .needsChoice(let row):
            // The starting notifier is refreshed first: when it shows, it is the notice and holds the checklist (SPEC 8.6).
            let requested = startingNotifier?.consumeChooserRequest(row.id) == true
            startingNotifier?.refresh()
            notice(.needsChoice(row: row, chooserRequested: requested, startingShown: startingNotifier?.isShown == true))
            statusMenu?.refreshIcon()
        case .autoAdded(let a):
            notice(.autoAdded(added: a))
            statusMenu?.refreshIcon()
        case .pending:
            startingNotifier?.refresh()
            statusMenu?.refreshIcon()
        case .duplicate(let id, let name, let added):
            notice(.duplicate(id: id, name: name, trackersAdded: UInt32(clamping: added)))
        case .addRequest(let r):
            chooserWindows.showAddRequest(r, commands: commands)
        case .agent(let a):
            settings.reloadActivity()
            settings.reloadRequests()
            notice(.agent(activity: a))
            // A request came, was answered (here or by a chat reply) or expired: the icon's dot and the notices follow.
            statusMenu?.refreshIcon()
            Notifier.withdrawStale(notices)
        case .settingsChanged(_, let warnings, let restart):
            qaConfigReloads += 1
            settings.noteReloaded(warnings: warnings, restart: restart)
            settings.reload()
        case .settingsRejected(_, let error):
            settings.noteRejected(error)
        case .locationsChanged(let n, let bytes):
            askToMove(count: n, bytes: bytes)
        case .attention:
            statusMenu?.refreshIcon()
        }
    }

    /// What the core says about an event: post a notification, or open something.
    private func notice(_ event: NoticeEvent) {
        switch notices.event(event: event) {
        case .nothing: break
        case .post(let n): Notifier.post(n)
        case .open(let route): open(route)
        }
    }

    /// Opens what a notification's route names.
    func open(_ route: NoticeRoute) {
        switch route {
        case .window: commands.showMain(nil, nil)
        case .transfer(let id): commands.showMain(nil, id)
        case .chooser(let id): commands.showChooser(id)
        case .starting(let id): startingNotifier?.reveal(id)
        case .place(let place): commands.chat.show(place)
        case .agentRequests: commands.showSettings(.agents)
        }
    }

    /// SPEC §7A.5: after a finished location changed, "Move the N existing torrents (X GB) to the new location?" One
    /// question at a time.
    private func askToMove(count: UInt32, bytes: UInt64) {
        guard count > 0, !askingToMove else { return }
        askingToMove = true
        let core = self.core!
        // What would move is worked out by the engine, off the main thread; so is the move.
        CoreCalls.run({ await core.locationMoves() }) { [weak self] proposal in
            guard let self else { return }
            guard let p = locationMovePrompt(proposal: proposal) else { askingToMove = false; return }
            let a = Self.locationMoveAlert(p)
            NSApp.activate()
            guard a.runModal() == .alertFirstButtonReturn else { askingToMove = false; return }
            CoreCalls.inOrder({ await core.moveTorrents(ids: p.ids) }) { [weak self] r in
                self?.askingToMove = false
                if let problem = locationMoveResult(result: r) { self?.commands.alert(problem.title, problem.text) }
            }
        }
    }

    static func locationMoveAlert(_ p: LocationMovePrompt) -> NSAlert {
        let a = NSAlert()
        a.messageText = p.title
        a.informativeText = p.text
        a.addButton(withTitle: p.moveTitle)
        a.addButton(withTitle: p.dontMoveTitle)
        return a
    }

    // MARK: URLs, notification clicks

    /// Double-clicked .torrent files, clicked magnet: links and `seedwell://` links (invitations, the Finder and Share
    /// extensions' links) all arrive here. A file is added; a link goes where the core routes it (SPEC 7G.5).
    func application(_ application: NSApplication, open urls: [URL]) {
        guard commands != nil else { openBeforeStart += urls; return }
        for url in urls {
            if url.isFileURL { commands.add(url.path) } else { commands.open(core.routeLink(url: url.absoluteString)) }
        }
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        let click = response.notification.request.content.userInfo[Notifier.clickKey] as? String ?? ""
        let action = response.actionIdentifier
        await MainActor.run { Notifier.respond(click: click, action: action, core: core) { [weak self] in self?.open($0) } }
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        // One that waits for an answer (an agent's request) stays in Notification Center too, until it is answered.
        notification.request.content.interruptionLevel == .timeSensitive ? [.banner, .list] : [.banner]
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool { false }

    // MARK: Dock icon

    /// Seedwell is a menu bar app: it has a Dock icon and a place in the app switcher only while one of
    /// its windows (the library, with Settings inside it) is open.
    func refreshActivationPolicy() {
        guard !quitting else { return }
        let open = mainWindow?.isOpen ?? false
        let want: NSApplication.ActivationPolicy = open ? .regular : .accessory
        // A quiet QA run never has a Dock icon or a place in the app switcher: `policy` reports the one it would have.
        if QuietRun.on { QuietRun.policy = want; return }
        guard NSApp.activationPolicy() != want else { return }
        NSApp.setActivationPolicy(want)
        if want == .regular { NSApp.activate() }
    }

    /// A click on the Dock icon with every window gone (it can only happen right after the last one
    /// closed): open the library.
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        // Quitting already (the menu bar's Quit lingers for the trackers): come back once it is done.
        if quitting { reopenAfterQuit = true; return false }
        if !flag, commands != nil { commands.showMain(nil, nil) }
        return true
    }

    /// QA: policy and windows, as one line for `<file>.bench`.
    func qaPolicyLine() -> String {
        let p = QuietRun.on ? QuietRun.policy : NSApp.activationPolicy()
        let name = p == .regular ? "regular" : p == .accessory ? "accessory" : "prohibited"
        let visible = NSApp.windows.filter { $0.isVisible && $0.styleMask.contains(.titled) }.count
        return "policy: \(name), main \(mainWindow?.isOpen == true ? "open" : "closed"), settings \(mainWindow?.libraryModel?.section == .settings ? "open" : "closed"), titled windows \(visible), pid \(getpid())"
    }

    /// QA: the quit Apple event, sent to this process the way the Dock's Quit menu item does (no reason),
    /// or with a system reason (`logout`, `restart`, `shutdown`).
    func qaSendQuitEvent(reason: String?) {
        let target = NSAppleEventDescriptor(processIdentifier: getpid())
        let event = NSAppleEventDescriptor(eventClass: AEEventClass(Self.fourCC("aevt")), eventID: AEEventID(Self.fourCC("quit")),
                                           targetDescriptor: target, returnID: AEReturnID(kAutoGenerateReturnID), transactionID: AETransactionID(kAnyTransactionID))
        if let reason {
            let code = ["logout": "logo", "restart": "rest", "shutdown": "shut"][reason] ?? "logo"
            event.setAttribute(NSAppleEventDescriptor(typeCode: OSType(Self.fourCC(code))), forKeyword: Self.fourCC("why?"))
        }
        // No reply wanted: the process must be able to answer from the same run loop.
        _ = try? event.sendEvent(options: [.noReply], timeout: 5)
    }

    /// The menu bar menu's own Quit row (and its ⌘Q): the one user action that ends the process.
    func quitForReal() {
        terminationAllowed = true
        NSApp.terminate(nil)
    }

    /// Quit from the Dock menu or ⌘Q in a window only closes the windows; the app stays in the menu bar.
    private func closeWindowsKeepRunning() {
        mainWindow?.closeForQuit()
        chooserWindows.closeAll()
        syncWindows.closeAll()
        refreshActivationPolicy()
    }

    /// The reason the system attached to a quit Apple event (`why?`): log out, restart or shut down.
    /// A quit from the Dock menu or from the app itself carries none.
    nonisolated static func isSystemQuit(reason: AEKeyword?) -> Bool {
        guard let reason else { return false }
        let systemReasons: [AEKeyword] = [
            fourCC("logo"),  // kAELogOut
            fourCC("rlgo"),  // kAEReallyLogOut
            fourCC("rrst"),  // kAEShowRestartDialog
            fourCC("rsdn"),  // kAEShowShutdownDialog
            fourCC("rest"),  // kAERestart
            fourCC("shut"),  // kAEShutDown
        ]
        return systemReasons.contains(reason)
    }

    nonisolated static func fourCC(_ s: String) -> AEKeyword { s.utf8.reduce(0) { $0 << 8 | AEKeyword($1) } }

    /// `kAEQuitReason` of the Apple event being handled right now, if there is one.
    private func currentQuitReason() -> AEKeyword? {
        NSAppleEventManager.shared().currentAppleEvent?.attributeDescriptor(forKeyword: Self.fourCC("why?"))?.typeCodeValue
    }

    // MARK: Quit (SPEC §12 #16)

    /// Open the app again as soon as this process has exited (a detached shell waits for the pid).
    private static func relaunchWhenGone() {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/sh")
        p.arguments = ["-c", "while kill -0 \(getpid()) 2>/dev/null; do sleep 0.2; done; /usr/bin/open \"$0\"", Bundle.main.bundlePath]
        try? p.run()
    }

    /// The icon and the windows go at once; the process lingers until every `stopped` announce is
    /// answered or given up, at most 120 s (3 s at logout or shutdown).
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard core != nil, !quitting else { return .terminateNow }
        // Only the menu bar menu's Quit (and an update, logout, restart or shutdown) ends the process.
        if !terminationAllowed, !powerOff, !Self.isSystemQuit(reason: currentQuitReason()) {
            closeWindowsKeepRunning()
            return .terminateCancel
        }
        quitting = true
        // From here the main thread waits for the engine's shutdown on purpose (a modal wait of AppKit's).
        watchdog?.stop()
        // A clean quit: this launch does not count toward a crash loop (SPEC 12A).
        UpdateCenter.shared.quitting()
        finderStatus?.stop()
        fileProvider?.stop()
        // Saved by the shutdown thread, before the engine stops: the main thread does not wait for the file.
        let unsaved = settings.takeUnsaved()
        startingNotifier?.close()
        statusMenu?.hide()
        mainWindow?.closeForQuit()
        chooserWindows.closeAll()
        syncWindows.closeAll()
        NSApp.setActivationPolicy(.prohibited)
        let secs: UInt32 = powerOff ? 3 : (options.mock ? 1 : 120)
        let core = self.core!
        // The run loop waits for this reply in a modal mode, where plain GCD blocks do not run. Whichever comes first
        // replies: the shutdown, or the deadline (the engine's own deadline and a margin) if the shutdown hangs.
        let reply: @Sendable () -> Void = {
            RunLoop.main.perform(inModes: [.common, .default, .modalPanel, .eventTracking]) {
                MainActor.assumeIsolated {
                    guard !self.replied else { return }
                    self.replied = true
                    // Never for an isolated (QA/dev) run: a plain relaunch would open the real state.
                    if self.reopenAfterQuit, self.options.stateDir == nil { Self.relaunchWhenGone() }
                    NSApp.reply(toApplicationShouldTerminate: true)
                }
            }
        }
        // Threads of their own, not the system's shared worker threads: those can all be busy (a render's animations
        // once took every one of them, and a shutdown waiting for one never ran).
        Self.thread("seedwell-shutdown") {
            if let unsaved { Self.waitFor(seconds: 3) { _ = await catching { try await core.settingsSave(settings: unsaved) } } }
            let r = core.shutdown(deadlineSecs: secs)
            os_log("shutdown: confirmed %d, abandoned %d, finished %d", r.confirmed, r.abandoned, r.finished ? 1 : 0)
            reply()
        }
        Self.thread("seedwell-shutdown-deadline") {
            Thread.sleep(forTimeInterval: Double(secs) + 10)
            os_log("shutdown: no answer within its deadline; quitting anyway")
            reply()
        }
        return .terminateLater
    }

    /// The quit has been answered (by the shutdown or its deadline).
    private var replied = false

    /// Starts `body` on a thread of its own.
    nonisolated static func thread(_ name: String, _ body: @escaping @Sendable () -> Void) {
        let t = Thread(block: body)
        t.name = name
        t.qualityOfService = .userInitiated
        t.start()
    }

    /// Waits on this (background) thread, at most `seconds`, for `work` to finish.
    nonisolated static func waitFor(seconds: Double, _ work: @escaping @Sendable () async -> Void) {
        let done = DispatchSemaphore(value: 0)
        Task.detached(priority: .userInitiated) {
            await work()
            done.signal()
        }
        _ = done.wait(timeout: .now() + seconds)
    }
}

/// An app command of the core's model (`Core.appCommands`): File's adding and Transfers' Pause All, Resume All and
/// Limit Speed. Each time its menu opens or its key is pressed it asks the core again whether it is enabled and
/// checked, and runs the command the core gives then.
@MainActor final class AppCommandItem: NSMenuItem, NSMenuItemValidation {
    private let commands: Commands
    private let menuTitle: String
    private let index: Int

    init(title: String, shortcut: KeyShortcut?, menu: String, index: Int, commands: Commands) {
        self.commands = commands
        menuTitle = menu
        self.index = index
        super.init(title: title, action: #selector(fire), keyEquivalent: shortcut?.key ?? "")
        target = self
        if let k = shortcut {
            var mods: NSEvent.ModifierFlags = .command
            if k.shift { mods.insert(.shift) }
            if k.alt { mods.insert(.option) }
            keyEquivalentModifierMask = mods
        }
    }

    required init(coder: NSCoder) { fatalError("not used") }

    /// The entry as the core has it now.
    private var current: (command: AppCommand, enabled: Bool, checked: Bool)? {
        let entries = commands.core.appCommands().first { $0.title == menuTitle }?.entries ?? []
        guard entries.indices.contains(index), case let .item(command, _, _, enabled, checked) = entries[index] else { return nil }
        return (command, enabled, checked)
    }

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        guard let c = current else { return false }
        state = c.checked ? .on : .off
        return c.enabled
    }

    @objc private func fire() {
        guard let c = current, c.enabled else { return }
        switch c.command {
        case .addTorrentFile: commands.chooseTorrentFile()
        case .addLink: commands.showAddLink()
        case .pauseAll: let core = commands.core; CoreCalls.inOrder { await core.pauseAll() }
        case .resumeAll: let core = commands.core; CoreCalls.inOrder { await core.resumeAll() }
        case .setLimitSpeed(let on): let core = commands.core; CoreCalls.inOrder { await core.setAltSpeed(on: on) }
        }
    }
}

@MainActor enum MainMenu {
    /// The menu bar is hidden for an LSUIElement app, but its key equivalents still work
    /// while one of our windows is key.
    static func make(commands: Commands) -> NSMenu {
        let bar = NSMenu()
        func add(_ title: String, _ items: [NSMenuItem]) {
            let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
            let menu = NSMenu(title: title)
            items.forEach(menu.addItem)
            item.submenu = menu
            bar.addItem(item)
        }
        func act(_ title: String, _ key: String = "", _ mods: NSEvent.ModifierFlags = .command, run: @escaping @MainActor () -> Void) -> NSMenuItem {
            let i = ActionItem(title: title, key: key, run: run)
            i.keyEquivalentModifierMask = mods
            return i
        }
        // File's adding and the Transfers menu: the core's commands, in its order.
        let model = commands.core.appCommands()
        func appItems(_ title: String) -> [NSMenuItem] {
            let entries = model.first { $0.title == title }?.entries ?? []
            return entries.enumerated().map { i, e in
                switch e {
                case .separator: .separator()
                case let .item(_, itemTitle, shortcut, _, _):
                    AppCommandItem(title: itemTitle, shortcut: shortcut, menu: title, index: i, commands: commands)
                }
            }
        }
        add(AppInfo.name, [
            act("Settings…", ",") { commands.showSettings(nil) },
            .separator(),
            NSMenuItem(title: "Hide \(AppInfo.name)", action: #selector(NSApplication.hide(_:)), keyEquivalent: "h"),
            .separator(),
            // ⌘Q only closes the windows: Seedwell keeps sharing from the menu bar (Quit is in its menu).
            NSMenuItem(title: "Close \(AppInfo.name)", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q"),
        ])
        add("File", appItems("File") + [
            act("Open \(AppInfo.name)", "l") { commands.showMain(nil, nil) },
            NSMenuItem(title: "Close Window", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w"),
        ])
        add("Edit", [
            NSMenuItem(title: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x"),
            NSMenuItem(title: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c"),
            NSMenuItem(title: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v"),
            NSMenuItem(title: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a"),
        ])
        add("Transfers", appItems("Transfers"))
        return bar
    }
}

/// Keeps App Nap away while peers are connected (`PowerNeeds.serving`): napping throttles the engine's timers and
/// its disk and network work, which slows seeding. The Mac may still idle-sleep.
@MainActor final class NapGuard {
    private var activity: NSObjectProtocol?

    func update(serving: Bool) {
        if serving == (activity != nil) { return }
        if serving {
            activity = ProcessInfo.processInfo.beginActivity(options: .userInitiatedAllowingIdleSystemSleep,
                                                             reason: "\(AppInfo.name) is sharing with peers")
        } else if let a = activity {
            ProcessInfo.processInfo.endActivity(a)
            activity = nil
        }
    }
}

/// Keeps the Mac from idle sleep while something downloads (setting "Prevent sleep while downloading") or an
/// agent works ("Keep awake while agents work"). The display still sleeps and the screen still locks.
@MainActor final class SleepAssertion {
    var enabled = true
    private var id: UInt32 = 0
    /// The reason it is held with (`pmset -g assertions` shows it).
    private var held: String?

    func update(downloading: Bool) { hold(downloading ? "\(AppInfo.name) is downloading" : nil) }

    func update(agents: AgentsAwake?) {
        hold(agents.map { "\(AppInfo.name): \(ListFormatter.localizedString(byJoining: $0.who)) working" })
    }

    /// Hold it for `reason`, or let go with `nil`.
    private func hold(_ reason: String?) {
        let want = enabled ? reason : nil
        if want == held { return }
        if held != nil {
            IOPMAssertionRelease(id)
            held = nil
        }
        guard let want else { return }
        var newID: UInt32 = 0
        let ok = IOPMAssertionCreateWithName("PreventUserIdleSystemSleep" as CFString, UInt32(kIOPMAssertionLevelOn),
                                             want as CFString, &newID)
        if ok == kIOReturnSuccess { id = newID; held = want }
    }
}
