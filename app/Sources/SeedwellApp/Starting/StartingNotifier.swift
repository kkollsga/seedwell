import AppKit
import SwiftUI

extension TransferRow {
    /// Held by the engine: waiting for a choice, or counting down to its start. Neither announces,
    /// connects or has anything to play or list.
    var isHeld: Bool { rowIsHeld(row: self) }
    /// A magnet link that has not fetched its metadata: the engine does not know its files yet (QA's pick of a row).
    var isUnresolvedMagnet: Bool { sizeBytes == 0 }
}

/// What the notifier draws: the core's page (`Starting`), kept for SwiftUI.
@MainActor @Observable final class StartingModel {
    private(set) var page = StartingPage(blocks: [], show: .close, compact: false, closeHelp: nil, tickMs: 1000)

    func set(_ p: StartingPage) { if p != page { page = p } }
}

/// The notifier (SPEC 8.6): a popover that slides out of the menu bar item for as long as a new torrent waits to
/// start. It never takes the focus from the app in front: it is only shown, the app is not activated, and it answers
/// the first click like any popover. What it shows, when it shows and what its buttons do are the core's (`Starting`);
/// this class shows the popover, ticks while it is up and runs the actions.
@MainActor final class StartingNotifier: NSObject, NSPopoverDelegate {
    let model = StartingModel()
    let starting: Starting
    private let commands: Commands
    /// The menu bar item's button, or nil when there is no item (QA without a menu bar).
    private let anchor: () -> NSStatusBarButton?
    private var popover: NSPopover?
    /// A quiet QA run (QuietRun) shows no popover at the menu bar: the same content lives in a window off every screen
    /// for as long as the popover would be up, and QA draws it (`shot … popover`).
    private var quietWindow: NSWindow?
    private var timer: Timer?
    /// QA: keep the popover up although nothing is pending any more, until `qaClose()`.
    private var pinned = false
    private let store = ChoiceStore.shared

    init(commands: Commands, choices: Choices, anchor: @escaping () -> NSStatusBarButton?) {
        self.commands = commands
        self.anchor = anchor
        starting = Starting(core: commands.core, choices: choices)
    }

    var isShown: Bool { popover?.isShown == true || quietWindow != nil }
    var popoverWindow: NSWindow? { quietWindow ?? popover?.contentViewController?.view.window }

    /// Asks the core to look and shows, updates or closes the popover as it says. Cheap; called on every pending
    /// event and every `tickMs` while the popover is up.
    func refresh(launch: Bool = false) {
        let before = store.choices?.ids()
        // The displayed list: a fully hidden tracker is not named in the "starting" popover (SPEC 7E.7).
        let page = starting.refresh(rows: commands.core.snapshot().downloading, nowMs: Date().unixMs, launch: launch)
        model.set(page)
        // The checklists the core loaded or forgot are the chooser window's and the window's list's too.
        if store.choices?.ids() != before { store.changed() }
        switch page.show {
        case let .show(stays):
            show(tickMs: page.tickMs)
            // While someone has to choose, a click elsewhere must not take the popover away.
            popover?.behavior = stays ? .applicationDefined : .transient
        case .quiet: stopTicking()
        case .close: if !pinned { close() }
        }
        qaFit()
    }

    /// The menu bar menu opens on the same spot: the popover steps aside for it and stays put for these torrents.
    func stepAside() {
        guard isShown else { return }
        starting.dismissAll()
        close()
    }

    func close() {
        stopTicking()
        if let w = quietWindow {
            quietWindow = nil
            w.contentViewController = nil
            w.close()
        }
        guard let p = popover else { return }
        p.delegate = nil
        if p.isShown { p.performClose(nil) }
        popover = nil
    }

    /// The file list of a magnet link the user asked to choose files for has arrived: its block opens.
    func consumeChooserRequest(_ id: String) -> Bool { starting.consumeRequest(id: id) }

    // MARK: Actions

    // The buttons reach the engine through the core's `async` calls: each returns at once, and the popover is drawn again
    // when the engine has answered.
    private(set) lazy var actions = StartingActions(
        chooseFiles: { [weak self] id in
            guard let starting = self?.starting else { return }
            CoreCalls.inOrder({ _ = await starting.chooseFiles(id: id) }) { self?.refresh() }
        },
        startNow: { [weak self] id in
            guard let starting = self?.starting else { return }
            CoreCalls.inOrder({ await starting.startNow(id: id) }) { self?.refresh() }
        },
        confirm: { [weak self] id in
            guard let starting = self?.starting else { return }
            CoreCalls.inOrder({ await catching { try await starting.confirm(id: id) } }) { [weak self] r in
                guard let self else { return }
                switch r {
                case .success: self.store.resolved(id)
                case .failure(let error): self.commands.alert("Couldn't start", "\(error)")
                }
                self.refresh()
            }
        },
        cancel: { [weak self] id, keepFile in
            guard let starting = self?.starting else { return }
            // Escape and ⇧Escape can both reach here: the core cancels a torrent once.
            CoreCalls.inOrder({ await starting.cancel(id: id, keepFile: keepFile) }) { [weak self] out in
                guard let self else { return }
                if out.cancelled { self.store.resolved(id, note: out.note) }
                self.refresh()
            }
        },
        open: { [weak self] id in
            guard let starting = self?.starting else { return }
            // An auto-start torrent: the core ends its countdown first and the torrent waits for the choice; the window
            // then opens in selection mode.
            CoreCalls.inOrder({ await starting.open(id: id) }) { [weak self] in
                guard let self else { return }
                self.commands.showTorrent(id)
                self.refresh()
            }
        },
        changeCategory: { [weak self] id, c in
            self?.store.changeCategory(id, to: c)
            self?.refresh()
        },
        closePanel: { [weak self] in self?.stepAside() },
        toggleExpanded: { [weak self] id in
            self?.starting.toggleExpanded(id: id)
            self?.refresh()
        })

    // MARK: Showing

    private func show(tickMs: UInt32) {
        startTicking(tickMs)
        if QuietRun.on { return showQuiet() }
        guard popover == nil || popover?.isShown == false else { return }
        guard let button = anchor(), button.window != nil else { return }
        let p = NSPopover()
        // Closes when the user clicks elsewhere; the countdown goes on and the menu has the same actions.
        // Not while a choice is pending: then it stays until Start, Cancel or its ×.
        p.behavior = model.page.show == .show(stays: true) ? .applicationDefined : .transient
        p.animates = true
        p.appearance = NSApp.appearance
        p.delegate = self
        let host = NSHostingController(rootView: StartingView(model: model, store: store, actions: actions))
        host.sizingOptions = [.preferredContentSize]
        p.contentViewController = host
        popover = p
        p.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        // Memory: pinned to sRGB like every other window (see `Windows`).
        host.view.window?.colorSpace = Windows.space
    }

    /// The popover's content in a borderless window off every screen (a quiet QA run).
    private func showQuiet() {
        guard quietWindow == nil else { return }
        let host = NSHostingController(rootView: StartingView(model: model, store: store, actions: actions))
        host.sizingOptions = [.preferredContentSize]
        let w = NSWindow(contentViewController: host)
        w.styleMask = [.borderless]
        w.isReleasedWhenClosed = false
        w.backgroundColor = .windowBackgroundColor
        w.colorSpace = Windows.space
        w.identifier = NSUserInterfaceItemIdentifier("starting-popover")
        w.orderFront(nil)
        quietWindow = w
        qaFit()
    }

    /// The quiet window takes the size the popover would (the content's preferred size).
    func qaFit() {
        guard let w = quietWindow, let size = w.contentViewController?.preferredContentSize, size.width > 0, size.height > 0,
              w.contentLayoutRect.size != size else { return }
        w.setContentSize(size)
    }

    func popoverDidClose(_ notification: Notification) {
        // Closed by the user (a click elsewhere, Escape): not again for the torrents it showed.
        starting.dismissAll()
        stopTicking()
        popover = nil
    }

    private func startTicking(_ ms: UInt32) {
        guard timer == nil else { return }
        let t = Timer(timeInterval: Double(ms) / 1000, repeats: true) { [weak self] _ in MainActor.assumeIsolated { self?.refresh() } }
        t.tolerance = 0.2
        RunLoop.main.add(t, forMode: .common)
        timer = t
    }

    private func stopTicking() {
        timer?.invalidate()
        timer = nil
    }

    // MARK: QA

    func qaShow() {
        refresh()
        pinned = true
        close()
        show(tickMs: model.page.tickMs)
    }

    /// QA: the chevron in front of the title.
    func chooseFromPopover(_ id: String) {
        guard let b = model.page.blocks.first(where: { $0.id == id }) else { return }
        if case .choice = b.state { starting.toggleExpanded(id: id); refresh() } else { actions.chooseFiles(id) }
    }

    /// QA stand-ins for clicks in the checklist: `ok`, `cancel`, `all`, `none`, `fold` (opens or closes the block),
    /// `open` (every group), `toggle <part of a row's title>`.
    func qaChoice(_ verb: String, id: String, arg: String) {
        let c = store.loaded(id)
        switch verb {
        case "ok": actions.confirm(id)
        case "cancel": actions.cancel(id, arg.hasSuffix("keep"))
        case "title": actions.open(id)
        case "category": categoryChoices(current: .other).first { $0.title.caseInsensitiveCompare(arg) == .orderedSame }.map { actions.changeCategory(id, $0.category) }
        case "all": store.edit { c?.selectAll(on: true) }
        case "none": store.edit { c?.selectAll(on: false) }
        case "fold": starting.toggleExpanded(id: id)
        case "open": store.edit { c?.openAll() }
        case "toggle": if let c, let row = c.findRow(text: arg) { store.edit { c.toggle(id: row) } }
        default: break
        }
        refresh()
    }

    /// QA: the category pill's menu of a block, as a native menu (a SwiftUI menu cannot be opened from code).
    func qaPillMenu(_ id: String) -> NSMenu? {
        guard let pill = model.page.blocks.first(where: { $0.id == id })?.category else { return nil }
        return MenuBuilder.menu(CategoryPill.entries(pill) { [weak self] in self?.actions.changeCategory(id, $0) })
    }

    func qaClose() {
        pinned = false
        close()
    }

    var qaSummary: String {
        let items = model.page.blocks.map { b -> String in
            var t = "\(b.title) | \(b.detail) | \(b.line)"
            if let keep = b.actions.keepFile { t += " | cancel caption: \(ShiftKey.down ? keep.caption : "-")" }
            if case .choice = b.state, let page = store.page(b.id) { t += " | picked \(page.selectedLine)" }
            return t
        }.joined(separator: " ;; ")
        return "starting: shown \(isShown), items \(model.page.blocks.count) [\(items)]"
    }
}
