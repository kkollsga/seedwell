import AppKit
import os

/// One description of a context menu, rendered both as an NSMenu (menu bar) and as SwiftUI
/// (main window), so the two can never drift apart (SPEC §8.5).
indirect enum MenuEntry {
    enum Role { case normal, destructive }
    case item(String, checked: Bool = false, enabled: Bool = true, role: Role = .normal, key: String? = nil,
              run: @MainActor () -> Void)
    case submenu(String, [MenuEntry])
    case separator
}

/// Every user command, in one place. The menu, the main window and URL handling all go through
/// here, and from here to the core.
@MainActor final class Commands {
    let core: Core
    /// Wired by the app delegate.
    var showMain: (_ section: LibraryModel.Section?, _ info: String?) -> Void = { _, _ in }
    var showSettings: (_ tab: SettingsTab?) -> Void = { _ in }
    /// Settings › Devices with Pair with a Link… open on this link (a `seedwell://devices/` link opened the app).
    var pairDevice: (_ link: String) -> Void = { _ in }
    /// Opens the main window on a tracker's Info page.
    var showTrackerInfo: (_ id: String) -> Void = { _ in }
    /// The main window with the "Add a Download" sheet (magnet link or URL) open.
    var showAddLink: () -> Void = {}
    /// Opens the chooser (SPEC §7A.6) for a torrent that awaits a choice, or to change its files.
    /// Opens the main window on Sharing, filtered to one tracker.
    var showTracker: (_ id: String) -> Void = { _ in }
    var showChooser: (_ id: String) -> Void = { _ in }
    /// The main window on Downloads with this torrent selected, scrolled to and its file list open (the notifier's title).
    var showTorrent: (_ id: String) -> Void = { _ in }
    /// The engine can hold a torrent for a choice and reopen it later (the other agent's API).
    var chooserAvailable = true
    /// Opens Settings ▸ Rules on a tracker profile ("Edit Tracker…").
    var editTracker: (_ id: String) -> Void = { _ in }
    /// Opens Settings ▸ Rules ▸ Trackers with a new profile for this host ("Create Tracker Profile…").
    var createTracker: (_ host: String) -> Void = { _ in }
    /// The calm note under the list when Play cannot start (SPEC §7F.3).
    let playNotice = PlayNotice()
    /// Asks the main window's Remove… question (wired by the app delegate; the window's core keeps it).
    var askRemoval: (_ rows: [TransferRow], _ deleteFiles: Bool) -> Void = { _, _ in }
    /// Favorite trackers: the inline renaming and address editing, and the question of removing one.
    let favorites: FavoriteEditing
    /// Which torrents just became downloads: the window switches to them, the menu lists them first.
    let arrivals = DownloadWatcher()
    /// Folder sync (SPEC 7G): its commands and the modals.
    let sync: SyncActions
    /// Channels (SPEC 7G.7): their commands, the New Channel window and notifications.
    let chat: ChatActions
    private let log = Logger(subsystem: "io.github.kkollsga.Seedwell", category: "commands")

    init(core: Core) {
        self.core = core
        sync = SyncActions(core: core)
        chat = ChatActions(core: core, sync: sync)
        favorites = FavoriteEditing(core: core)
        favorites.openTab = { [weak self] id in self?.showMain(.tracker(trackerId: id), nil) }
        favorites.openInfo = { [weak self] id in self?.showTrackerInfo(id) }
    }

    // MARK: Adding

    func add(_ source: String) {
        let core = self.core
        CoreCalls.inOrder({ await catching { try await core.addSource(source: source) } }) { [weak self] r in
            if case .failure(let error) = r {
                let a = addFailed(error: SyncText.message(error))
                self?.alert(a.title, a.text)
            }
        }
    }

    func add(urls: [URL]) {
        for url in urls { add(url.isFileURL ? url.path : url.absoluteString) }
    }

    /// Opens what a link asks for, as the core routes it (`Core.routeLink`).
    func open(_ route: LinkRoute) {
        switch route {
        case .addFolder(let link): sync.showAdd(link)
        case .joinChannel(let link): chat.showNew(prefill: link)
        case .pairDevice(let link): pairDevice(link)
        case .sharePath(let path): sync.share(path: path)
        case .showFolder(let id): sync.showFolder(id, false)
        case .shareFolder(let id): sync.showShare(id)
        case .addSource(let source): add(source)
        case let .problem(title, text): alert(title, text)
        }
    }

    func chooseTorrentFile() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [.init(filenameExtension: "torrent") ?? .data]
        panel.allowsMultipleSelection = true
        if panel.runModal() == .OK { add(urls: panel.urls) }
    }

    // MARK: Waiting to start (SPEC 7A.7)

    enum ChooseResult { case open, afterMetadata, failed }

    /// Pause or resume the countdown of a torrent that waits to start. The torrent stays held, silent.
    func pendingCountdown(_ id: String, pause: Bool) {
        let (core, log) = (self.core, self.log)
        CoreCalls.inOrder {
            do { if pause { try await core.pendingPause(id: id) } else { try await core.pendingResume(id: id) } } catch { log.error("countdown: \(error)") }
        }
    }

    /// Start on a held torrent, from a row or a menu: one that waits for a choice takes the ticks of its shared list
    /// (everything unless changed); a countdown ends now.
    func startHeld(_ id: String, awaiting: Bool) {
        guard awaiting else { return pendingStartNow(id) }
        // The core loads the list first when none showed it, and starts with its default ticks.
        ChoiceStore.shared.confirm(id) { [weak self] error in self?.alert("Couldn't start", "\(error)") }
    }

    /// Cancel on a held torrent: removed, its `.torrent` file to the Trash, or kept when Shift is down (SPEC 7A.6).
    func cancelHeld(_ id: String, keepFile: Bool? = nil) {
        let store = ChoiceStore.shared
        let keep = keepFile ?? (ShiftKey.down && store.hasSource(id))
        store.cancel(id, keepFile: keep)
    }

    func pendingStartNow(_ id: String) {
        let (core, log) = (self.core, self.log)
        CoreCalls.inOrder { do { try await core.pendingStartNow(id: id) } catch { log.error("start now: \(error)") } }
    }

    /// "Choose Files…": ends the countdown, keeps the torrent held and opens the chooser. A magnet link
    /// has no file list yet: its metadata is fetched now and the chooser follows (`afterMetadata`). `done` gets the
    /// outcome once the engine answered.
    func pendingChoose(_ id: String, openWindow: Bool = true, done: @escaping @MainActor (ChooseResult) -> Void = { _ in }) {
        let (core, log) = (self.core, self.log)
        CoreCalls.inOrder({ await catching { try await core.pendingChoose(id: id) } }) { [weak self] r in
            switch r {
            case .success(true):
                if openWindow { self?.showChooser(id) }
                done(.open)
            case .success(false):
                done(.afterMetadata)
            case .failure(let error):
                log.error("choose files: \(error)")
                done(.failed)
            }
        }
    }

    // MARK: Row actions

    /// Pause and Resume of a row's own buttons (the core's commands, in order, off the main thread).
    func pause(_ ids: [String]) { let core = self.core; CoreCalls.inOrder { await core.pause(ids: ids) } }

    func resume(_ ids: [String]) { let core = self.core; CoreCalls.inOrder { await core.resume(ids: ids) } }

    /// Forgets the Recent list (the finished torrents keep seeding).
    func clearRecent() { let core = self.core; CoreCalls.inOrder { await core.clearRecent() } }

    func reveal(_ rows: [TransferRow]) {
        NSWorkspace.shared.activateFileViewerSelecting(rows.map(\.url))
    }

    /// Get Info's "Copy URL": the real announce URL (passkey included) goes to the pasteboard and nowhere else.
    func copyTrackerURL(id: String, index: UInt32) {
        let core = self.core
        CoreCalls.run({ try? await core.trackerUrl(id: id, index: index) }) { url in
            guard let url else { return }
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(url, forType: .string)
        }
    }

    /// "Ask Tracker Again": a manual re-announce. The answer says whether it went out or when it may.
    func askTrackerAgain(_ rows: [TransferRow]) {
        let (core, ids) = (self.core, rows.map(\.id))
        CoreCalls.inOrder({ await core.askTrackerAgain(ids: ids) }) { [weak self] out in
            if out.sent > 0 {
                self?.playNote("Asked the tracker again.", symbol: "arrow.clockwise")
            } else if let w = out.waitSecs {
                self?.playNote("The tracker can be asked again in \(Fmt.until(w)).", symbol: "clock")
            }
        }
    }

    /// "Announce anyway": also past the tracker's minimum interval (the engine allows one per tracker per 5 minutes).
    func announceAnyway(_ rows: [TransferRow]) {
        let (core, ids) = (self.core, rows.map(\.id))
        CoreCalls.inOrder({ await core.announceAnyway(ids: ids) }) { [weak self] out in
            if out.sent > 0 {
                self?.playNote("Asked the tracker again.", symbol: "arrow.clockwise")
            } else if let w = out.waitSecs {
                self?.playNote("Announcing anyway is limited to once in 5 minutes; try again in \(Fmt.until(w)).", symbol: "clock")
            }
        }
    }

    func copyMagnet(_ row: TransferRow) {
        guard let link = try? core.magnetLink(id: row.id) else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(link, forType: .string)
    }

    /// Every Remove… goes here: the question slides down under the torrent's row in the main window
    /// (no alert). From the menu bar the window opens on the row's section first.
    func confirmRemove(_ rows: [TransferRow], deleteFiles: Bool, openWindow: Bool = false) {
        guard let first = rows.first else { return }
        if openWindow { showMain(first.isSharing ? .sharing : .downloads, nil) }
        askRemoval(rows, deleteFiles)
    }

    func customGoal(_ rows: [TransferRow]) {
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 160, height: 24))
        field.stringValue = "2.0"
        let a = NSAlert()
        a.messageText = "Seed until ratio…"
        a.accessoryView = field
        a.addButton(withTitle: "Set")
        a.addButton(withTitle: "Cancel")
        NSApp.activate()
        if a.runModal() == .alertFirstButtonReturn, let ratio = Double(field.stringValue.replacingOccurrences(of: ",", with: ".")) {
            let (core, ids) = (self.core, rows.map(\.id))
            CoreCalls.inOrder { await core.setSeedingGoal(ids: ids, goal: .ratio(ratio: ratio)) }
        }
    }

    func alert(_ title: String, _ text: String) {
        let a = NSAlert()
        a.messageText = title
        a.informativeText = text
        NSApp.activate()
        a.runModal()
    }

    /// "Move to…": below a folder the user picks, pinned there (SPEC §7A.5).
    func moveTo(_ rows: [TransferRow]) {
        guard let picked = FolderPicker.choose(start: rows.first?.savePath ?? "~") else { return }
        let (core, ids, path) = (self.core, rows.map(\.id), self.core.expandPath(path: picked))
        CoreCalls.inOrder({ await catching { try await core.moveTo(ids: ids, path: path) } }) { [weak self] r in
            switch r {
            case .success(let r): if let a = moveFailed(result: r) { self?.alert(a.title, a.text) }
            case .failure(let error):
                let a = moveError(error: (error as? CoreError) ?? .Failed(message: "\(error)"))
                self?.alert(a.title, a.text)
            }
        }
    }

    // MARK: Menu model (SPEC §8.5): the list is the core's (`rowCommands`); the app draws it and runs its commands.

    /// The commands of one row or a selection. `inMenuBar`: the menu bar's version ("Move to Top of Queue", "Files…").
    func entries(for rows: [TransferRow], inMenuBar: Bool = false) -> [MenuEntry] {
        guard let first = rows.first else { return [] }
        let single = rows.count == 1
        // Both read within a few milliseconds or as last read (the core's `info`, `play_offer`): a busy engine never
        // holds the menu up.
        let info = single ? try? core.info(id: first.id) : nil
        let play = single ? playOffer(first) : nil
        let list = rowCommands(rows: rows, info: info, rules: core.rulesLoad().rules, play: play,
                               inMenuBar: inMenuBar, chooserAvailable: chooserAvailable, host: hostWindowsAll())
        return draw(list, rows: rows, videos: play?.videos ?? [], inMenuBar: inMenuBar)
    }

    /// A command list as menu entries. `videos` are the files its Play commands name; `then` runs after each command
    /// (Get Info reads the torrent again).
    func draw(_ list: [CommandEntry], rows: [TransferRow], videos: [TorrentFile], inMenuBar: Bool,
              then: @escaping @MainActor () -> Void = {}) -> [MenuEntry] {
        list.map { entry in
            switch entry {
            case .separator:
                return .separator
            case let .note(title):
                return .item(title, enabled: false) {}
            case let .submenu(title, entries):
                return .submenu(title, draw(entries, rows: rows, videos: videos, inMenuBar: inMenuBar, then: then))
            case let .item(command, title, checked, enabled, destructive, shortcut):
                return .item(title, checked: checked, enabled: enabled, role: destructive ? .destructive : .normal,
                             key: shortcut.map { "⌘" + $0 }) {
                    self.run(command, rows: rows, videos: videos, inMenuBar: inMenuBar)
                    then()
                }
            }
        }
    }

    /// What each command does here: a core call, Finder, the pasteboard, a window or a dialog.
    func run(_ command: RowCommand, rows: [TransferRow], videos: [TorrentFile], inMenuBar: Bool) {
        guard let first = rows.first else { return }
        UIAction.mark("row command \(command)")
        let ids = rows.map(\.id)
        let core = self.core
        switch command {
        case let .askTrackerAgain(asked): askTrackerAgain(rows.filter { asked.contains($0.id) })
        case .confirmChoice: startHeld(first.id, awaiting: true)
        case .startNow: startHeld(first.id, awaiting: false)
        case .cancelHeld: cancelHeld(first.id)
        case .openChooser: showChooser(first.id)
        case .chooseFiles: pendingChoose(first.id)
        case .resume: CoreCalls.inOrder { await core.resume(ids: ids) }
        case .pause: CoreCalls.inOrder { await core.pause(ids: ids) }
        case .stopSharing: CoreCalls.inOrder { await core.stopSharing(ids: ids) }
        case .moveToTop: CoreCalls.inOrder { await core.moveToTop(id: first.id) }
        case .reveal: reveal(rows)
        case .getInfo: showMain(nil, first.id)
        case .moveTo: moveTo(rows)
        case let .setSeedingGoal(goal): CoreCalls.inOrder { await core.setSeedingGoal(ids: ids, goal: goal) }
        case .customGoal: customGoal(rows)
        case let .setPriority(priority): CoreCalls.inOrder { await core.setPriority(ids: ids, priority: priority) }
        case let .setCategory(category): CoreCalls.inOrder { await core.setCategory(ids: ids, category: category) }
        case let .setRule(ruleId): CoreCalls.inOrder { await core.setRule(ids: ids, rule: ruleId) }
        case .editRules: showSettings(.rules)
        case .verify: CoreCalls.inOrder { await core.verify(ids: ids) }
        case .copyMagnet: copyMagnet(first)
        case .remove: confirmRemove(rows, deleteFiles: false, openWindow: inMenuBar)
        case .removeAndDeleteFiles: confirmRemove(rows, deleteFiles: true, openWindow: inMenuBar)
        case .playRow: playButtonPressed(first)
        case let .play(fileIndex, playerId): play(first, fileIndex: fileIndex, playerId: playerId)
        }
    }
}
