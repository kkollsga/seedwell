import AppKit
import SwiftUI
import Darwin
import os

/// Owns the main window. The window and its view model exist only while it is open; closing
/// tears down the hosting controller, the model and its timer (SPEC §2, §8.3). The core's `Library` (the window's
/// state) lives with the app: it remembers a new download that arrives while the window is closed.
@MainActor final class MainWindowController: NSObject, NSWindowDelegate {
    private let commands: Commands
    private let settings: SettingsModel
    let nav = SettingsNavigation()
    private var window: NSWindow?
    private var model: LibraryModel?
    private var observers: [NSObjectProtocol] = []
    private var keyMonitor: Any?

    private let defaultSize: CGSize?
    private let library: Library
    /// Called after the window opened or closed (the Dock icon follows it).
    var onOpenChange: () -> Void = {}
    /// Called when the window closes (Settings are saved then).
    var onClose: () -> Void = {}

    init(commands: Commands, settings: SettingsModel, defaultSize: CGSize? = nil) {
        self.commands = commands
        self.settings = settings
        self.defaultSize = defaultSize
        library = LibraryModel.makeLibrary(core: commands.core)
        super.init()
        nav.openTracker = { [weak self] id in self?.commands.showMain(.tracker(trackerId: id), nil) }
    }

    var isOpen: Bool { window != nil }
    var libraryModel: LibraryModel? { model }
    var nsWindow: NSWindow? { window }

    /// Opens the window, on `settings` (a tab of Settings) when given: Settings is a section of this window. `torrent`:
    /// a click on its title in the notifier (the core decides where the window goes).
    func show(section: LibraryModel.Section? = nil, info id: String? = nil, torrent: String? = nil,
              settings tab: SettingsTab? = nil, editTracker: String? = nil, newTrackerHost: String? = nil, openSettings: Bool = false) {
        let opening = self.model == nil
        let model = self.model ?? makeWindow()
        if openSettings || tab != nil || editTracker != nil || newTrackerHost != nil {
            // A plain open starts on General; a deep link (a tab, a tracker to edit) names its own tab.
            if let tab { nav.tab = tab } else if editTracker == nil, newTrackerHost == nil, model.section != .settings { nav.tab = .general }
            if let editTracker { nav.trackerToEdit = editTracker }
            if let newTrackerHost { nav.newTrackerHost = newTrackerHost }
            self.settings.reload()
            model.select(.settings)
        } else if let torrent {
            // Downloads on the row, its files open; an open add sheet keeps the window and its place (SPEC 8.3).
            if model.showTorrent(torrent) == .shown { FilesStore.shared.expand(torrent) }
        } else if let id {
            model.reveal(id: id)
            // The popover anchors to the row, which exists one layout pass later.
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { model.openInfo(id) }
        } else if let section, section != model.section {
            model.select(section)
        }
        // Opened plainly (Open Seedwell…, the Dock): on the download that arrived meanwhile, if it still downloads.
        if opening {
            model.takeArrival(plain: section == nil && id == nil && torrent == nil && tab == nil && !openSettings && editTracker == nil && newTrackerHost == nil)
        }
        onOpenChange()
        NSApp.activate()
        window?.makeKeyAndOrderFront(nil)
        // Nothing takes the first-responder ring the moment the window is key: it opens calm (SPEC 8.3).
        DispatchQueue.main.async { [window] in window?.makeFirstResponder(nil) }
    }

    func closeForQuit() { window?.close() }

    /// A click on a title in the notifier: the window comes forward on Downloads, this torrent selected and scrolled into
    /// view, its files open (selection mode for a torrent that waits for a choice). The torrent stays held as it is. With
    /// the "Add a Download" sheet open the window only comes forward: the sheet keeps its text, Downloads gets its dot.
    func showTorrent(_ id: String) {
        show(torrent: id)
    }

    /// A torrent became a download (the watcher's `onArrival`, SPEC 8.3): the open window switches to
    /// Downloads and selects it (the core decides); a closed one remembers it for its next opening.
    func newDownload(_ id: String) {
        if let model { model.newDownload(id) } else {
            _ = library.newDownload(rowId: id, context: WindowContext(editing: false, openLists: [], fileTorrent: nil, fileRows: 0))
        }
    }

    private func makeWindow() -> LibraryModel {
        let model = LibraryModel(core: commands.core, library: library)
        model.editing = { [commands] in commands.favorites.target != nil || commands.favorites.removal != nil }
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
        // A quiet QA run never saves its frame (off every screen) where the installed app would read it.
        if defaultSize == nil, !QuietRun.on {
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

    /// Polls only while the window can actually be seen; in a quiet QA run, off every screen, always (QuietRun).
    private func updatePolling() {
        if QuietRun.on || window?.occlusionState.contains(.visible) == true { model?.start() } else { model?.stop() }
    }

    /// The window's keys (Delete, Return, Escape: the Remove… question; ⌘I: Get Info; ⌘A: Select All), as the core
    /// decides them. Window-wide, so they work without the list holding focus, but never while typing in a field.
    private func handleKey(_ event: NSEvent) -> Bool {
        guard let w = window, let model else { return false }
        if event.window !== w { return handleInfoKey(event) }
        guard w.isKeyWindow, !(w.firstResponder is NSText) else { return false }
        let key: LibraryKey
        switch event.keyCode {
        case 51, 117: key = .delete  // delete, forward delete
        case 36, 76: key = .return  // return, enter
        case 53: key = .escape
        default:
            // A letter by what it types, as menu key equivalents are (the keyboard layout decides).
            switch event.charactersIgnoringModifiers?.lowercased() {
            case "i": key = .i
            case "a": key = .a
            default: return false
            }
        }
        let m = event.modifierFlags
        let mods = KeyModifiers(shift: m.contains(.shift), command: m.contains(.command), option: m.contains(.option), control: m.contains(.control))
        return model.key(key, mods, scope: .window)
    }

    /// ↑/↓ while Get Info's popover (a child window of this one) has the keys, as it does once it opens: the list's
    /// arrows, so the selection moves and Get Info follows it to the row (or closes for several, SPEC 8.3).
    private func handleInfoKey(_ event: NSEvent) -> Bool {
        guard let w = window, let model, model.infoRow != nil, let popover = event.window, popover.parent === w, popover.isKeyWindow,
              !(popover.firstResponder is NSText), event.keyCode == 125 || event.keyCode == 126 else { return false }
        let m = event.modifierFlags
        let mods = KeyModifiers(shift: m.contains(.shift), command: m.contains(.command), option: m.contains(.option), control: m.contains(.control))
        return model.key(event.keyCode == 125 ? .down : .up, mods, scope: .list)
    }

    /// Get Info's popover while it is open (QA).
    var infoWindow: NSWindow? {
        guard model?.infoRow != nil else { return nil }
        return window?.childWindows?.first { $0.isVisible && String(describing: type(of: $0)).contains("Popover") }
    }

    /// Edit › Select All when no text field takes it (the window's delegate is the last stop of its responder chain):
    /// the rows, as ⌘A.
    @objc func selectAll(_ sender: Any?) {
        _ = model?.key(.a, KeyModifiers(shift: false, command: true, option: false, control: false), scope: .window)
    }

    func windowWillClose(_ notification: Notification) {
        if let keyMonitor { NSEvent.removeMonitor(keyMonitor) }
        keyMonitor = nil
        onClose()
        model?.close()
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
