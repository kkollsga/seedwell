import AppKit
import SwiftUI

/// The notifier (SPEC 8.6): a popover that slides out of the menu bar item for as long as a new
/// torrent waits to start. It never takes the focus from the app in front: it is only shown, the app
/// is not activated, and it answers the first click like any popover.
@MainActor final class StartingNotifier: NSObject, NSPopoverDelegate {
    let model = StartingModel()
    private let commands: Commands
    /// The menu bar item's button, or nil when there is no item (QA without a menu bar).
    private let anchor: () -> NSStatusBarButton?
    private var popover: NSPopover?
    private var timer: Timer?
    /// Torrents whose "Choose Files…" is waiting for their metadata (magnet links): the chooser opens
    /// by itself when it arrives, without the "Choose what to download" notification.
    private(set) var chooserRequests: Set<String> = []
    /// QA: keep the popover up although nothing is pending any more, until `qaClose()`.
    private var pinned = false
    private let store = ChoiceStore.shared

    init(commands: Commands, anchor: @escaping () -> NSStatusBarButton?) {
        self.commands = commands
        self.anchor = anchor
    }

    var isShown: Bool { popover?.isShown == true }
    var popoverWindow: NSWindow? { popover?.contentViewController?.view.window }

    /// Looks at the core and shows, updates or closes the popover. Cheap; called on every pending event
    /// and once a second while the popover is up.
    func refresh(launch: Bool = false) {
        // The displayed list: a fully hidden tracker is not named in the "starting" popover (SPEC 7E.7).
        let rows = commands.core.snapshot().downloading
        model.sync(rows, requested: chooserRequests, hasSource: { [store] in store.hasSource($0) },
                   // The chooser's own destination (it follows a category change at once), when the plan says one.
                   destination: { [store] id in store.models[id].map(\.destination).flatMap { $0.hasPrefix("the ") ? nil : $0 } })
        if launch { model.dismissWaiting() }
        // The checklists of torrents that wait for a choice: loaded once, shared with the main-window chooser.
        let waiting = Set(model.items.filter(\.awaiting).map(\.id))
        store.prune(keeping: waiting)
        for id in waiting { _ = store.model(for: id) }
        if model.wantsToShow {
            show()
            // While someone has to choose, a click elsewhere must not take the popover away.
            popover?.behavior = model.hasChoice ? .applicationDefined : .transient
        } else if !model.items.isEmpty {
            stopTicking()
        } else if !pinned {
            close()
        }
    }

    /// The menu bar menu opens on the same spot: the popover steps aside for it and stays put for these torrents.
    func stepAside() {
        guard isShown else { return }
        model.dismissAll()
        close()
    }

    func close() {
        stopTicking()
        guard let p = popover else { return }
        p.delegate = nil
        if p.isShown { p.performClose(nil) }
        popover = nil
    }

    /// The file list of a magnet link the user asked to select files for has arrived: its block opens.
    func consumeChooserRequest(_ id: String) -> Bool {
        let asked = chooserRequests.remove(id) != nil
        if asked { model.expand(id) }
        return asked
    }

    // MARK: Actions

    private lazy var actions = StartingActions(
        chooseFiles: { [weak self] id in self?.chooseFiles(id) },
        togglePause: { [weak self] id, paused in
            self?.commands.pendingCountdown(id, pause: !paused)
            self?.refresh()
        },
        startNow: { [weak self] id in
            self?.commands.pendingStartNow(id)
            self?.refresh()
        },
        confirm: { [weak self] id in
            guard let self else { return }
            model.collapse(id)
            do { try store.confirm(id) } catch { commands.alert("Couldn't start", "\(error)") }
            refresh()
        },
        cancel: { [weak self] id, keepFile in
            guard let self else { return }
            // Escape and ⇧Escape can both reach here: the torrent is only cancelled once.
            guard model.items.contains(where: { $0.id == id }) else { return }
            // Fetching a magnet link's list: not waiting for a choice yet, so the engine has nothing to cancel.
            chooserRequests.remove(id)
            model.collapse(id)
            do { try store.cancel(id, keepFile: keepFile) } catch { commands.core.remove(ids: [id], deleteFiles: false) }
            refresh()
        },
        open: { [weak self] id in
            guard let self else { return }
            // An auto-start torrent: the click ends its countdown first (as the chevron does) and the torrent waits for
            // the choice; the window then opens in selection mode. The block here switches to awaiting by itself.
            if let item = model.items.first(where: { $0.id == id }), !item.choosing {
                if commands.pendingChoose(id, openWindow: false) == .afterMetadata { chooserRequests.insert(id) }
            }
            commands.showTorrent(id)
            refresh()
        },
        changeCategory: { [weak self] id, c in
            self?.store.changeCategory(id, to: c)
            self?.refresh()
        },
        closePanel: { [weak self] in self?.stepAside() })

    /// "Select Files…": the countdown ends, the torrent stays held and this block's checklist opens right
    /// here (never the main window or the chooser window); every other block goes compact.
    private func chooseFiles(_ id: String) {
        switch commands.pendingChoose(id, openWindow: false) {
        case .open: model.expand(id)
        case .afterMetadata: chooserRequests.insert(id); model.expand(id)
        case .failed: break
        }
        refresh()
    }

    // MARK: Showing

    private func show() {
        startTicking()
        guard popover == nil || popover?.isShown == false else { return }
        guard let button = anchor(), button.window != nil else { return }
        let p = NSPopover()
        // Closes when the user clicks elsewhere; the countdown goes on and the menu has the same actions.
        // Not while a choice is pending: then it stays until OK, Cancel or its ×.
        p.behavior = model.hasChoice ? .applicationDefined : .transient
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

    func popoverDidClose(_ notification: Notification) {
        // Closed by the user (a click elsewhere, Escape): not again for the torrents it showed.
        model.dismissAll()
        stopTicking()
        popover = nil
    }

    private func startTicking() {
        guard timer == nil else { return }
        let t = Timer(timeInterval: 1, repeats: true) { [weak self] _ in MainActor.assumeIsolated { self?.refresh() } }
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
        model.sync(commands.core.snapshot().downloading)
        pinned = true
        close()
        show()
    }

    /// QA: the chevron in front of the title.
    func chooseFromPopover(_ id: String) {
        if model.items.first(where: { $0.id == id })?.awaiting == true { model.toggleExpanded(id); refresh() } else { chooseFiles(id) }
    }

    /// QA stand-ins for clicks in the checklist: `ok`, `cancel`, `all`, `none`, `fold` (opens or closes the block),
    /// `open` (every group), `toggle <part of a row's title>`.
    func qaChoice(_ verb: String, id: String, arg: String) {
        let m = store.models[id]
        switch verb {
        case "ok": actions.confirm(id)
        case "cancel": actions.cancel(id, arg.hasSuffix("keep"))
        case "title": actions.open(id)
        case "category": Category.all.first { $0.label.caseInsensitiveCompare(arg) == .orderedSame }.map { actions.changeCategory(id, $0) }
        case "all": m?.selectAll()
        case "none": m?.selectNone()
        case "fold": model.toggleExpanded(id)
        case "open": m.map { $0.expanded = Set($0.items.filter { !$0.children.isEmpty }.map(\.id)) }
        case "toggle":
            if let m, let hit = (m.items + m.items.flatMap(\.children)).first(where: { $0.title.localizedCaseInsensitiveContains(arg) }) { m.toggle(hit) }
        default: break
        }
        refresh()
    }

    func qaClose() {
        pinned = false
        close()
    }

    var qaSummary: String {
        let now = Date()
        let items = model.items.map { i -> String in
            var t = "\(i.title) | \(i.detail) | \(i.line(at: now))"
            if i.hasSource { t += " | cancel caption: \(CancelText.caption(shift: ShiftKey.down, hasSourceFile: true) ?? "-")" }
            if let m = store.models[i.id], i.awaiting { t += " | picked \(Fmt.bytes(m.selectedBytes)) of \(Fmt.bytes(m.totalBytes))" }
            return t
        }.joined(separator: " ;; ")
        return "starting: shown \(isShown), items \(model.items.count) [\(items)]"
    }
}
