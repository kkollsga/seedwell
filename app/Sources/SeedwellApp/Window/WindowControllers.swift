import AppKit
import SwiftUI
import Darwin
import os

/// Owns the main window. The window and its view model exist only while it is open; closing
/// tears down the hosting controller, the model and its timer (SPEC §2, §8.3).
@MainActor final class MainWindowController: NSObject, NSWindowDelegate {
    private let commands: Commands
    private let settings: SettingsModel
    let nav = SettingsNavigation()
    private var window: NSWindow?
    private var model: LibraryModel?
    private var observers: [NSObjectProtocol] = []
    private var keyMonitor: Any?

    private let defaultSize: CGSize?
    /// A new download that arrived while the window was closed: the next opening selects it (SPEC 8.3).
    private var arrivalWhileClosed = ClosedWindowArrival()
    /// Called after the window opened or closed (the Dock icon follows it).
    var onOpenChange: () -> Void = {}
    /// Called when the window closes (Settings are saved then).
    var onClose: () -> Void = {}

    init(commands: Commands, settings: SettingsModel, defaultSize: CGSize? = nil) {
        self.commands = commands
        self.settings = settings
        self.defaultSize = defaultSize
        super.init()
        nav.openTracker = { [weak self] id in self?.commands.showMain(.tracker(id), nil) }
    }

    var isOpen: Bool { window != nil }
    var libraryModel: LibraryModel? { model }
    var nsWindow: NSWindow? { window }

    /// Opens the window, on `settings` (a tab of Settings) when given: Settings is a section of this window.
    func show(section: LibraryModel.Section? = nil, info id: String? = nil,
              settings tab: SettingsTab? = nil, editTracker: String? = nil, newTrackerHost: String? = nil, openSettings: Bool = false) {
        let opening = self.model == nil
        let model = self.model ?? makeWindow()
        if openSettings || tab != nil || editTracker != nil || newTrackerHost != nil {
            if let tab { nav.tab = tab }
            if let editTracker { nav.trackerToEdit = editTracker }
            if let newTrackerHost { nav.newTrackerHost = newTrackerHost }
            self.settings.reload()
            model.select(.settings)
        } else if let id {
            model.reveal(id: id)
            // The popover anchors to the row, which exists one layout pass later.
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { model.infoID = id }
        } else if let section, section != model.section {
            model.select(section)
        }
        // Opened plainly (Open Seedwell…, the Dock): on the download that arrived meanwhile, if it still downloads.
        let snap = commands.core.snapshot()
        let active = Set((snap.downloading + snap.queued).map(\.id))
        let waiting = arrivalWhileClosed.take { active.contains($0) }
        if opening, section == nil, id == nil, tab == nil, !openSettings, editTracker == nil, newTrackerHost == nil, let waiting {
            model.showNewDownload(waiting)
        }
        onOpenChange()
        NSApp.activate()
        window?.makeKeyAndOrderFront(nil)
        // Nothing takes the first-responder ring the moment the window is key: it opens calm (SPEC 8.3).
        DispatchQueue.main.async { [window] in window?.makeFirstResponder(nil) }
    }

    func closeForQuit() { window?.close() }

    /// A click on a title in the notifier: Downloads, this torrent selected and scrolled into view, its files open (selection mode
    /// for a torrent that waits for a choice). The torrent stays held as it is.
    func showTorrent(_ id: String) {
        show(section: .downloads)
        guard let model else { return }
        if model.section == .settings { model.select(.downloads) }
        model.showNewDownload(id)
        FilesStore.shared.expand(id)
    }

    /// A torrent became a download (the watcher's `onArrival`, SPEC 8.3): the open window switches to
    /// Downloads and selects it; a closed one remembers it for its next opening.
    func newDownload(_ id: String) {
        if let model { model.showNewDownload(id) } else { arrivalWhileClosed.remember(id) }
    }

    private func makeWindow() -> LibraryModel {
        let model = LibraryModel(core: commands.core)
        model.isBusy = { [commands] in
            commands.favorites.target != nil || commands.favorites.removal != nil || commands.removal.pending != nil
        }
        model.onSnapshot = { [commands] in commands.arrivals.note($0) }
        let host = NSHostingController(rootView: MainWindowView(model: model, commands: commands, settings: settings, nav: nav))
        let w = NSWindow(contentViewController: host)
        w.styleMask = [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView]
        w.title = AppInfo.name
        w.isReleasedWhenClosed = false
        w.colorSpace = Windows.space  // memory: see `Windows`
        w.titleVisibility = .hidden
        w.titlebarAppearsTransparent = true
        w.toolbarStyle = .unified
        w.setContentSize(defaultSize ?? NSSize(width: 1100, height: 720))
        if defaultSize == nil {
            w.setFrameAutosaveName("MainWindow")
            if !w.setFrameUsingName("MainWindow") { w.center() }
        } else {
            w.center()
        }
        w.delegate = self
        let nc = NotificationCenter.default
        observers = [
            nc.addObserver(forName: NSWindow.didChangeOcclusionStateNotification, object: w, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.updatePolling() }
            },
            nc.addObserver(forName: NSWindow.didMiniaturizeNotification, object: w, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.model?.stop() }
            },
        ]
        window = w
        self.model = model
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            MainActor.assumeIsolated { self?.handleKey(event) ?? false } ? nil : event
        }
        model.start()
        return model
    }

    /// Polls only while the window can actually be seen.
    private func updatePolling() {
        if window?.occlusionState.contains(.visible) == true { model?.start() } else { model?.stop() }
    }

    /// Delete asks to remove the selected torrents, ⇧Delete to remove them and trash their files; while
    /// that question is open, Return answers it and Escape drops it. Window-wide, so it works without the
    /// list holding focus, but never while typing in a field, in Settings or on selected files.
    private func handleKey(_ event: NSEvent) -> Bool {
        guard let w = window, event.window === w, w.isKeyWindow, let model, model.section != .settings,
              !(w.firstResponder is NSText) else { return false }
        let mods = event.modifierFlags.intersection([.command, .option, .control, .shift])
        let removal = commands.removal
        switch event.keyCode {
        case 51, 117:  // delete, forward delete
            guard removal.pending == nil, mods.subtracting(.shift).isEmpty, FilesStore.shared.selectedNodes.isEmpty else { return false }
            let rows = model.rows(for: model.selection)
            guard !rows.isEmpty else { return false }
            commands.confirmRemove(rows, deleteFiles: mods.contains(.shift))
            return true
        case 36, 76:  // return, enter
            guard removal.pending != nil, mods.isEmpty else { return false }
            // Only while its rows are on screen: never remove what the user cannot see.
            guard removal.isAnswerable(visible: Set(model.visibleRows.map(\.id))) else { removal.cancel(); return false }
            commands.confirmedRemove()
            model.selection = []
            return true
        case 53:  // escape
            guard removal.pending != nil else { return false }
            removal.cancel()
            return true
        default:
            return false
        }
    }

    func windowWillClose(_ notification: Notification) {
        if let keyMonitor { NSEvent.removeMonitor(keyMonitor) }
        keyMonitor = nil
        commands.removal.cancel()
        onClose()
        model?.stop()
        observers.forEach(NotificationCenter.default.removeObserver)
        observers = []
        window?.delegate = nil
        window?.contentViewController = nil
        window = nil
        model = nil
        MemoryRelief.schedule()
        onOpenChange()
    }
}

/// Closing the main window frees its memory (SPEC §2): the view tree is gone, but the allocator keeps
/// the freed pages. Asking it to give them back is what brings `phys_footprint` down.
@MainActor enum MemoryRelief {
    private static var pending = false

    static func schedule() {
        guard !pending else { return }
        pending = true
        for delay in [0.6, 3.0] {
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) {
                MainActor.assumeIsolated {
                    FileIcons.reset()
                    _ = malloc_zone_pressure_relief(nil, 0)
                    if delay > 1 { pending = false }
                }
            }
        }
    }
}
