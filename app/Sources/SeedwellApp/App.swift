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
    private var mainWindow: MainWindowController?
    var mainWindowController: MainWindowController? { mainWindow }
    var statusMenuController: StatusMenuController? { statusMenu }
    func closeChoosers() { chooserWindows.closeAll() }
    func qaChooserChange(to c: Category) { chooserWindows.qaChange(to: c) }
    private var chooserWindows = ChooserPresenter()
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
    /// QA keeps the app a regular, frontmost app so that captures show the windows active; the
    /// `policy-real` command lets the Dock logic run unpinned.
    var qaPinnedRegular = false
    private var askingToMove = false
    private var powerOff = false
    private var sleepAssertion = SleepAssertion()
    private var power: PowerWatcher?
    /// The OS network path and sleep/wake, told to the engine (SPEC 7C.4c).
    private(set) var networkWatcher: NetworkWatcher?

    init(options: LaunchOptions) {
        self.options = options
        // The real home folder as the system sees it, whatever `--home-root` did to `$HOME`.
        realHome = getpwuid(getuid()).flatMap { String(cString: $0.pointee.pw_dir) } ?? NSHomeDirectory()
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        signal(SIGTERM, SIG_IGN)
        let term = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
        term.setEventHandler { [weak self] in MainActor.assumeIsolated { self?.quitForReal() } }
        term.resume()
        termSource = term
        if let problem = options.configurationError { return fatal("Cannot start", problem) }
        let launchedByUser = Self.isUserLaunch(notification)
        log.notice("launch: by user \(launchedByUser), background \(self.options.background)")
        userLaunch = launchedByUser && !options.background && options.stateDir == nil && options.qaControl == nil
        startCore(waitedSecs: 0)
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
    private func startCore(waitedSecs: Int) {
        let dir = options.resolvedStateDir()
        try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        // First launch only: look at Transmission's port (read-only) before the engine picks a random one.
        if !options.mock || options.transmissionPrefs != nil { TransmissionPort.prepare(stateDir: dir, prefs: options.transmissionPrefs.map { URL(fileURLWithPath: $0) }) }
        do {
            core = try Core(options: CoreOptions(
                configDir: dir,
                mock: options.mock,
                allowLoopbackTrackers: options.allowLoopbackTrackers,
                forbiddenRoot: options.isolated ? realHome : nil))
        } catch {
            // The control socket says "another instance"; the state database, opened first, says it is locked.
            let busy = ["another instance", "Database already open", "Cannot acquire lock"].contains { "\(error)".contains($0) }
            if busy, waitedSecs < 150 {
                log.info("previous run still quitting; waiting (\(waitedSecs) s so far)")
                DispatchQueue.main.asyncAfter(deadline: .now() + 1) { [weak self] in
                    MainActor.assumeIsolated { self?.startCore(waitedSecs: waitedSecs + 1) }
                }
                return
            }
            return fatal("\(AppInfo.name) could not start", "\(error)")
        }
        commands = Commands(core: core)
        settings = SettingsModel(core: core)
        settings.refreshTransmissionOffer()
        if let snapshotDir = options.renderSnapshots {
            Task { @MainActor in
                await SnapshotRenderer(core: core, commands: commands, settings: settings, dir: snapshotDir).renderAll()
                self.quitForReal()
            }
            return
        }
        start()
    }

    private func fatal(_ title: String, _ text: String) {
        NSApp.setActivationPolicy(.regular)
        let a = NSAlert()
        a.messageText = title
        a.informativeText = text
        NSApp.activate()
        a.runModal()
        exit(1)
    }

    private func start() {
        UpdateCenter.shared.apply(settings.app.updates)
        UpdateCenter.shared.start()
        let main = MainWindowController(commands: commands, settings: settings, defaultSize: options.showWindowSize)
        mainWindow = main
        main.onOpenChange = { [weak self] in self?.refreshActivationPolicy() }
        commands.showMain = { section, info in main.show(section: section, info: info) }
        commands.showTrackerInfo = { id in main.show(section: .tracker(id)); main.libraryModel?.trackerInfo = true }
        commands.showTracker = { id in main.show(section: .tracker(id)) }
        commands.showAddLink = { main.show(); main.libraryModel?.adding = true }
        // Settings is a section of the main window: every way in opens it on the right tab.
        commands.showSettings = { tab in main.show(settings: tab, openSettings: true) }
        commands.editTracker = { id in main.show(settings: .rules, editTracker: id) }
        commands.createTracker = { host in main.show(settings: .rules, newTrackerHost: host) }
        commands.showTorrent = { id in main.showTorrent(id) }
        commands.showChooser = { [weak self] id in self?.chooserWindows.show(id: id, commands: self?.commands) }
        ChoiceStore.shared.engine = ChoiceEngine(
            load: { [core = self.core] id, reopen in try core.choice(id: id, reopen: reopen) },
            confirm: { [core = self.core] id, files in try core.confirmChoice(id: id, files: files) },
            cancel: { [core = self.core] id in try core.cancelChoice(id: id) },
            sourceFile: { [core = self.core] id in core.heldSourceFile(id: id) },
            hasSource: { [core = self.core] id in core.hasHeldSourceFile(id: id) },
            remove: { [core = self.core] id in core.remove(ids: [id], deleteFiles: false) },
            setCategory: { [core = self.core] id, c in core.setCategory(ids: [id], category: c) })
        ChoiceStore.shared.didResolve = { [weak self] id in
            self?.commands.arrivals.userReselected(id)  // a running torrent's files chosen again
            self?.chooserWindows.close(id)
        }
        FilesStore.shared.onTicked = { [weak self] id in self?.commands.arrivals.userReselected(id) }
        statusMenu = StatusMenuController(core: core, commands: commands, settings: settings)
        let notifier = StartingNotifier(commands: commands) { [weak self] in self?.statusMenu?.statusButton }
        startingNotifier = notifier
        statusMenu?.onWillOpen = { [weak notifier] in notifier?.stepAside() }
        commands.arrivals.onArrival = { [weak main] id in main?.newDownload(id) }
        commands.arrivals.note(core.snapshot())  // the baseline: what is downloading now is not new
        statusMenu?.onRefresh = { [weak self] s in
            self?.commands.arrivals.note(s)
            UpdateCenter.shared.noteActivity(busy: s.allDownloading.contains { !$0.isHeld } || s.movingPercent != nil)
            self?.sleepAssertion.update(downloading: s.allDownloading.contains { !$0.awaitingChoice && $0.state == .downloading })
        }
        NSApp.mainMenu = MainMenu.make(commands: commands)
        settings.applied = { [weak self] s in self?.apply(s) }
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
            qaPinnedRegular = true
            PlayerLauncher.blocked = true  // QA never starts the user's players on its own
            qa = QAControl(path: path, app: self)
        }
        offerBacklog()
        // Torrents still waiting when the app last quit are held again with a fresh countdown: show them.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { [weak self] in MainActor.assumeIsolated { self?.startingNotifier?.refresh(launch: true) } }
        if options.showWindow || userLaunch { main.show() }
        if let tab = options.showSettings { main.show(settings: SettingsTab(rawValue: tab), openSettings: true) }
    }

    /// Watch folders pick up only new files. The `.torrent` files that were already there when a
    /// folder joined the list are offered once: "Add 5 existing torrents from Downloads?"
    private func offerBacklog() {
        let marker = URL(fileURLWithPath: core.stateDir()).appendingPathComponent("backlog-offered")
        guard !options.mock, !FileManager.default.fileExists(atPath: marker.path) else { return }
        var offers: [(folder: WatchFolderSetting, files: [String])] = []
        for w in settings.app.folders.watch where w.enabled {
            let files = core.watchBacklog(folder: w.path)
            if !files.isEmpty { offers.append((w, files)) }
        }
        try? "offered".write(to: marker, atomically: true, encoding: .utf8)
        guard let offer = offers.first else { return }
        let n = offer.files.count
        let a = NSAlert()
        a.messageText = "Add \(n) existing torrent\(n == 1 ? "" : "s") from \((offer.folder.path as NSString).lastPathComponent)?"
        var then = "\(AppInfo.name) only picks up new files from a watched folder. These were already there."
        switch offer.folder.afterAdd {
        case .trash: then += " Added ones are moved to the Trash."
        case .archive: then += " Added ones are moved to the archive folder."
        case .leave: then += " The .torrent files stay where they are."
        }
        a.informativeText = then
        a.addButton(withTitle: "Add \(n)")
        a.addButton(withTitle: "Not Now")
        NSApp.activate()
        if a.runModal() == .alertFirstButtonReturn { for f in offer.files { commands.add(f) } }
    }

    /// Settings the shell implements itself.
    private func apply(_ s: AppSettings) {
        if Bundle.main.bundleIdentifier != nil, Bundle.main.bundlePath.hasSuffix(".app"), options.stateDir == nil {
            let service = SMAppService.mainApp
            let want = s.general.launchAtLogin
            if want, service.status != .enabled { try? service.register() }
            if !want, service.status == .enabled { try? service.unregister() }
        }
        sleepAssertion.enabled = s.general.preventSleepWhileDownloading
        UpdateCenter.shared.apply(s.updates)
        if power == nil, !options.mock { power = PowerWatcher(core: core) }
        power?.mode = s.general.onBattery
        statusMenu?.refreshIcon()
    }

    private func handle(_ event: CoreEvent) {
        let g = settings.app.general
        let trackers = core.snapshot().trackers
        let silenced = { (tracker: String?, kind: HiddenNotices.Kind) in HiddenNotices.silenced(tracker, kind, in: trackers) }
        switch event {
        case .finished(let row):
            if g.notifyFinished, !silenced(row.trackerId, .download) { Notifier.finished(row) }
            statusMenu?.refreshIcon()
        case .error(let id, let message):
            log.error("core error \(id ?? "-", privacy: .public): \(message, privacy: .public)")
            let snap = core.snapshot()
            let downloading = id.map { id in (snap.allDownloading + snap.allQueued).contains { $0.id == id } } ?? false
            if g.notifyErrors, !silenced(id.flatMap { core.trackerOf(id: $0) }, downloading ? .download : .sharing) {
                let name = id.flatMap { id in snap.allDownloading.first { $0.id == id }?.displayName } ?? "a transfer"
                Notifier.failed(name, message: message, id: id)
            }
            statusMenu?.refreshIcon()
        case .goalReached(let row):
            if g.notifyGoal, !silenced(row.trackerId, .sharing) { Notifier.goalReached(row.displayName) }
        case .needsChoice(let row):
            // "Choose Files…" on a magnet link asked for this chooser: open it, no notification.
            // The notifier popover is the notice, and holds the checklist (SPEC 8.6).
            let requested = startingNotifier?.consumeChooserRequest(row.id) == true
            startingNotifier?.refresh()
            if startingNotifier?.isShown != true {
                if requested { commands.showChooser(row.id) } else if !silenced(row.trackerId, .download) { Notifier.needsChoice(row) }
            }
            statusMenu?.refreshIcon()
        case .autoAdded(let a):
            // The notifier is the notification of a torrent that waits to start.
            if core.snapshot().allDownloading.first(where: { $0.id == a.id })?.pending == nil, !silenced(core.trackerOf(id: a.id), .download) { Notifier.autoAdded(a) }
            statusMenu?.refreshIcon()
        case .pending:
            startingNotifier?.refresh()
            statusMenu?.refreshIcon()
        case .duplicate(let name, let added):
            Notifier.duplicate(name, trackersAdded: added)
        case .addRequest(let r):
            chooserWindows.showAddRequest(r, commands: commands)
        case .agent(let a):
            settings.reloadActivity()
            if settings.app.agents.notifyChanges { Notifier.agent(a) }
        case .settingsChanged(_, let warnings, let restart):
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

    /// SPEC §7A.5: after a finished location changed, "Move the N existing torrents (X GB) to the new location?"
    private func askToMove(count: UInt32, bytes: UInt64) {
        guard count > 0, !askingToMove else { return }
        askingToMove = true
        defer { askingToMove = false }
        let proposal = core.locationMoves()
        let n = proposal.items.count
        guard n > 0 else { return }
        let a = NSAlert()
        let size = proposal.totalBytes > 0 ? " (\(Fmt.bytes(proposal.totalBytes)))" : ""
        a.messageText = "Move the \(n) existing torrent\(n == 1 ? "" : "s")\(size) to the new location?"
        var detail = "They keep seeding while they move."
        if proposal.crossVolume > 0 {
            detail += " \(proposal.crossVolume) on another disk, so their files are copied and checked first; the rest move instantly."
        } else {
            detail += " Same disk: the files move instantly, nothing is copied."
        }
        a.informativeText = detail
        a.addButton(withTitle: "Move")
        a.addButton(withTitle: "Don't Move")
        NSApp.activate()
        if a.runModal() == .alertFirstButtonReturn {
            let r = core.moveTorrents(ids: proposal.items.map(\.id))
            if !r.refused.isEmpty { commands.alert("Some torrents were not moved", r.refused.prefix(3).joined(separator: "\n")) }
        }
    }

    // MARK: URLs, notification clicks

    /// Double-clicked .torrent files and clicked magnet: links both arrive here.
    func application(_ application: NSApplication, open urls: [URL]) {
        commands.add(urls: urls)
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        let id = response.notification.request.content.userInfo["transfer"] as? String
        let isChoice = response.notification.request.identifier.hasPrefix("choice-")
        await MainActor.run {
            if let id, isChoice {
                commands.showChooser(id)
            } else {
                commands.showMain(nil, id)
            }
        }
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        [.banner]
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool { false }

    // MARK: Dock icon

    /// Seedwell is a menu bar app: it has a Dock icon and a place in the app switcher only while one of
    /// its windows (the library, with Settings inside it) is open.
    func refreshActivationPolicy() {
        guard !quitting else { return }
        let open = mainWindow?.isOpen ?? false
        let want: NSApplication.ActivationPolicy = open || qaPinnedRegular ? .regular : .accessory
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
        let p = NSApp.activationPolicy()
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
        settings.save()
        startingNotifier?.close()
        statusMenu?.hide()
        mainWindow?.closeForQuit()
        chooserWindows.closeAll()
        NSApp.setActivationPolicy(.prohibited)
        let secs: UInt32 = powerOff ? 3 : (options.mock ? 1 : 120)
        let core = self.core!
        DispatchQueue.global(qos: .utility).async {
            let r = core.shutdown(deadlineSecs: secs)
            os_log("shutdown: confirmed %d, abandoned %d, finished %d", r.confirmed, r.abandoned, r.finished ? 1 : 0)
            // The run loop waits for this reply in a modal mode, where plain GCD blocks do not run.
            RunLoop.main.perform(inModes: [.common, .default, .modalPanel, .eventTracking]) {
                MainActor.assumeIsolated {
                    // Never for an isolated (QA/dev) run: a plain relaunch would open the real state.
                    if self.reopenAfterQuit, self.options.stateDir == nil { Self.relaunchWhenGone() }
                    NSApp.reply(toApplicationShouldTerminate: true)
                }
            }
        }
        return .terminateLater
    }
}

/// Puts the check mark on Transfers ▸ Limit Speed while the alternative limits are on.
@MainActor final class LimitSpeedChecker: NSObject, NSMenuDelegate {
    private let isOn: @MainActor () -> Bool
    init(isOn: @escaping @MainActor () -> Bool) { self.isOn = isOn }

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.items.first { $0.title == "Limit Speed" }?.state = isOn() ? .on : .off
    }
}

@MainActor private var limitSpeedChecker: LimitSpeedChecker?

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
        add(AppInfo.name, [
            act("Settings…", ",") { commands.showSettings(nil) },
            .separator(),
            NSMenuItem(title: "Hide \(AppInfo.name)", action: #selector(NSApplication.hide(_:)), keyEquivalent: "h"),
            .separator(),
            // ⌘Q only closes the windows: Seedwell keeps sharing from the menu bar (Quit is in its menu).
            NSMenuItem(title: "Close \(AppInfo.name)", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q"),
        ])
        add("File", [
            act("Add Torrent File…", "o") { commands.chooseTorrentFile() },
            act("Add Magnet Link or URL…", "o", [.command, .shift]) { commands.showAddLink() },
            act("Open \(AppInfo.name)", "l") { commands.showMain(nil, nil) },
            NSMenuItem(title: "Close Window", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w"),
        ])
        add("Edit", [
            NSMenuItem(title: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x"),
            NSMenuItem(title: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c"),
            NSMenuItem(title: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v"),
            NSMenuItem(title: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a"),
        ])
        add("Transfers", [
            act("Pause All") { commands.core.pauseAll() },
            act("Resume All") { commands.core.resumeAll() },
            .separator(),
            act("Limit Speed", "l", [.command, .option]) { commands.core.setAltSpeed(on: !commands.core.snapshot().altSpeed) },
        ])
        // Limit Speed is a toggle: its check mark follows the engine whenever the menu opens.
        if let menu = bar.items.last?.submenu {
            limitSpeedChecker = LimitSpeedChecker(isOn: { commands.core.snapshot().altSpeed })
            menu.delegate = limitSpeedChecker
        }
        return bar
    }
}

/// Keeps the Mac awake while something downloads (setting "Prevent sleep while downloading").
@MainActor final class SleepAssertion {
    var enabled = true
    private var id: UInt32 = 0
    private var held = false

    func update(downloading: Bool) {
        let want = enabled && downloading
        if want == held { return }
        if want {
            var newID: UInt32 = 0
            let ok = IOPMAssertionCreateWithName("PreventUserIdleSystemSleep" as CFString, UInt32(kIOPMAssertionLevelOn),
                                                 "\(AppInfo.name) is downloading" as CFString, &newID)
            if ok == kIOReturnSuccess { id = newID; held = true }
        } else {
            IOPMAssertionRelease(id)
            held = false
        }
    }
}
