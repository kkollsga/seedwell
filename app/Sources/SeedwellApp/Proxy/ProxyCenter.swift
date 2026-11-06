import AppKit
import SwiftUI

/// The torrent proxy as the app shows it (SPEC §7H): the shield's model, read from the core only while something shows
/// it (the menu bar menu, Settings › Network, the "SOCKS5 proxy" window), the shield's click and right-click menu (the
/// same in every place), and the window. Every word and state is the core's (`ProxyModel`, `ProxyWindowModel`); the
/// slow calls are the core's async ones, so the main thread never waits for a key test or a server ranking.
@MainActor @Observable final class ProxyCenter {
    @ObservationIgnored let core: Core
    /// The shield and the status line, as last read.
    private(set) var model: ProxyModel
    /// "A NordVPN key is installed. …" (the window model's key line, for Settings › Network; never the key).
    private(set) var keyLine: String
    /// A click's toggle is running: the shield waits for its answer (disabled, so a second click cannot undo the first).
    private(set) var toggling = false
    /// The shield's click, the switch and the country wait: a toggle runs, or the window's call (a key test, Find a
    /// Faster Server), which the core would refuse them during anyway.
    var busy: Bool { toggling || window?.busy == true }
    /// Why the last switch was refused, in the core's words (a test was running), for a few seconds.
    private(set) var notice: String?
    /// The open window's state (nil: closed).
    private(set) var window: ProxyWindowState?
    /// Where copied text is read from while the window is open (QA and the tests: a pasteboard of their own).
    @ObservationIgnored var pasteboard: ProxyPasteboard = SystemPasteboard(.general)
    /// Tests: the window's state without an NSWindow.
    @ObservationIgnored var drawsWindows = true
    @ObservationIgnored private var nsWindow: NSWindow?
    @ObservationIgnored private var closer: WindowCloser?
    /// How many places show the shield now; polled only while some do.
    @ObservationIgnored private var viewers = 0
    @ObservationIgnored private var timer: Timer?
    /// AppKit views that draw the model (the menu's shield): told after each read.
    @ObservationIgnored private var listeners: [ObjectIdentifier: () -> Void] = [:]

    init(core: Core) {
        self.core = core
        model = core.proxyModel()
        keyLine = core.proxyOpenModalModel().keyLine
    }

    // MARK: Reading

    /// Reads the models again (cheap: no Keychain read after the core's first, no network).
    func refresh() {
        model = core.proxyModel()
        keyLine = core.proxyOpenModalModel().keyLine
        window?.refresh()
        notify()
    }

    private func notify() {
        for l in listeners.values { l() }
    }

    /// A place shows the shield: the model is read now and then every `refreshMs` (500 ms while a key or the servers
    /// are tested, else 3 s) until the last place hides it.
    func watch() {
        viewers += 1
        if viewers == 1 { tick() }
    }

    func unwatch() {
        viewers = max(0, viewers - 1)
        if viewers == 0 { timer?.invalidate(); timer = nil }
    }

    /// Whether something shows the shield now (tests: the poll stops when nothing does).
    var polling: Bool { timer != nil }

    private func tick() {
        refresh()
        schedule(after: Double(model.refreshMs) / 1000)
    }

    /// A call that the model will show as running has just started: the next read comes at once, not after up to 3 s.
    func pollSoon() {
        schedule(after: 0.1)
    }

    private func schedule(after seconds: TimeInterval) {
        timer?.invalidate()
        guard viewers > 0 else { timer = nil; return }
        // In every run loop mode: the menu bar menu tracks in its own.
        let t = Timer(timeInterval: seconds, repeats: false) { [weak self] _ in MainActor.assumeIsolated { self?.tick() } }
        t.tolerance = min(0.1, seconds / 2)
        RunLoop.main.add(t, forMode: .common)
        timer = t
    }

    func listen(_ owner: AnyObject, _ changed: @escaping () -> Void) { listeners[ObjectIdentifier(owner)] = changed }
    func stopListening(_ owner: AnyObject) { listeners[ObjectIdentifier(owner)] = nil }

    // MARK: The shield's click and right-click menu

    /// A click on the shield: without a key it opens the window (`clickOpensWindow`), with one it switches the proxy on
    /// or off.
    func click() {
        if model.clickOpensWindow { openWindow() } else if !busy { toggle() }
    }

    /// On or off (the shield's click, the menu's "Turn On NordVPN"). The answer is the new model; a refusal means the key
    /// went meanwhile, and the window opens as a click without a key would.
    func toggle() {
        guard !busy else { return }
        toggling = true
        notice = nil
        notify()
        let core = core
        CoreCalls.inOrder({ await catching { try await core.proxyToggle() } }) { [weak self] r in
            guard let self else { return }
            toggling = false
            if case .success(let m) = r { model = m }
            refresh()
            if case .failure(let e) = r {
                if model.clickOpensWindow { openWindow() } else { say(SyncText.message(e)) }
            }
        }
    }

    /// Shows the core's refusal where the proxy shows (Settings, the window) for a few seconds.
    private func say(_ text: String) {
        notice = text
        Task { @MainActor [weak self] in
            try? await Task.sleep(for: .seconds(6))
            if self?.notice == text { self?.notice = nil }
        }
    }

    /// The menu bar menu's shield's right-click menu.
    var menuEntries: [MenuEntry] { menuEntries(at: .menuBar) }

    /// The shield's right-click menu at `place`: the window, then (with a key) on or off and Find a Faster Server, and
    /// "Country: Sweden…", which opens the country list at that place (not a submenu: about 60 rows would not fit).
    func menuEntries(at place: CountryListPlace) -> [MenuEntry] {
        let m = model
        var entries: [MenuEntry] = [.item(m.menuOpenLabel) { [weak self] in self?.openWindow() }]
        if !m.menuToggleLabel.isEmpty { entries.append(.item(m.menuToggleLabel, enabled: !busy) { [weak self] in self?.toggle() }) }
        if !m.findFasterLabel.isEmpty { entries.append(.item(m.findFasterLabel) { [weak self] in self?.findFaster() }) }
        if !m.countryMenuLabel.isEmpty, !m.countries.isEmpty {
            entries.append(.separator)
            // After the menu has closed: a popover cannot open from inside the menu's tracking.
            entries.append(.item(m.countryMenuLabel, enabled: !busy) { [weak self] in
                DispatchQueue.main.async { MainActor.assumeIsolated { self?.openCountryList(at: place) } }
            })
        }
        return entries
    }

    // MARK: The country list

    /// The open country list and where it shows (nil: none).
    private(set) var countryList: (place: CountryListPlace, state: CountryListState)?
    /// The menu bar item's button, where the list opens for the menu bar menu's shield (that menu has closed by then;
    /// nil: no menu bar item, as in a QA run, and a small stand-in window at the pointer anchors it).
    @ObservationIgnored var menuBarAnchor: () -> NSView? = { nil }
    @ObservationIgnored private var popover: NSPopover?
    @ObservationIgnored private var popoverDelegate: PopoverCloser?
    @ObservationIgnored private var anchorWindow: NSWindow?

    /// Opens the country list at `place`: the window's and Settings' in their own SwiftUI popovers (they watch
    /// `countryList`), the menu bar's in a popover from the menu bar item. Choosing a row closes it and sets the country.
    func openCountryList(at place: CountryListPlace) {
        closeCountryList()
        guard !busy else { return }
        refresh()
        let m = model
        let state = CountryListState(countries: m.countries, chosen: m.country, visibleRows: m.countryVisibleRows,
                                     choose: { [weak self] code in self?.closeCountryList(); self?.setCountry(code) },
                                     cancel: { [weak self] in self?.closeCountryList() })
        countryList = (place, state)
        if place == .menuBar, drawsWindows { showMenuBarPopover(state) }
    }

    /// Closes the open country list, wherever it is (a menu bar menu opening closes it too).
    func closeCountryList() {
        countryList = nil
        if let p = popover {
            popover = nil
            p.close()
        }
        popoverDelegate = nil
        anchorWindow?.orderOut(nil)
        anchorWindow = nil
    }

    /// The menu bar's list: an NSPopover under the menu bar item (else a stand-in at the pointer), closed by a click
    /// elsewhere or Escape. The app comes forward so the arrows and Return reach it.
    private func showMenuBarPopover(_ state: CountryListState) {
        let host = NSHostingController(rootView: CountryListPopover(state: state))
        host.sizingOptions = .preferredContentSize
        let p = NSPopover()
        p.contentViewController = host
        p.behavior = .transient
        p.animates = !QuietRun.on
        let closer = PopoverCloser { [weak self] in
            guard let self, self.popover === p else { return }
            self.popover = nil
            self.closeCountryList()
        }
        p.delegate = closer
        popoverDelegate = closer
        popover = p
        NSApp.activate()
        if let button = menuBarAnchor(), button.window != nil {
            p.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        } else {
            let at = NSEvent.mouseLocation
            let w = NSWindow(contentRect: NSRect(x: at.x - 1, y: at.y - 1, width: 2, height: 2), styleMask: .borderless, backing: .buffered, defer: false)
            w.isOpaque = false
            w.backgroundColor = .clear
            w.level = .popUpMenu
            w.isReleasedWhenClosed = false
            w.ignoresMouseEvents = true
            w.orderFront(nil)
            anchorWindow = w
            p.show(relativeTo: w.contentView!.bounds, of: w.contentView!, preferredEdge: .minY)
        }
        host.view.window?.identifier = NSUserInterfaceItemIdentifier("country-list")
    }

    /// QA: the menu bar's popover window, while it shows.
    var countryPopoverWindow: NSWindow? { popover?.contentViewController?.view.window }

    /// The user's country (a code from the model's countries, "" none). The servers are ranked again at once.
    func setCountry(_ code: String) {
        let core = core
        CoreCalls.inOrder({ await catching { try await core.proxySetCountry(code: code) } }) { [weak self] r in
            guard let self else { return }
            if case .success(let m) = r { model = m }
            refresh()
        }
    }

    /// "Find a Faster Server": the servers are ranked now; meanwhile the status says "Testing servers…" (the window
    /// shows the outcome).
    func findFaster(then done: @escaping @MainActor (Result<String, Error>) -> Void = { _ in }) {
        let core = core
        CoreCalls.run({ await catching { try await core.proxyFindFaster() } }) { [weak self] r in
            self?.refresh()
            done(r)
        }
        // The "Testing servers…" line, where the shield is.
        pollSoon()
    }

    // MARK: The window

    /// Opens the "SOCKS5 proxy" window (or brings it forward). While it is open the pasteboard is watched for the
    /// credentials copied in Nord Account.
    func openWindow() {
        if let w = nsWindow { NSApp.activate(); w.makeKeyAndOrderFront(nil); return }
        if window != nil { return }
        let state = ProxyWindowState(center: self, pasteboard: pasteboard)
        window = state
        state.startWatchingPasteboard()
        watch()
        guard drawsWindows else { return }
        let host = NSHostingController(rootView: ProxyWindowView(state: state, center: self))
        host.sizingOptions = .preferredContentSize
        let w = NSWindow(contentViewController: host)
        w.styleMask = [.titled, .closable, .fullSizeContentView]
        w.titleVisibility = .hidden
        w.titlebarAppearsTransparent = true
        w.isReleasedWhenClosed = false
        w.colorSpace = Windows.space
        w.identifier = NSUserInterfaceItemIdentifier("proxy")
        w.title = state.model.title
        let closer = WindowCloser { [weak self] in self?.windowClosed() }
        w.delegate = closer
        self.closer = closer
        w.center()
        nsWindow = w
        NSApp.activate()
        w.makeKeyAndOrderFront(nil)
    }

    /// Close (the button, Escape or the window's close button).
    func closeWindow() {
        guard let w = nsWindow else { windowClosed(); return }
        w.close()  // the delegate's windowWillClose ends the rest
    }

    private func windowClosed() {
        guard let state = window else { return }
        if countryList?.place == .window { closeCountryList() }
        state.stopWatchingPasteboard()
        window = nil
        unwatch()
        if let w = nsWindow {
            nsWindow = nil
            w.delegate = nil
            DispatchQueue.main.async { w.contentViewController = nil }
        }
        closer = nil
    }
}

/// Where a country list opens: from the menu bar menu's shield, Settings › Network's or the window's (its country
/// button, also for the window header shield's menu).
enum CountryListPlace { case menuBar, settings, window }

/// Tells the center when the menu bar's country popover closed (a click elsewhere, Escape).
@MainActor private final class PopoverCloser: NSObject, NSPopoverDelegate {
    let closed: () -> Void
    init(_ closed: @escaping () -> Void) { self.closed = closed }
    func popoverDidClose(_ notification: Notification) { closed() }
}

/// Ends the window's state when it closes, however it closes.
@MainActor private final class WindowCloser: NSObject, NSWindowDelegate {
    let closed: () -> Void
    init(_ closed: @escaping () -> Void) { self.closed = closed }
    func windowWillClose(_ notification: Notification) { closed() }
}

/// Where the window reads copied text: the system's pasteboard (QA and the tests: one of their own).
@MainActor protocol ProxyPasteboard: AnyObject {
    /// Changes with every copy, in any app (reading it never asks the user).
    var changeCount: Int { get }
    /// The copied text, when it is text. On macOS 15.4 and later a read the user did not start may ask them first
    /// ("Allow Paste"); a pasteboard they set to Always Deny is not read at all.
    func string() -> String?
}

@MainActor final class SystemPasteboard: ProxyPasteboard {
    let board: NSPasteboard
    init(_ board: NSPasteboard) { self.board = board }
    var changeCount: Int { board.changeCount }
    func string() -> String? {
        if #available(macOS 15.4, *), board.accessBehavior == .alwaysDeny { return nil }
        guard board.types?.contains(.string) == true else { return nil }
        return board.string(forType: .string)
    }
}
