import AppKit

/// The "SOCKS5 proxy" window's state: the fields, which pane shows, the guide, the call running and its answer. The
/// words, the progress and the last result are the core's (`ProxyWindowModel`); the fields go to the core's async calls
/// and the password field is emptied after every call (the key is never shown again once it is kept).
@MainActor @Observable final class ProxyWindowState {
    @ObservationIgnored private weak var center: ProxyCenter?
    @ObservationIgnored let core: Core
    private(set) var model: ProxyWindowModel
    /// The NordVPN service credentials as typed or copied.
    var username = ""
    var password = ""
    /// The custom proxy.
    var host: String
    var port: String
    var customUser = ""
    var customPassword = ""
    /// What a copy put into the username and password, and when the username came (the core lets a copy fill the
    /// password only soon after; a field the user has since changed counts as typed).
    private var copiedUsername: (text: String, at: ContinuousClock.Instant)?
    private var copiedPassword: String?
    /// One of the fields holds what a copy put there: the model's `pasted_note` shows under them.
    var filledFromCopy: Bool {
        (copiedUsername.map { $0.text == username && !username.isEmpty } ?? false) || (copiedPassword.map { $0 == password && !password.isEmpty } ?? false)
    }
    /// The guide to the service credentials is shown.
    var guideShown = false
    /// With a key installed, its pane shows the form again ("Replace Key…") instead of the installed line.
    var replacing = false {
        didSet { if oldValue, !replacing { username = ""; password = ""; customUser = ""; customPassword = "" } }
    }
    /// The pane shown changed: a form opened for replacing is closed again.
    var pane: ProxyKind {
        didSet { if pane != oldValue { replacing = false } }
    }
    /// The call running (its button shows it and every button waits).
    private(set) var running: Call?
    /// Why the last call was refused, in the core's words (a refusal before a test, such as a missing port, is said
    /// only here; the model's `result` says the rest).
    private(set) var refusal: String?

    enum Call { case enable, custom, findFaster, remove }
    /// Cancel under a form opened for replacing: the installed line again, the fields emptied.
    func cancelReplace() { replacing = false }

    var busy: Bool { running != nil }

    @ObservationIgnored private let pasteboard: ProxyPasteboard
    @ObservationIgnored private var seenChange = 0
    @ObservationIgnored private var pasteTimer: Timer?
    /// The pasteboard is watched (only while the window is open).
    private(set) var watchingPasteboard = false

    init(center: ProxyCenter, pasteboard: ProxyPasteboard) {
        self.center = center
        core = center.core
        self.pasteboard = pasteboard
        let m = center.core.proxyOpenModalModel()
        model = m
        host = m.customHost
        port = m.customPort
        pane = m.kind
    }

    func refresh() { model = core.proxyOpenModalModel() }

    /// The line under the panes: the call's progress, else its refusal or the core's last result (`problem`: red).
    var statusLine: (text: String, problem: Bool, progress: Bool)? {
        if !model.progress.isEmpty { return (model.progress, false, true) }
        if let r = refusal { return (r, true, false) }
        if !model.result.isEmpty { return (model.result, model.resultIsProblem, false) }
        return nil
    }

    // MARK: The calls

    /// "Enable NordVPN": the credentials are tested with NordVPN's servers and kept only when they work. Both fields are
    /// emptied once they are kept; after a refusal the password is (to be copied again), the username stays.
    func enable() {
        guard !busy else { return }
        let (user, pass) = (username, password)
        start(.enable) { core in try await core.proxySetNordvpnKey(user: user, pass: pass) } done: { [weak self] ok in
            self?.password = ""
            if ok { self?.username = ""; self?.replacing = false }
        }
    }

    /// "Use This Proxy": a SOCKS5 proxy of the user's own (empty username and password: none). A port that is not a
    /// number is port 0, which the core refuses in its own words.
    func useCustom() {
        guard !busy else { return }
        let trimmed = { (s: String) -> String? in let t = s.trimmingCharacters(in: .whitespaces); return t.isEmpty ? nil : t }
        let (h, p, u, w) = (host, UInt16(port.trimmingCharacters(in: .whitespaces)) ?? 0, trimmed(customUser), customPassword.isEmpty ? nil : customPassword)
        start(.custom) { core in try await core.proxySetCustom(host: h, port: p, user: u, pass: w) } done: { [weak self] ok in
            self?.customPassword = ""
            if ok { self?.customUser = ""; self?.replacing = false }
        }
    }

    /// "Find a Faster Server" (shown while the proxy is on).
    func findFaster() {
        guard !busy, let center else { return }
        running = .findFaster
        refusal = nil
        center.findFaster { [weak self] r in
            self?.running = nil
            if case .failure(let e) = r { self?.refusal = SyncText.message(e) }
            self?.refresh()
        }
    }

    /// "Remove Key": the key is forgotten and torrents connect directly.
    func removeKey() {
        guard !busy else { return }
        start(.remove) { core in try await core.proxyRemoveKey(); return "" } done: { _ in }
    }

    /// Runs `work` off the main thread (the core's async call), then `done` with whether it worked; the progress shows
    /// meanwhile (the center polls every 500 ms while it runs).
    private func start(_ call: Call, _ work: @escaping @Sendable (Core) async throws -> String,
                       done: @escaping @MainActor (Bool) -> Void) {
        running = call
        refusal = nil
        let core = core
        Task { @MainActor [weak self] in
            let r = await catching { try await work(core) }
            guard let self else { return }
            running = nil
            if case .failure(let e) = r { refusal = SyncText.message(e) }
            done((try? r.get()) != nil)
            center?.refresh()
        }
        // "Testing the key…" as soon as the core says it (then every 500 ms, as the model asks).
        center?.pollSoon()
    }

    // MARK: Copied credentials

    /// How often the pasteboard's change count is looked at while the window is open (it says nothing when it changes;
    /// reading the count never asks the user).
    static let pasteboardEvery: TimeInterval = 0.4

    /// From now on, while the window is open, each copy may fill the NordVPN fields (`proxyPasteFill`). What was on the
    /// pasteboard before is never used.
    func startWatchingPasteboard() {
        guard !watchingPasteboard else { return }
        watchingPasteboard = true
        seenChange = pasteboard.changeCount
        let t = Timer(timeInterval: Self.pasteboardEvery, repeats: true) { [weak self] _ in MainActor.assumeIsolated { self?.checkPasteboard() } }
        t.tolerance = 0.1
        RunLoop.main.add(t, forMode: .common)
        pasteTimer = t
    }

    func stopWatchingPasteboard() {
        watchingPasteboard = false
        pasteTimer?.invalidate()
        pasteTimer = nil
    }

    /// A new copy fills the first empty NordVPN field (the core's rule: the username first, then the password only soon
    /// after the username came from a copy; a field that holds something is never changed). The pasteboard is read only
    /// when its change count has changed, so a timer tick never reads it (no repeated "Allow Paste" questions). Nothing is read while the window is closed, while the other pane shows,
    /// when the fields are hidden (a NordVPN key installed and not being replaced), or when both fields hold something.
    func checkPasteboard() {
        guard watchingPasteboard else { return }
        let change = pasteboard.changeCount
        guard change != seenChange else { return }
        seenChange = change
        // Only into fields that show: the NordVPN pane, with no NordVPN key installed or while replacing it.
        let formShown = model.nordvpnInstalled.isEmpty || replacing
        guard pane == .nordVpn, formShown, !busy, username.isEmpty || password.isEmpty, let text = pasteboard.string() else { return }
        let ago = copiedUsername.flatMap { c -> UInt64? in
            guard c.text == username else { return nil }
            let d = c.at.duration(to: .now).components
            return UInt64(max(0, d.seconds)) * 1000 + UInt64(max(0, d.attoseconds) / 1_000_000_000_000_000)
        }
        switch proxyPasteFill(username: username, password: password, copied: text, usernameCopiedMsAgo: ago) {
        case .username(let t)?:
            username = t
            copiedUsername = (t, .now)
        case .password(let t)?:
            password = t
            copiedPassword = t
        case nil: break
        }
    }
}
