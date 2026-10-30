import AppKit
import os

/// The menu bar item and its plain native NSMenu (SPEC §8.2). The menu is rebuilt when it opens
/// and refreshed once a second only while it is open; when closed nothing polls the core.
///
/// What the menu and the icon show is the core's (`menuModel`, `IconState`: sections, rows, texts, signatures,
/// cadence); this draws it and runs the commands.
@MainActor final class StatusMenuController: NSObject, NSMenuDelegate {
    private let core: Core
    private let commands: Commands
    private let settings: SettingsModel?
    private var statusItem: NSStatusItem?
    /// The menu of the running app (also in a quiet QA run, which has no menu bar item), not a picture of it.
    private var live: Bool
    /// What the icon shows now; it is redrawn only when this changes.
    private var iconLook: IconLook?
    /// The icon's model (it remembers when a move started).
    private let icon = IconState()
    /// Called with every snapshot the icon refresh reads (the sleep assertion follows it).
    var onRefresh: ((Snapshot) -> Void)?
    /// The menu is about to open: the notifier steps aside for it.
    var onWillOpen: (() -> Void)?
    /// The button of the menu bar item: the notifier anchors to it.
    var statusButton: NSStatusBarButton? { statusItem?.button }
    private let menu = NSMenu(title: AppInfo.name)
    private var timer: Timer?
    /// The icon's next look, when the core says (`IconModel.nextRefreshMs`); events ask sooner.
    private var iconTimer: Timer?
    private var rightClickMonitor: Any?
    private var signature = ""
    private var rowItems: [String: NSMenuItem] = [:]
    /// The snapshot's rows, for the commands (right-click, reveal, Play, the file list).
    private var rows: [String: TransferRow] = [:]
    /// The menu's rows as the core last gave them (the check flips here at once; the next refresh confirms it).
    private var models: [String: MenuRow] = [:]
    /// The Sharing line: a row of its own, so the rate sits at the right edge as plain text.
    private var sharingRow: MenuCheckRow?
    /// The Folder Sync line and its submenu (SPEC 8.2 item 5).
    private var syncItem: NSMenuItem?
    private(set) lazy var menuSync = MenuSync(actions: commands.sync)
    /// The inline file lists of the torrent rows (lazy: nothing is fetched until one opens).
    private(set) lazy var menuFiles = MenuFiles(core: core, commands: commands)
    /// The Open / Quit row (QA draws its hover state).
    private(set) var footerRow: FooterRow?
    private let log = Logger(subsystem: "io.github.kkollsga.Seedwell", category: "menu")

    /// `inMenuBar: false` (the visual QA run only): no menu bar item, so the menu can be built and drawn offscreen. A
    /// quiet QA run (QuietRun) has no menu bar item either, but its menu and icon state run as usual.
    init(core: Core, commands: Commands, settings: SettingsModel? = nil, inMenuBar: Bool = true) {
        self.core = core
        self.commands = commands
        self.settings = settings
        live = inMenuBar
        statusItem = inMenuBar && !QuietRun.on ? NSStatusItem.make() : nil
        super.init()
        menu.delegate = self
        menu.autoenablesItems = false
        statusItem?.menu = menu
        menuFiles.onMoved = { [weak self] in self?.refreshHover() }
        refreshIcon()
    }

    /// QA: presses one side of the footer row as a click would.
    func qaPressFooter(_ side: String) { footerRow?.qaPress(side) }

    /// QA: opens or closes the menu as a click on the icon would. A quiet QA run shows no menu: it opens and closes
    /// it as the menu itself would (built, refreshed once a second, the icon's open cadence) without tracking it on a
    /// screen; QA draws its items (`shot … menu`).
    func perform(open: Bool) {
        if QuietRun.on {
            if open, !qaMenuOpen { menuWillOpen(menu) } else if !open, qaMenuOpen { menuDidClose(menu) }
            return
        }
        if open { statusItem?.button?.performClick(nil) } else { menu.cancelTracking() }
    }

    /// Quit hides everything at once (SPEC §12 #16): the icon goes while the engine finishes its announces.
    func hide() {
        timer?.invalidate(); iconTimer?.invalidate()
        timer = nil; iconTimer = nil
        if let item = statusItem { NSStatusBar.system.removeStatusItem(item) }
        statusItem = nil
        live = false
    }

    // MARK: Icon (event-driven, plus a timer at the cadence the core gives)

    func refreshIcon() {
        guard live, statusItem != nil || QuietRun.on else { return }  // the visual QA run's copy draws, nothing more
        let s = core.snapshot()
        onRefresh?(s)
        let m = icon.icon(snapshot: s, nowUnixMs: Date().unixMs, menuOpen: qaMenuOpen)
        if m.look != iconLook {
            iconLook = m.look
            statusItem?.button?.image = Drawing.statusIcon(down: m.look.down, up: m.look.up, progress: m.look.progress, moving: m.look.moving, attention: m.look.attention)
        }
        statusItem?.button?.setAccessibilityLabel(m.accessibilityLabel)
        iconTimer?.invalidate()
        let t = Timer(timeInterval: Double(m.nextRefreshMs) / 1000, repeats: false) { [weak self] _ in MainActor.assumeIsolated { self?.refreshIcon() } }
        t.tolerance = Double(m.refreshLeewayMs) / 1000
        RunLoop.main.add(t, forMode: .common)
        iconTimer = t
    }

    // MARK: NSMenuDelegate

    func menuWillOpen(_ menu: NSMenu) {
        guard menu === self.menu else { return }
        onWillOpen?()
        qaMenuOpen = true
        pinToSRGB()
        let sync = core.syncSnapshot()
        menuSync.menuOpened(sync)
        let s = core.snapshot()
        let m = model(s, sync)
        rebuild(s, m)
        let t = Timer(timeInterval: Double(m.refreshMs) / 1000, repeats: true) { [weak self] _ in MainActor.assumeIsolated { self?.tick() } }
        t.tolerance = 0.1
        // eventTracking mode: timers in the default mode do not fire while a menu is tracking.
        RunLoop.main.add(t, forMode: .common)
        timer = t
        refreshIcon()  // the open menu's cadence
        installRightClickMonitor()
    }

    /// Memory: the menu's pop-up window is the system's; with the display's wide-gamut profile it builds
    /// ColorSync conversions of 270 KB per tone curve and keeps them (about 3 MB per window). Pinned
    /// to sRGB like every other window (see `Windows`), its conversions are the cheap identity.
    private func pinToSRGB() {
        for w in NSApp.windows where w.colorSpace != Windows.space { w.colorSpace = Windows.space }
    }

    func menuDidClose(_ menu: NSMenu) {
        guard menu === self.menu else { return }
        timer?.invalidate()
        timer = nil
        if let m = rightClickMonitor { NSEvent.removeMonitor(m) }
        rightClickMonitor = nil
        clearHover()
        rowItems = [:]
        models = [:]
        qaMenuOpen = false
        menuFiles.menuClosed()
        refreshIcon()
    }

    /// Only the row NSMenu highlights (or, when it highlights none, the one under the pointer) looks hovered.
    func menu(_ menu: NSMenu, willHighlight item: NSMenuItem?) {
        guard menu === self.menu else { return }
        refreshHover(highlighted: item)
    }

    // MARK: Hover

    private var hoverRows: [any HoverRow] {
        var all: [any HoverRow] = rowItems.values.compactMap { $0.view as? MenuTorrentRow }
        if let r = sharingRow { all.append(r) }
        all += menuFiles.hoverRows
        all += footerRow?.hoverRows ?? []
        return all
    }

    /// One row at most is lit. A row keeps its own tracking, but a missed mouse-exit (a fast move, a scroll,
    /// rows inserted under the pointer) would leave it lit, so the highlight is worked out again from where
    /// the pointer is and which item NSMenu has highlighted.
    private func refreshHover(highlighted: NSMenuItem?? = nil) {
        guard qaMenuOpen else { return }
        let item = highlighted ?? menu.highlightedItem
        var taken = false
        for v in hoverRows {
            var on = false
            if !taken {
                if let owner = item?.view, v.isDescendant(of: owner) {
                    // A torrent row is lit by the highlight itself (the keyboard too); list rows follow the pointer.
                    on = v is MenuTorrentRow || v.menuPointerInside
                } else if item == nil {
                    on = v.menuPointerInside
                }
            }
            if on { taken = true }
            v.hovered = on
        }
    }

    private func clearHover() {
        for v in hoverRows { v.hovered = false }
    }

    // MARK: Build and refresh

    private func tick() {
        let s = core.snapshot()
        let sync = core.syncSnapshot()
        let m = model(s, sync)
        // An open file list is not rebuilt under the pointer (ticking a file can move the row).
        if m.signature != signature, !menuFiles.isOpen { rebuild(s, m) } else { update(s, m) }
        menuSync.update(sync)
    }

    /// The menu as the core lays it out, with the torrents that just arrived first (they are noted here too).
    private func model(_ s: Snapshot, _ sync: SyncSnapshot) -> MenuModel {
        commands.arrivals.note(s)
        return menuModel(snapshot: s, sync: sync, arrivals: commands.arrivals.arrivals, nowUnixMs: Date().unixMs,
                         updateVersion: UpdateCenter.shared.availableVersion)
    }

    /// QA: the titles of the open menu, headers first ("Downloading|name|name||Recent|…").
    var qaTitles: String {
        menu.items.filter { !$0.isHidden && ($0.view == nil || $0.view is MenuTorrentRow || $0.view is MenuCheckRow) }
            .map { ($0.view as? MenuCheckRow).map { "\($0.content.title) \($0.content.trailing)" } ?? ($0.isSeparatorItem ? "-" : $0.attributedTitle?.string ?? $0.title) }
            .joined(separator: "|")
    }

    /// QA: the subtitles of the open menu's rows ("42% · 3.1 MB/s · 4 min left|…").
    var qaSubtitles: String {
        menu.items.filter { !$0.isHidden }.compactMap(\.subtitle).joined(separator: "|")
    }

    /// QA: the menu items (top level).
    var qaItems: [NSMenuItem] { menu.items }

    /// QA (the visual QA run): builds the menu's items from the core as opening the menu does, without opening it
    /// (no refresh timer, no right-click monitor, no hover tracking). The items keep the views the menu shows.
    func qaBuildItems() -> [NSMenuItem] {
        let sync = core.syncSnapshot()
        menuSync.menuOpened(sync)
        let s = core.snapshot()
        rebuild(s, model(s, sync))
        return menu.items
    }

    /// QA: the menu is tracking.
    private(set) var qaMenuOpen = false

    /// QA: the titles of the right-click menu of the open menu's row whose title contains `key`, plus the row's
    /// check ("check on|off|none") and Play icon ("play yes|no").
    func qaRowContext(_ key: String) -> String {
        guard let item = rowItems.values.first(where: { ($0.attributedTitle?.string ?? $0.title).localizedCaseInsensitiveContains(key) }),
              let id = item.representedObject as? String, let row = rows[id] else { return "none" }
        let titles = commands.entries(for: [row], inMenuBar: true).map { e -> String in
            switch e {
            case .separator: return "-"
            case let .item(title, _, _, _, _, _): return title
            case let .submenu(title, _): return "\(title) >"
            }
        }.joined(separator: "|")
        let view = item.view as? MenuTorrentRow
        let check = view?.running.map { $0 ? "on" : "off" } ?? "none"
        return "\(titles) [check \(check), play \(view?.playable == true ? "yes" : "no")]"
    }

    private func rebuild(_ s: Snapshot, _ m: MenuModel) {
        signature = m.signature
        syncItem?.submenu = nil  // the submenu is reused: it may hang under one item only
        syncItem = nil
        sharingRow = nil
        menu.removeAllItems()
        rowItems = [:]
        rows = [:]
        models = [:]
        clearHover()
        for r in s.downloading + s.queued + s.recent { rows[r.id] = r }

        if let line = m.offline {
            let item = NSMenuItem(title: "", action: nil, keyEquivalent: "")
            item.attributedTitle = NSAttributedString(string: line, attributes: [.foregroundColor: NSColor.secondaryLabelColor, .font: NSFont.menuFont(ofSize: 0)])
            item.isEnabled = false
            menu.addItem(item)
        }
        for section in m.sections {
            if section.separatorBefore { menu.addItem(.separator()) }
            menu.addItem(.sectionHeader(title: section.title))
            for r in section.rows { menu.addItem(rowItem(r, section.kind)) }
            if let more = section.more {
                let item = ActionItem(title: "") { [commands] in commands.showMain(.downloads, nil) }
                item.attributedTitle = NSAttributedString(string: more, attributes: [.foregroundColor: NSColor.secondaryLabelColor])
                menu.addItem(item)
            }
        }
        if let line = m.sharing {
            if line.separatorBefore { menu.addItem(.separator()) }
            // A click opens Sharing in the window. No submenu (user, 2026-10-14): the line is the summary, with the
            // combined upload rate at its right edge (user, 2026-10-26).
            let item = NSMenuItem()
            let row = MenuCheckRow(content: Self.sharingContent(line), width: 220) { [commands, weak item] in
                MenuSync.closeThen(item?.menu) { commands.showMain(.sharing, nil) }
            }
            item.view = row
            sharingRow = row
            menu.addItem(item)
        }
        if let line = m.folderSync {
            if line.separatorBefore { menu.addItem(.separator()) }
            // The one submenu of this menu (approved 2026-10-25): the folders, then Add Folder….
            let item = NSMenuItem(title: line.title, action: nil, keyEquivalent: "")
            item.image = NSImage(systemSymbolName: "arrow.triangle.2.circlepath", accessibilityDescription: nil)
            item.subtitle = line.subtitle
            item.submenu = menuSync.menu
            syncItem = item
            menu.addItem(item)
        }
        menu.addItem(.separator())
        // Only while an update is waiting (read when the menu is rebuilt; no timer of its own).
        if let title = m.update {
            let item = ActionItem(title: title) { UpdateCenter.shared.install() }
            item.image = NSImage(systemSymbolName: "arrow.down.circle", accessibilityDescription: nil)
            menu.addItem(item)
        }
        // One row: Open on the left, Quit on the right; the shortcuts stay on hidden items.
        let showMain: @MainActor () -> Void = { [commands] in commands.showMain(nil, nil) }
        // The only Quit that ends the process; ⌘Q elsewhere (and the Dock's Quit) just close the windows.
        let quit: @MainActor () -> Void = { (NSApp.delegate as? AppDelegate)?.quitForReal() ?? NSApp.terminate(nil) }
        let footer = NSMenuItem()
        let row = FooterRow(openTitle: m.openTitle, onOpen: showMain, onQuit: quit)
        footer.view = row
        footerRow = row
        menu.addItem(footer)
        for (title, key, run) in [(m.openTitle, "l", showMain), (m.quitTitle, "q", quit)] {
            let item = ActionItem(title: title, key: key, run: run)
            item.isHidden = true
            item.allowsKeyEquivalentWhenHidden = true
            menu.addItem(item)
        }
        update(s, m)
        // New rows under a pointer that has not moved: NSMenu says nothing until it does.
        DispatchQueue.main.async { [weak self] in self?.refreshHover() }
    }

    /// Updates subtitles, marks and checks in place; no structural change, so the highlight stays put.
    private func update(_ s: Snapshot, _ m: MenuModel) {
        for r in s.downloading + s.queued + s.recent { rows[r.id] = r }
        for section in m.sections {
            for r in section.rows {
                let old = models[r.id]
                models[r.id] = r
                guard let item = rowItems[r.id] else { continue }
                (item.view as? MenuTorrentRow)?.running = r.running
                let subtitle = r.subtitle.isEmpty ? nil : r.subtitle
                if item.subtitle != subtitle { item.subtitle = subtitle }
                if old?.mark != r.mark { item.image = Self.image(r.mark) }
            }
        }
        if let line = m.sharing { sharingRow?.content = Self.sharingContent(line) }
        if let item = syncItem, let line = m.folderSync, item.subtitle != line.subtitle { item.subtitle = line.subtitle }
        for item in rowItems.values { (item.view as? MenuTorrentRow)?.sync(from: item) }
        refreshHover()
    }

    static func sharingContent(_ line: MenuSharingLine) -> MenuCheckRow.Content {
        MenuCheckRow.Content(title: line.title, trailing: line.rate, rateActive: line.rateActive)
    }

    static func image(_ mark: MenuMark?) -> NSImage? {
        switch mark {
        case let .ring(fraction): Drawing.ring(fraction)
        case .attention: Drawing.dot(.systemOrange)
        case .done: Drawing.check()
        case nil: nil
        }
    }

    private func rowItem(_ m: MenuRow, _ kind: MenuSectionKind) -> NSMenuItem {
        let id = m.id
        var activate: (@MainActor () -> Void)?
        switch m.click {
        case .openChooser: activate = { [commands] in commands.showChooser(id) }
        case .reveal: activate = { [weak self] in self?.reveal(id) }
        case nil: break
        }
        var onCheck: (@MainActor () -> Void)?
        if m.running != nil { onCheck = { [weak self] in self?.toggleRunning(id) } }
        // No submenu: the commands are on the right-click menu (`handleRightClick`).
        let item = activate.map { ActionItem(title: m.title, run: $0) } ?? NSMenuItem(title: m.title, action: nil, keyEquivalent: "")
        item.image = Self.image(m.mark)
        item.subtitle = m.subtitle.isEmpty ? nil : m.subtitle
        if m.dimmed {
            item.attributedTitle = NSAttributedString(string: m.title, attributes: [.foregroundColor: NSColor.secondaryLabelColor])
        }
        item.representedObject = id
        models[id] = m
        // The row's own view: the file list's chevron, the check (running / paused) and Play, as the model says. A
        // Recent row without a file list keeps the plain item.
        if let r = rows[id] {
            menuFiles.decorate(item, row: r, expands: m.expands, playable: m.playable, interactive: kind != .recent,
                               onActivate: activate, running: m.running, onCheck: onCheck)
        }
        (item.view as? MenuTorrentRow)?.onContext = { [weak self] in
            guard let self, let row = self.rows[id] else { return }
            self.popContext(row)
        }
        rowItems[id] = item
        return item
    }

    private func reveal(_ id: String) {
        if let r = rows[id] { commands.reveal([r]) }
    }

    private func toggleRunning(_ id: String) {
        guard let m = models[id], let running = m.running else { return }
        if m.countdown { commands.pendingCountdown(id, pause: running) }
        else if running { core.pause(ids: [id]) }
        else { core.resume(ids: [id]) }
        // The row follows at once; the next refresh confirms it from the engine.
        (rowItems[id]?.view as? MenuTorrentRow)?.running = !running
        models[id]?.running = !running
    }

    // MARK: Right-click (SPEC §8.2)

    /// Right-click on a row closes the menu and pops its commands (`commands.entries`, the same as the window's
    /// context menu minus the window-only ones) at the pointer. Two routes, as the menu's tracking loop is not
    /// documented to pass right-clicks on: a local monitor, and `MenuTorrentRow.onContext` (its rightMouseUp).
    /// Whichever fires first wins; neither is verified with real hardware input.
    private func installRightClickMonitor() {
        rightClickMonitor = NSEvent.addLocalMonitorForEvents(matching: [.rightMouseDown, .rightMouseUp]) { [weak self] event in
            let isUp = event.type == .rightMouseUp
            let handled = MainActor.assumeIsolated { self?.handleRightClick(isUp: isUp) ?? false }
            return handled ? nil : event
        }
    }

    /// The row under the pointer, else the highlighted one.
    private func rowForContext() -> TransferRow? {
        if let id = rowItems.first(where: { $0.value.view?.menuPointerInside == true })?.key, let row = rows[id] { return row }
        guard let id = menu.highlightedItem?.representedObject as? String else { return nil }
        return rows[id]
    }

    /// Returns true when the click was consumed.
    private func handleRightClick(isUp: Bool) -> Bool {
        log.notice("right-click event during menu tracking (up: \(isUp))")
        guard isUp, let row = rowForContext() else { return false }
        popContext(row)
        return true
    }

    private func popContext(_ row: TransferRow) {
        log.notice("right-click on row \(row.name, privacy: .public)")
        menu.cancelTracking()
        let ctx = MenuBuilder.menu(commands.entries(for: [row], inMenuBar: true))
        DispatchQueue.main.async { ctx.popUp(positioning: nil, at: NSEvent.mouseLocation, in: nil) }
    }
}

private extension NSStatusItem {
    /// The item's own window is pinned to sRGB too: on a wide-gamut display its icon and layers
    /// otherwise cost four ColorSync conversions of about 270 KB each, for ever (see `Windows`).
    @MainActor static func make() -> NSStatusItem {
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        item.button?.window?.colorSpace = Windows.space
        return item
    }
}
