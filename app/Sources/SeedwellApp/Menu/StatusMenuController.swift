import AppKit
import os

/// The menu bar item and its plain native NSMenu (SPEC §8.2). The menu is rebuilt when it opens
/// and refreshed once a second only while it is open; when closed nothing polls the core.
@MainActor final class StatusMenuController: NSObject, NSMenuDelegate {
    private let core: Core
    private let commands: Commands
    private let settings: SettingsModel?
    private var statusItem: NSStatusItem? = NSStatusItem.make()
    /// What the icon shows now; it is redrawn only when this changes.
    private var iconLook: IconLook?
    /// Called with every snapshot the icon refresh reads (the sleep assertion follows it).
    var onRefresh: ((Snapshot) -> Void)?
    /// The menu is about to open: the notifier steps aside for it.
    var onWillOpen: (() -> Void)?
    /// The button of the menu bar item: the notifier anchors to it.
    var statusButton: NSStatusBarButton? { statusItem?.button }
    private let menu = NSMenu(title: AppInfo.name)
    private var timer: Timer?
    private var iconTimer: Timer?
    private var moveTimer: Timer?
    private var moveBar = MoveBar()
    private var attentionTimer: Timer?
    private var rightClickMonitor: Any?
    private var signature = ""
    private var rowItems: [String: NSMenuItem] = [:]
    private var rows: [String: TransferRow] = [:]
    private var sharingItem: NSMenuItem?
    /// The inline file lists of the torrent rows (lazy: nothing is fetched until one opens).
    private(set) lazy var menuFiles = MenuFiles(core: core, commands: commands)
    /// The Open / Quit row (QA draws its hover state).
    private(set) var footerRow: FooterRow?
    private let log = Logger(subsystem: "io.github.kkollsga.Seedwell", category: "menu")
    private static let queuedShown = 3
    private static let recentShown = 3
    /// Longest name a downloading row shows before the middle is cut.
    static let titleLimit = 44

    init(core: Core, commands: Commands, settings: SettingsModel? = nil) {
        self.core = core
        self.commands = commands
        self.settings = settings
        super.init()
        menu.delegate = self
        menu.autoenablesItems = false
        statusItem?.menu = menu
        menuFiles.onMoved = { [weak self] in self?.refreshHover() }
        refreshIcon()
        // Errors, dead torrents and uploads start without an event: look again every 30 s (cheap, coalesced).
        let slow = Timer(timeInterval: 30, repeats: true) { [weak self] _ in MainActor.assumeIsolated { self?.refreshIcon() } }
        slow.tolerance = 15
        RunLoop.main.add(slow, forMode: .common)
        attentionTimer = slow
    }

    /// QA: opens or closes the menu as a click on the icon would.
    /// QA: presses one side of the footer row as a click would.
    func qaPressFooter(_ side: String) { footerRow?.qaPress(side) }

    func perform(open: Bool) {
        if open { statusItem?.button?.performClick(nil) } else { menu.cancelTracking() }
    }

    /// Quit hides everything at once (SPEC §12 #16): the icon goes while the engine finishes its announces.
    func hide() {
        timer?.invalidate(); iconTimer?.invalidate(); attentionTimer?.invalidate(); moveTimer?.invalidate()
        timer = nil; iconTimer = nil; attentionTimer = nil; moveTimer = nil
        if let item = statusItem { NSStatusBar.system.removeStatusItem(item) }
        statusItem = nil
    }

    // MARK: Icon (event-driven; a slow timer runs only while something is downloading)

    func refreshIcon() {
        guard let statusItem else { return }
        let s = core.snapshot()
        onRefresh?(s)
        let active = s.downloading.filter { !$0.isHeld }
        let total = active.reduce(0) { $0 + $1.progressSize }
        let done = active.reduce(0) { $0 + $1.progressDone }
        let downloading = !active.isEmpty
        let look = IconLook(down: Drawing.intensity(s.downRate), up: Drawing.intensity(s.upRate),
                            progress: downloading ? (Double(done) / Double(max(total, 1)) * 50).rounded() / 50 : nil,
                            moving: moveBar.fraction(percent: s.movingPercent, now: Date()),
                            attention: s.attention)
        if look != iconLook {
            iconLook = look
            statusItem.button?.image = Drawing.statusIcon(down: look.down, up: look.up, progress: look.progress, moving: look.moving, attention: look.attention)
        }
        // Move progress is not an event: look twice a second while a move runs (and for the grace
        // period after it starts), then stop.
        if moveBar.isWatching, moveTimer == nil {
            let t = Timer(timeInterval: 0.5, repeats: true) { [weak self] _ in MainActor.assumeIsolated { self?.refreshIcon() } }
            t.tolerance = 0.1
            RunLoop.main.add(t, forMode: .common)
            moveTimer = t
        } else if !moveBar.isWatching {
            moveTimer?.invalidate()
            moveTimer = nil
        }
        var label = AppInfo.name
        if s.downRate > 0 { label += ", downloading \(Fmt.rate(s.downRate))" } else if downloading { label += ", downloading" }
        if s.upRate > 0 { label += ", uploading \(Fmt.rate(s.upRate))" }
        if s.attention { label += ", needs attention" }
        statusItem.button?.setAccessibilityLabel(label)
        // Colours follow the rates: a 5 s refresh while something moves or an arrow is still
        // coloured (a finished download's rate fades over a few seconds after its last block).
        let moving = downloading || s.upRate > 0 || s.downRate > 0 || look.down > 0 || look.up > 0
        if moving, iconTimer == nil {
            let t = Timer(timeInterval: 5, repeats: true) { [weak self] _ in MainActor.assumeIsolated { self?.refreshIcon() } }
            t.tolerance = 2
            RunLoop.main.add(t, forMode: .common)
            iconTimer = t
        } else if !moving {
            iconTimer?.invalidate()
            iconTimer = nil
        }
    }

    // MARK: NSMenuDelegate

    func menuWillOpen(_ menu: NSMenu) {
        guard menu === self.menu else { return }
        onWillOpen?()
        qaMenuOpen = true
        pinToSRGB()
        rebuild(core.snapshot())
        let t = Timer(timeInterval: 1, repeats: true) { [weak self] _ in MainActor.assumeIsolated { self?.tick() } }
        t.tolerance = 0.1
        // eventTracking mode: timers in the default mode do not fire while a menu is tracking.
        RunLoop.main.add(t, forMode: .common)
        timer = t
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
        commands.arrivals.note(s)
        // An open file list is not rebuilt under the pointer (ticking a file can move the row).
        if signatureOf(s) != signature, !menuFiles.isOpen { rebuild(s) } else { update(s) }
    }

    /// The Downloading section and the Queued section as the menu shows them (SPEC 8.2). A torrent that
    /// just arrived is listed first in Downloading, newest first, even while it waits for a free slot: the
    /// user must see what they just added without looking under Queued. Held torrents (waiting for the
    /// user's choice, then counting down) stay ahead of it, as the engine orders them.
    struct Layout {
        var downloading: [TransferRow]
        var queued: [TransferRow]
    }

    /// At most this many queued arrivals are lifted into Downloading.
    static let arrivedQueuedShown = 3

    static func layout(_ s: Snapshot, arrivals: DownloadArrivals) -> Layout {
        let lifted = Array(s.queued.filter { arrivals.arrived.contains($0.id) && $0.state == .queued }
            .sorted { arrivals.rank(of: $0.id) > arrivals.rank(of: $1.id) }.prefix(arrivedQueuedShown))
        let liftedIDs = Set(lifted.map(\.id))
        let all = s.downloading + lifted
        func tier(_ r: TransferRow) -> Int { r.awaitingChoice ? 0 : r.pending != nil ? 1 : 2 }
        // Stable: the engine's order inside a tier, newest arrival first.
        let sorted = all.enumerated().sorted { a, b in
            let (ta, tb) = (tier(a.element), tier(b.element))
            if ta != tb { return ta < tb }
            let (ra, rb) = (arrivals.rank(of: a.element.id), arrivals.rank(of: b.element.id))
            if ta == 2, ra != rb { return ra > rb }
            return a.offset < b.offset
        }.map(\.element)
        return Layout(downloading: sorted, queued: s.queued.filter { !liftedIDs.contains($0.id) })
    }

    /// QA: the titles of the open menu, headers first ("Downloading|name|name||Recent|…").
    var qaTitles: String {
        menu.items.filter { !$0.isHidden && ($0.view == nil || $0.view is MenuTorrentRow) }.map { $0.isSeparatorItem ? "-" : $0.attributedTitle?.string ?? $0.title }.joined(separator: "|")
    }

    /// QA: the menu items (top level).
    var qaItems: [NSMenuItem] { menu.items }
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

    private func signatureOf(_ s: Snapshot) -> String {
        let l = Self.layout(s, arrivals: commands.arrivals.arrivals)
        let d = l.downloading.map { "\($0.id)\($0.state)\($0.pending != nil)\($0.pending?.paused ?? false)" }
        let q = l.queued.prefix(Self.queuedShown).map(\.id) + ["\(l.queued.count)"]
        let r = s.recent.prefix(Self.recentShown).map(\.id)
        let a = ["\(s.sharingTotal > 0)", "\(s.errorCount)", "\(s.online)"]
        let c = s.downloading.filter(\.awaitingChoice).map(\.id)
        return [d, q, r, a, c].map { $0.joined(separator: ",") }.joined(separator: "|")
    }

    private func rebuild(_ s: Snapshot) {
        commands.arrivals.note(s)
        signature = signatureOf(s)
        let layout = Self.layout(s, arrivals: commands.arrivals.arrivals)
        menu.removeAllItems()
        rowItems = [:]
        rows = [:]
        clearHover()
        for r in s.downloading + s.queued + s.recent { rows[r.id] = r }

        if !s.online, s.sharingTotal == 0 {
            // The Sharing line carries it when there is one; otherwise a single quiet line.
            let item = NSMenuItem(title: "", action: nil, keyEquivalent: "")
            item.attributedTitle = NSAttributedString(string: "Offline", attributes: [.foregroundColor: NSColor.secondaryLabelColor, .font: NSFont.menuFont(ofSize: 0)])
            item.isEnabled = false
            menu.addItem(item)
        }

        if !layout.downloading.isEmpty {
            menu.addItem(.sectionHeader(title: "Downloading"))
            for r in layout.downloading { menu.addItem(rowItem(r, .active)) }
        }
        if !layout.queued.isEmpty {
            menu.addItem(.sectionHeader(title: "Queued"))
            for r in layout.queued.prefix(Self.queuedShown) { menu.addItem(rowItem(r, .queued)) }
            let more = layout.queued.count - Self.queuedShown
            if more > 0 {
                let item = ActionItem(title: "") { [commands] in commands.showMain(.downloads, nil) }
                item.attributedTitle = NSAttributedString(string: "+ \(more) more", attributes: [.foregroundColor: NSColor.secondaryLabelColor])
                menu.addItem(item)
            }
        }
        if !s.recent.isEmpty {
            if !menu.items.isEmpty { menu.addItem(.separator()) }
            menu.addItem(.sectionHeader(title: "Recent"))
            for r in s.recent.prefix(Self.recentShown) { menu.addItem(rowItem(r, .recent)) }
        }
        if s.sharingTotal > 0 {
            if !menu.items.isEmpty { menu.addItem(.separator()) }
            // A click opens Sharing in the window. No submenu (user, 2026-10-14): the line is the summary.
            let item = ActionItem(title: "Sharing") { [commands] in commands.showMain(.sharing, nil) }
            item.image = NSImage(systemSymbolName: "arrow.up", accessibilityDescription: nil)
            sharingItem = item
            menu.addItem(item)
            update(s)
        }
        menu.addItem(.separator())
        // Only while an update is waiting (read when the menu is rebuilt; no timer of its own).
        if let v = UpdateCenter.shared.availableVersion {
            let item = ActionItem(title: "Update to \(AppInfo.name) \(v)") { UpdateCenter.shared.install() }
            item.image = NSImage(systemSymbolName: "arrow.down.circle", accessibilityDescription: nil)
            menu.addItem(item)
        }
        // One row: Open on the left, Quit on the right; the shortcuts stay on hidden items.
        let showMain: @MainActor () -> Void = { [commands] in commands.showMain(nil, nil) }
        // The only Quit that ends the process; ⌘Q elsewhere (and the Dock's Quit) just close the windows.
        let quit: @MainActor () -> Void = { (NSApp.delegate as? AppDelegate)?.quitForReal() ?? NSApp.terminate(nil) }
        let footer = NSMenuItem()
        let row = FooterRow(openTitle: "Open \(AppInfo.name)…", onOpen: showMain, onQuit: quit)
        footer.view = row
        footerRow = row
        menu.addItem(footer)
        for (title, key, run) in [("Open \(AppInfo.name)…", "l", showMain), ("Quit", "q", quit)] {
            let item = ActionItem(title: title, key: key, run: run)
            item.isHidden = true
            item.allowsKeyEquivalentWhenHidden = true
            menu.addItem(item)
        }
        update(s)
        // New rows under a pointer that has not moved: NSMenu says nothing until it does.
        DispatchQueue.main.async { [weak self] in self?.refreshHover() }
    }

    /// Updates titles, subtitles and rings in place; no structural change, so the highlight stays put.
    private func update(_ s: Snapshot) {
        for r in s.downloading + s.queued + s.recent { rows[r.id] = r }
        for r in s.queued { (rowItems[r.id]?.view as? MenuTorrentRow)?.running = Self.isRunning(r) }
        for r in Self.layout(s, arrivals: commands.arrivals.arrivals).downloading where !r.awaitingChoice {
            guard let item = rowItems[r.id] else { continue }
            (item.view as? MenuTorrentRow)?.running = Self.isRunning(r)
            if let p = r.pending {
                item.subtitle = StartingText.line(remaining: p.remaining(at: Date()), paused: p.paused)
                continue
            }
            item.subtitle = r.menuSubtitle
            item.image = r.state == .queued ? Drawing.ring(0) : Drawing.ring(r.fraction)
        }
        for r in s.recent {
            rowItems[r.id]?.subtitle = Self.recentSubtitle(r)
        }
        if let item = sharingItem { item.attributedTitle = Self.sharingTitle(s) }
        for item in rowItems.values { (item.view as? MenuTorrentRow)?.sync(from: item) }
        refreshHover()
    }

    /// "Sharing 2 torrents · 14 KB/s" (the rate only while uploading), plus "· 1 error" in the secondary colour (only problems that need the user count). No badge.
    private static func sharingTitle(_ s: Snapshot) -> NSAttributedString {
        var text = "Sharing \(Fmt.count(s.sharingTotal, "torrent"))"
        if s.online, s.upRate > 0 { text += " · \(Fmt.rate(s.upRate))" }
        let t = NSMutableAttributedString(string: text, attributes: [.font: NSFont.menuFont(ofSize: 0)])
        if !s.online {
            t.append(NSAttributedString(string: " · Offline", attributes: [.foregroundColor: NSColor.secondaryLabelColor, .font: NSFont.menuFont(ofSize: 0)]))
        }
        if s.errorCount > 0 {
            t.append(NSAttributedString(string: " · \(s.errorCount) error\(s.errorCount == 1 ? "" : "s")",
                                        attributes: [.foregroundColor: NSColor.secondaryLabelColor, .font: NSFont.menuFont(ofSize: 0)]))
        }
        return t
    }

    private static func recentSubtitle(_ r: TransferRow) -> String {
        [r.category.label, r.finishedAt.map { Fmt.ago($0, short: true) }].compactMap { $0 }.joined(separator: " · ")
    }

    private enum Kind { case active, queued, recent }

    private func rowItem(_ r: TransferRow, _ kind: Kind) -> NSMenuItem {
        let item: NSMenuItem
        if r.awaitingChoice {
            // Waits for the user: orange dot, and a click opens the chooser (SPEC §7A.6).
            item = ActionItem(title: r.displayName) { [commands] in commands.showChooser(r.id) }
            item.image = Drawing.dot(.systemOrange)
            item.subtitle = "Choose what to download…"
            menuFiles.decorate(item, row: r, revealOnClick: false, interactive: true, onActivate: { [commands] in commands.showChooser(r.id) })
        } else if kind == .recent {
            // Recent: a plain row; a click reveals it in Finder.
            item = ActionItem(title: r.displayName) { [commands] in commands.reveal([r]) }
            item.image = Drawing.check()
            item.subtitle = Self.recentSubtitle(r)
        } else {
            // The name gives way, never the numbers under it (SPEC 8.2).
            item = NSMenuItem(title: kind == .active ? Fmt.middleTruncated(r.displayName, to: Self.titleLimit) : r.displayName, action: nil, keyEquivalent: "")
            // No submenu: the commands are on the right-click menu (`handleRightClick`).
            if r.pending != nil {
                item.image = Drawing.ring(0)
            } else if r.state == .queued {
                item.image = Drawing.ring(0)
            } else if r.state == .downloading || r.state == .paused && r.progressDone > 0 {
                item.image = Drawing.ring(r.fraction)
            }
        }
        if kind == .queued {
            item.attributedTitle = NSAttributedString(string: r.displayName, attributes: [.foregroundColor: NSColor.secondaryLabelColor])
        }
        item.representedObject = r.id
        // A multi-file torrent gets a chevron that opens its file list under the row, a download a check
        // (running / paused) and a video a Play icon (SPEC 8.2).
        if !r.awaitingChoice {
            let live = kind != .recent
            menuFiles.decorate(item, row: r, revealOnClick: kind == .recent, interactive: live,
                               running: live ? Self.isRunning(r) : nil, onCheck: { [weak self] in self?.toggleRunning(r.id) })
        }
        (item.view as? MenuTorrentRow)?.onContext = { [weak self] in
            guard let self, let row = self.rows[r.id] else { return }
            self.popContext(row)
        }
        rowItems[r.id] = item
        return item
    }

    /// The check of a row: running = downloading or queued, off = paused. A held row's countdown pauses too.
    static func isRunning(_ r: TransferRow) -> Bool {
        if let p = r.pending { return !p.paused }
        return r.state != .paused
    }

    private func toggleRunning(_ id: String) {
        guard let r = rows[id] else { return }
        let running = Self.isRunning(r)
        if r.pending != nil { commands.pendingCountdown(id, pause: running) }
        else if running { core.pause(ids: [id]) }
        else { core.resume(ids: [id]) }
        // The row follows at once; the next refresh confirms it from the engine.
        (rowItems[id]?.view as? MenuTorrentRow)?.running = !running
        if var next = rows[id] {
            if next.pending != nil { next.pending?.paused = running } else { next.state = running ? .paused : .queued }
            rows[id] = next
        }
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

/// The inputs of the menu bar icon, quantised so that equal looks compare equal.
struct IconLook: Equatable {
    let down: Double
    let up: Double
    let progress: Double?
    /// Aggregate move progress once a move has run long enough to show (`MoveBar`), else nil.
    let moving: Double?
    let attention: Bool
}

/// When the menu bar icon shows the green move bar. A same-volume move is a rename that takes about
/// 30 ms; showing the bar for it would only flicker, so the bar appears for a move that is still
/// running after `grace` seconds and goes as soon as no move is left.
struct MoveBar {
    static let grace: TimeInterval = 0.4
    private var since: Date?

    /// `percent`: the snapshot's aggregate move progress (nil = nothing moves). Returns the fraction
    /// (0...1, in steps of 2 % so equal looks compare equal) to draw, or nil while nothing is shown.
    mutating func fraction(percent: UInt8?, now: Date) -> Double? {
        guard let percent else { since = nil; return nil }
        let start = since ?? now
        since = start
        guard now.timeIntervalSince(start) >= Self.grace else { return nil }
        return (Double(min(percent, 100)) / 100 * 50).rounded() / 50
    }

    /// A move is under way (shown or still within the grace period): the icon is checked again soon.
    var isWatching: Bool { since != nil }
}

extension TransferRow {
    /// The grey line of a downloading row in the menu: "42% · 3.1 MB/s · 4 min left". With no speed (or
    /// no estimate yet) the engine's reason replaces the time ("42% · Waiting for peers"), never a stale one.
    var menuSubtitle: String {
        if state == .paused { return "Paused · \(percent)%" }
        if state == .queued { return "Up next · waiting for a free slot" }
        if state == .downloading {
            if downRate > 0, let eta = etaSecs { return "\(percent)% · \(Fmt.rate(downRate)) · \(Fmt.remaining(eta, short: true))" }
            if downRate == 0, let w = downloadWaiting { return "\(percent)% · \(w.note)\(w.tail)" }
            return "\(percent)% · " + (downRate > 0 ? "\(Fmt.rate(downRate)) · " : "") + Fmt.remaining(nil, short: true)
        }
        return "\(percent)% · \(Fmt.remaining(etaSecs, short: true))"
    }
}
