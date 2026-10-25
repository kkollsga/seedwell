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
    /// The Remove… waiting for its answer under the row.
    let removal = RemovalPrompt()
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
        favorites = FavoriteEditing(store: core)
        favorites.openTab = { [weak self] id in self?.showMain(.tracker(id), nil) }
        favorites.openInfo = { [weak self] id in self?.showTrackerInfo(id) }
    }

    // MARK: Adding

    func add(_ source: String) {
        do { _ = try core.addSource(source: source) } catch { alert("Couldn't add", "\(error)") }
    }

    func add(urls: [URL]) {
        for url in urls { add(url.isFileURL ? url.path : url.absoluteString) }
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
        do { if pause { try core.pendingPause(id: id) } else { try core.pendingResume(id: id) } } catch { log.error("countdown: \(error)") }
    }

    /// Start on a held torrent, from a row or a menu: one that waits for a choice takes the ticks of its shared list
    /// (everything unless changed); a countdown ends now.
    func startHeld(_ id: String, awaiting: Bool) {
        guard awaiting else { return pendingStartNow(id) }
        let store = ChoiceStore.shared
        _ = store.model(for: id)
        do { try store.confirm(id) } catch { alert("Couldn't start", "\(error)") }
    }

    /// Cancel on a held torrent: removed, its `.torrent` file to the Trash, or kept when Shift is down (SPEC 7A.6).
    func cancelHeld(_ id: String, keepFile: Bool? = nil) {
        let store = ChoiceStore.shared
        let keep = keepFile ?? (ShiftKey.down && store.hasSource(id))
        do { try store.cancel(id, keepFile: keep) } catch { core.remove(ids: [id], deleteFiles: false) }
    }

    func pendingStartNow(_ id: String) {
        do { try core.pendingStartNow(id: id) } catch { log.error("start now: \(error)") }
    }

    /// "Choose Files…": ends the countdown, keeps the torrent held and opens the chooser. A magnet link
    /// has no file list yet: its metadata is fetched now and the chooser follows (`afterMetadata`).
    @discardableResult func pendingChoose(_ id: String, openWindow: Bool = true) -> ChooseResult {
        do {
            if try core.pendingChoose(id: id) {
                if openWindow { showChooser(id) }
                return .open
            }
            return .afterMetadata
        } catch {
            log.error("choose files: \(error)")
            return .failed
        }
    }

    // MARK: Row actions

    func reveal(_ rows: [TransferRow]) {
        NSWorkspace.shared.activateFileViewerSelecting(rows.map(\.url))
    }

    /// Get Info's "Copy URL": the real announce URL (passkey included) goes to the pasteboard and nowhere else.
    func copyTrackerURL(id: String, index: UInt32) {
        guard let url = try? core.trackerUrl(id: id, index: index) else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(url, forType: .string)
    }

    /// "Ask Tracker Again": a manual re-announce. The answer says whether it went out or when it may.
    func askTrackerAgain(_ rows: [TransferRow]) {
        let out = core.askTrackerAgain(ids: rows.map(\.id))
        if out.sent > 0 {
            playNote("Asked the tracker again.", symbol: "arrow.clockwise")
        } else if let w = out.waitSecs {
            playNote("The tracker can be asked again in \(Fmt.until(w)).", symbol: "clock")
        }
    }

    /// "Announce anyway": also past the tracker's minimum interval (the engine allows one per tracker per 5 minutes).
    func announceAnyway(_ rows: [TransferRow]) {
        let out = core.announceAnyway(ids: rows.map(\.id))
        if out.sent > 0 {
            playNote("Asked the tracker again.", symbol: "arrow.clockwise")
        } else if let w = out.waitSecs {
            playNote("Announcing anyway is limited to once in 5 minutes; try again in \(Fmt.until(w)).", symbol: "clock")
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
        removal.ask(rows, deleteFiles: deleteFiles)
    }

    func confirmedRemove() {
        guard let p = removal.pending else { return }
        removal.cancel()
        core.remove(ids: p.ids, deleteFiles: p.deleteFiles)
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
            core.setSeedingGoal(ids: rows.map(\.id), goal: .ratio(ratio: ratio))
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
        do {
            let r = try core.moveTo(ids: rows.map(\.id), path: core.expandPath(path: picked))
            if !r.refused.isEmpty { alert("Not moved", r.refused.prefix(3).joined(separator: "\n")) }
        } catch { alert("Not moved", "\(error)") }
    }

    // MARK: Menu model (SPEC §8.5)

    /// `inMenuBar`: the menu bar's version also offers "Files…".
    func entries(for rows: [TransferRow], inMenuBar: Bool = false) -> [MenuEntry] {
        guard let first = rows.first else { return [] }
        let ids = rows.map(\.id)
        let single = rows.count == 1
        let sharing = first.isSharing
        let info = single ? try? core.info(id: first.id) : nil
        var out: [MenuEntry] = []

        // Play first (SPEC §7F.3): only for a single torrent that holds a video selected for download.
        if single {
            let play = playEntries(for: first)
            if !play.isEmpty { out += play + [.separator] }
        }

        // Actions
        if rows.contains(where: \.canAskAgain) {
            out.append(.item("Ask Tracker Again") { self.askTrackerAgain(rows.filter(\.canAskAgain)) })
        }
        if first.awaitingChoice {
            if single {
                out.append(.item("Start") { self.startHeld(first.id, awaiting: true) })
                out.append(.item("Cancel") { self.cancelHeld(first.id) })
            }
            out.append(.item("Choose Files…") { self.showChooser(first.id) })
        } else if first.pending != nil, single {
            out.append(.item("Start") { self.startHeld(first.id, awaiting: false) })
            out.append(.item("Cancel") { self.cancelHeld(first.id) })
            out.append(.item("Choose Files…") { self.pendingChoose(first.id) })
        } else if sharing {
            if first.state == .paused {
                out.append(.item("Resume Sharing") { self.core.resume(ids: ids) })
            } else {
                out.append(.item("Stop Sharing") { self.core.stopSharing(ids: ids) })
            }
        } else {
            let paused = first.state == .paused
            out.append(.item(paused ? "Resume" : "Pause") { paused ? self.core.resume(ids: ids) : self.core.pause(ids: ids) })
            if single { out.append(.item(inMenuBar ? "Move to Top of Queue" : "Move to Top") { self.core.moveToTop(id: first.id) }) }
        }
        out.append(.separator)

        // Reveal / info
        out.append(.item("Show in Finder") { self.reveal(rows) })
        if single { out.append(.item("Get Info", key: "⌘I") { self.showMain(nil, first.id) }) }
        if !inMenuBar, !first.isHeld { out.append(.item("Move to…") { self.moveTo(rows) }) }
        out.append(.separator)

        // Settings submenus
        if sharing {
            let custom = info?.goalIsCustom ?? false
            let goal = first.ratioGoal
            out.append(.submenu("Seeding Goal", [
                .item("Rule default", checked: single && !custom) { self.core.setSeedingGoal(ids: ids, goal: .ruleDefault) },
                .separator,
                .item("Ratio 1.0", checked: single && custom && goal == 1.0) { self.core.setSeedingGoal(ids: ids, goal: .ratio(ratio: 1)) },
                .item("Ratio 3.0", checked: single && custom && goal == 3.0) { self.core.setSeedingGoal(ids: ids, goal: .ratio(ratio: 3)) },
                .item("Seed Forever", checked: single && custom && goal == nil) { self.core.setSeedingGoal(ids: ids, goal: .forever) },
                .separator,
                .item("Custom…") { self.customGoal(rows) },
            ]))
        }
        out.append(.submenu("Priority", Priority.all.map { p in
            .item(p == .high ? "★ High" : p.label, checked: info?.priority == p) { self.core.setPriority(ids: ids, priority: p) }
        }))
        do {
            let title = single ? "Category: \(first.category.label)" : "Category"
            var cats: [MenuEntry] = Category.all.map { c in
                .item(c.label, checked: single && c == first.category) { self.core.setCategory(ids: ids, category: c) }
            }
            if single { cats += [.separator, .item("Detected: \(first.category.label)", enabled: false) {}] }
            out.append(.submenu(title, cats))
        }
        out.append(ruleMenu(ids: ids, current: info?.ruleName))
        if inMenuBar && chooserAvailable && !sharing && single && first.pending == nil {
            out.append(.item("Files…") { self.showChooser(first.id) })
        }
        out.append(.separator)

        // Utilities, then destructive last
        if sharing { out.append(.item("Verify Files") { self.core.verify(ids: ids) }) }
        if single && first.kind == .torrent {
            out.append(.item("Copy Magnet Link") { self.copyMagnet(first) })
        }
        out.append(.separator)
        out.append(.item("Remove…") { self.confirmRemove(rows, deleteFiles: false, openWindow: inMenuBar) })
        out.append(.item("Remove and Delete Files…", role: .destructive) { self.confirmRemove(rows, deleteFiles: true, openWindow: inMenuBar) })
        return out
    }

    private func ruleMenu(ids: [String], current: String?) -> MenuEntry {
        let rules = core.rulesLoad().rules.filter(\.enabled)
        var items: [MenuEntry] = [
            .item("Automatic, by tracker", checked: current == "Default") { self.core.setRule(ids: ids, rule: nil) },
            .separator,
        ]
        for r in rules {
            items.append(.item(r.name, checked: current == r.name) { self.core.setRule(ids: ids, rule: r.id) })
        }
        items += [.separator, .item("Edit Rules…") { self.showSettings(.rules) }]
        return .submenu("Rule: \(current ?? "Default")", items)
    }
}
