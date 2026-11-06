import AppKit
import FileProvider
import os
import SeedwellFileProviderShared
import SwiftUI

/// The system's File Provider domains, behind a seam: the tests use a fake, so no test ever registers a domain.
protocol FileProviderDomains: Sendable {
    func list() async throws -> [FileProviderDomainState]
    func add(id: String, name: String) async throws
    /// Removes the domain; what the system had not handed to the extension yet is kept, and its place returned.
    func remove(id: String) async throws -> URL?
    func signal(id: String) async throws
    /// Where the Finder shows an item of the domain (the core's identifier; `root`: the domain's own folder).
    func itemURL(id: String, item: String) async throws -> URL?
    /// Waits until the system has handed the extension every change below the item (`root`: the whole domain).
    func waitForChanges(id: String, below item: String) async throws
    /// The items (the core's identifiers) the system has on disk in the domain.
    func materialized(id: String) async throws -> [String]
    /// Asks the system to download an item (it fetches it from the extension when it can).
    func requestDownload(id: String, item: String) async throws
}

/// A registered domain: its folder, name and whether the user allowed it (System Settings).
struct FileProviderDomainState: Equatable, Sendable {
    var id: String
    var name: String
    var enabled: Bool
}

/// The real domains (NSFileProviderManager).
struct SystemDomains: FileProviderDomains {
    private func domain(_ id: String) async throws -> NSFileProviderDomain? {
        try await NSFileProviderManager.domains().first { $0.identifier.rawValue == id }
    }

    func list() async throws -> [FileProviderDomainState] {
        try await NSFileProviderManager.domains().map { FileProviderDomainState(id: $0.identifier.rawValue, name: $0.displayName, enabled: $0.userEnabled) }
    }

    func add(id: String, name: String) async throws {
        // An existing domain of that identifier only takes the new name.
        try await NSFileProviderManager.add(NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(rawValue: id), displayName: name))
    }

    func remove(id: String) async throws -> URL? {
        guard let d = try await domain(id) else { return nil }
        return try await NSFileProviderManager.remove(d, mode: .preserveDirtyUserData)
    }

    func signal(id: String) async throws {
        guard let d = try await domain(id), let m = NSFileProviderManager(for: d) else { return }
        try await m.signalEnumerator(for: .workingSet)
    }

    func itemURL(id: String, item: String) async throws -> URL? {
        guard let d = try await domain(id), let m = NSFileProviderManager(for: d) else { return nil }
        return try await m.getUserVisibleURL(for: FPIdentifiers.provider(item))
    }

    func materialized(id: String) async throws -> [String] {
        guard let d = try await domain(id), let m = NSFileProviderManager(for: d) else { return [] }
        let set = MaterializedSet(m.enumeratorForMaterializedItems())
        return try await withCheckedThrowingContinuation { (c: CheckedContinuation<[String], Error>) in
            set.collect { ids in
                if let ids { c.resume(returning: ids) } else { c.resume(throwing: CocoaError(.fileReadUnknown)) }
            }
        }
    }

    /// `NSFileProviderManager.waitForChanges(below:)` (macOS 11.3, the Barrier category): returns once the system has
    /// handed the extension every change on disk below the item.
    func waitForChanges(id: String, below item: String) async throws {
        guard let d = try await domain(id), let m = NSFileProviderManager(for: d) else { return }
        try await m.waitForChanges(below: FPIdentifiers.provider(item))
    }

    func requestDownload(id: String, item: String) async throws {
        guard let d = try await domain(id), let m = NSFileProviderManager(for: d) else { return }
        try await m.requestDownloadForItem(withIdentifier: FPIdentifiers.provider(item), requestedRange: NSRange(location: NSNotFound, length: 0))
    }
}

/// On-demand folders in the Finder (SPEC 7G.11), the app's side: it serves the extension (the core's server, at the
/// socket in the app group's container) and keeps one domain, the hub (*Seedwell* in the Finder's sidebar), which lists
/// every synced folder as the core says: on-demand folders as folders of placeholders, full ones as links to their
/// place. It signals the system when the hub's feed moves, lets go of a folder before the core removes or switches it,
/// moves the domains of the per-folder era over once, and notices when macOS keeps the hub switched off (a new domain
/// is, until the user allows it in System Settings).
@MainActor @Observable final class FileProviderHost {
    @ObservationIgnored let core: Core
    @ObservationIgnored let domains: FileProviderDomains
    /// The hub's domain (the core's).
    @ObservationIgnored let hub: SyncHostDomain
    /// Domains macOS keeps out of the Finder until the user allows Seedwell (the hub, while it is off).
    private(set) var disabled: Set<String> = []
    /// Opens the window that asks the user to allow Seedwell (wired by the app delegate).
    @ObservationIgnored var showSetup: () -> Void = {}
    /// Tells the user something (what the system kept of a folder let go of, and where it is now).
    @ObservationIgnored var onNotice: (_ title: String, _ text: String) -> Void = { _, _ in }
    /// The core's let-go of a folder and its undo (plumbing: a test records them). They read the disk: run off the
    /// main thread.
    @ObservationIgnored var letGo: @Sendable (_ folder: String, _ visible: String?) throws -> SyncHostNotice?
    @ObservationIgnored var undoLetGo: @Sendable (_ folder: String) -> Void
    /// One reconcile at a time: each waits for the one before (a stale list never undoes a newer one). The migration
    /// from per-folder domains is the first of them.
    @ObservationIgnored private var reconciling: Task<Void, Never>?
    /// The domains the core last asked for (the hub, once any folder is synced).
    @ObservationIgnored private var wanted: [SyncHostDomain] = []
    /// Domains whose held files were shown downloaded this run (once each, when enabled), and those doing it now.
    @ObservationIgnored private var shownHeld: Set<String> = []
    @ObservationIgnored private var showingHeld: Set<String> = []
    @ObservationIgnored private var poll: Timer?
    @ObservationIgnored private let log = Logger(subsystem: "io.github.kkollsga.Seedwell", category: "file-provider")

    init(core: Core, domains: FileProviderDomains = SystemDomains()) {
        self.core = core
        self.domains = domains
        hub = core.syncHostHubDomain()
        letGo = { folder, visible in try core.syncHostLetGo(id: folder, visible: visible) }
        undoLetGo = { folder in core.syncHostLetGoUndo(id: folder) }
    }

    /// Serves the extension (the user's own app only: the core refuses an isolated run), then moves the domains of the
    /// per-folder era over (once) before the hub is registered.
    func start() {
        guard let dir = FPGroup.container else {
            log.error("no app group container: on-demand folders stay out of the Finder")
            return
        }
        do {
            let socket = try core.syncHostStart(dir: dir.path, listener: FileProviderListener(self), mode: .hub)
            log.info("serving the File Provider extension at \(socket, privacy: .public)")
            // Removals the app did not finish the last time it ran.
            for n in core.syncHostUnfinishedReleases() { onNotice(n.title, n.text) }
            beginMigration()
        } catch {
            log.error("File Provider server: \(SyncText.message(error), privacy: .public)")
        }
    }

    func stop() {
        poll?.invalidate()
        poll = nil
        core.syncHostStop()
    }

    /// The migration runs first; reconciles (the hub's registration among them) wait for it.
    func beginMigration() {
        let before = reconciling
        reconciling = Task { @MainActor [weak self] in
            await before?.value
            await self?.migrate()
        }
    }

    // MARK: The core's calls

    /// The domains there should be (the core reports them only while folder sync runs: the hub, while any folder is
    /// synced): a missing one is added, a renamed one takes the new name. Nothing is removed for that (the domains of
    /// the per-folder era went in the migration; a folder leaves the hub through `releaseDomain`). One at a time, in
    /// order, after the migration.
    func reconcile(_ want: [SyncHostDomain]) async {
        let before = reconciling
        let next = Task { @MainActor [weak self] in
            await before?.value
            await self?.reconcileNow(want)
        }
        reconciling = next
        await next.value
    }

    private func reconcileNow(_ want: [SyncHostDomain]) async {
        if want.contains(where: { $0.id == hub.id }) {
            // The core tells the hub again when an on-demand folder joined it (switched here, by the command line or an
            // agent): what the core holds shows downloaded again (asking for what is on disk already asks nothing).
            shownHeld.remove(hub.id)
        }
        wanted = want
        var new: Set<String> = []
        do {
            let have = try await domains.list()
            for d in want where !have.contains(where: { $0.id == d.id && $0.name == d.name }) {
                // One that cannot be added now is tried again on its own.
                guard await addDomain(d.id, name: d.name) else { continue }
                if !have.contains(where: { $0.id == d.id }) { new.insert(d.id) }
                log.info("domain added: \(d.name, privacy: .public)")
            }
        } catch {
            log.error("domains: \(error.localizedDescription, privacy: .public)")
        }
        await refreshEnabled()
        // The hub registered now and kept off by macOS: the user is asked, once.
        if !new.isDisjoint(with: disabled) { showSetup() }
    }

    /// How long the system may take to hand over a folder's pending changes before it is let go of (plumbing: what it
    /// still has then is kept by the let-go anyway).
    var handOverWait: UInt64 = 60_000_000_000

    /// Runs `body` for at most `handOverWait`: the system's hand-over, which a stuck extension could hold forever.
    /// `waitForChanges(below:)` ignores cancellation, so the two race and whichever ends first ends the wait (the
    /// hand-over left behind finishes on its own); never awaited past the cap.
    private func handOver(_ body: @escaping @Sendable () async throws -> Void) async {
        let (log, wait, once) = (self.log, handOverWait, Once())
        await withCheckedContinuation { (done: CheckedContinuation<Void, Never>) in
            let timer = Task.detached {
                try? await Task.sleep(nanoseconds: wait)
                if once.claim() {
                    log.info("hand-over: no answer in time, going on")
                    done.resume()
                }
            }
            Task.detached {
                do { try await body() } catch { log.info("hand-over: \(error.localizedDescription, privacy: .public)") }
                if once.claim() {
                    // Over first: the timer has nothing left to do.
                    timer.cancel()
                    done.resume()
                }
            }
        }
    }

    /// The core lets go of a folder (it is about to be removed or switched to full, whoever asked): the system hands
    /// over what it has pending below the folder in the hub (`waitForChanges(below:)`), then the core refuses what it
    /// sends for it, keeps what is still on disk there and unsynced next to the folder (the user is told), and the
    /// folder leaves the hub. The hub itself stays. `attempt` says whether the core still waits: once it gave up (its
    /// removal or switch refused), nothing is let go of, and a let-go that finishes late is undone.
    func releaseDomain(_ id: String, attempt: ReleaseAttempt = ReleaseAttempt()) async -> SyncHostRemoval {
        do {
            // No hub registered: nothing of the folder is in the Finder.
            guard let d = try await domains.list().first(where: { $0.id == hub.id }) else { return SyncHostRemoval(error: nil) }
            let (core, domains, hubId) = (self.core, self.domains, hub.id)
            let item = await Task.detached(priority: .utility) { core.syncHostHubItem(id: id) }.value
            // Switched off, the system has nothing of it on disk, and nothing pending.
            if d.enabled { await handOver { try await domains.waitForChanges(id: hubId, below: item) } }
            guard attempt.commit() else {
                log.info("the core gave up waiting: the folder stays in the hub")
                return SyncHostRemoval(error: syncHostText(t: .noAnswer))
            }
            let visible = d.enabled ? (try? await domains.itemURL(id: hubId, item: item))?.path : nil
            let (letGo, undo) = (self.letGo, self.undoLetGo)
            let r = await Task.detached(priority: .utility) { Result { try letGo(id, visible) } }.value
            switch r {
            case .success(let n): if let n { onNotice(n.title, n.text) }
            case .failure(let e):
                // Something there could not be kept: the folder stays (the system would delete it), and the removal or
                // switch is refused with the core's words.
                // (Never `finish`: a core that gave up meanwhile must hear a refusal, not that the folder went.)
                await Task.detached(priority: .utility) { undo(id) }.value
                log.error("let-go refused: \(SyncText.message(e), privacy: .public)")
                return SyncHostRemoval(error: SyncText.message(e))
            }
            try? await domains.signal(id: hubId)
            if !attempt.finish() {
                // The core refused meanwhile: the folder stays, so it comes back in the hub.
                let undo = self.undoLetGo
                await Task.detached(priority: .utility) { undo(id) }.value
                try? await domains.signal(id: hubId)
                log.info("the core gave up while the folder was let go of: back in the hub")
                return SyncHostRemoval(error: syncHostText(t: .noAnswer))
            }
            log.info("folder let go of before it was removed or switched")
            return SyncHostRemoval(error: nil)
        } catch {
            return SyncHostRemoval(error: error.localizedDescription)
        }
    }

    /// Moves the domains of the per-folder era (`~/Library/CloudStorage/Seedwell-<Name>`) over to the hub, once: the
    /// core says which are left. Each goes through the safe path: noted first (a start after the app stopped in the
    /// middle tells the user), the system's pending changes handed over, removed keeping what is unsynced, which goes
    /// next to its folder (the user is told). Only when every one went is the migration noted done; else the next
    /// start goes on with what is left. Never removes the hub.
    func migrate() async {
        let core = self.core
        guard let have = try? await domains.list() else { return }
        let registered = have.map { SyncHostDomain(id: $0.id, name: $0.name) }
        let plan = await Task.detached(priority: .utility) { core.syncHostMigration(registered: registered) }.value
        var whole = true
        for d in plan {
            // (Notes are files: written off the main thread.)
            await Task.detached(priority: .utility) { core.syncHostReleaseBegin(id: d.id, name: d.name) }.value
            if have.first(where: { $0.id == d.id })?.enabled == true {
                let domains = self.domains
                await handOver { try await domains.waitForChanges(id: d.id, below: FPIdentifiers.root) }
            }
            do {
                let preserved = try await domains.remove(id: d.id)
                await keep(preserved, id: d.id, name: d.name)
                log.info("domain of the per-folder era removed: \(d.name, privacy: .public)")
            } catch {
                // Still there: nothing was lost, and the next start tries again.
                whole = false
                log.error("migration: \(d.name, privacy: .public): \(error.localizedDescription, privacy: .public)")
            }
            await Task.detached(priority: .utility) { core.syncHostReleaseDone(id: d.id) }.value
        }
        if whole { await Task.detached(priority: .utility) { core.syncHostMigrationDone() }.value }
    }

    /// Waits before each new try to add a domain the system refused (plumbing; nothing else would try again until the
    /// core's list changed).
    var addRetries: [UInt64] = [1, 2, 4, 8, 16, 32, 64].map { $0 * 1_000_000_000 }

    /// Adds (or renames) a domain; one the system refuses is tried again in the background with growing waits. A domain
    /// added anew shows its held files downloaded again. Whether it was added now.
    @discardableResult
    func addDomain(_ id: String, name: String) async -> Bool {
        do {
            try await domains.add(id: id, name: name)
            shownHeld.remove(id)
            return true
        } catch {
            log.error("add domain: \(error.localizedDescription, privacy: .public); trying again")
            let waits = addRetries
            Task { @MainActor [weak self] in
                for w in waits {
                    try? await Task.sleep(nanoseconds: w)
                    guard let self, self.wanted.contains(where: { $0.id == id }) else { return }
                    if (try? await self.domains.add(id: id, name: name)) != nil {
                        self.shownHeld.remove(id)
                        await self.refreshEnabled()
                        return
                    }
                }
            }
            return false
        }
    }

    /// What the system kept of a domain removed here goes where the user finds it (the core says where, and moves it,
    /// across volumes too), and the user is told.
    private func keep(_ preserved: URL?, id: String, name: String) async {
        guard let preserved else { return }
        let core = self.core
        let n = await Task.detached(priority: .utility) { core.syncHostKeepPreserved(id: id, name: name, preserved: preserved.path) }.value
        if let n { onNotice(n.title, n.text) }
    }

    /// The hub's change feed moved: the system asks for the changes.
    func feedMoved(_ id: String) async {
        do { try await domains.signal(id: id) } catch { log.info("signal \(id, privacy: .public): \(error.localizedDescription, privacy: .public)") }
    }

    /// Whether macOS keeps the hub switched off. While it does, checked again every two seconds (System Settings has
    /// no callback); the folder pages and the setup window follow.
    func refreshEnabled() async {
        let now: Set<String>
        do {
            let list = try await domains.list()
            now = Set(list.filter { d in !d.enabled && wanted.contains { $0.id == d.id } }.map(\.id))
        } catch {
            return
        }
        if now != disabled { disabled = now }
        // Enabled: what the core holds shows downloaded (once a run, and again when a folder became on demand).
        for d in wanted where !disabled.contains(d.id) && !shownHeld.contains(d.id) {
            shownHeld.insert(d.id)
            Task { await showHeld(d.id) }
        }
        if disabled.isEmpty {
            poll?.invalidate()
            poll = nil
        } else if poll == nil {
            poll = Timer.scheduledTimer(withTimeInterval: 2, repeats: true) { [weak self] _ in
                Task { @MainActor [weak self] in await self?.refreshEnabled() }
            }
        }
    }

    /// Whether macOS keeps on-demand folders out of the Finder now (the hub is off): the folder page says so.
    func isOff(_ folder: String) -> Bool { disabled.contains(hub.id) }

    // MARK: The app's calls

    /// A folder became on demand here (switched, joined): its files the core holds show downloaded in the hub. (The
    /// user is asked to allow Seedwell once, when the hub is registered; the folder page says so while it is off.)
    func added(_ f: SyncFolder) async {
        shownHeld.remove(hub.id)
        await refreshEnabled()
    }

    /// Files the core holds show downloaded in the Finder, not online only (a folder switched to on demand has every
    /// file here; a domain added again finds them here too): the core lists those the system does not have on disk,
    /// and the system is asked for them in the core's batches. Their fetch hands back clones of the cache (no space
    /// twice, nothing downloaded), and the core never takes them for the system letting go of something.
    func showHeld(_ id: String) async {
        guard !showingHeld.contains(id) else { return }
        showingHeld.insert(id)
        defer { showingHeld.remove(id) }
        let onDisk: [String]
        do { onDisk = try await domains.materialized(id: id) } catch {
            shownHeld.remove(id)  // tried again on the next check
            return
        }
        let core = self.core
        let queued = await Task.detached(priority: .utility) { (try? core.syncHostMaterialize(id: id, onDisk: onDisk)) ?? 0 }.value
        guard queued > 0 else { return }
        log.info("showing \(queued) files held here as downloaded")
        let domains = self.domains
        while let batch = await Task.detached(priority: .utility, operation: { core.syncHostNextBatch(id: id) }).value {
            await withTaskGroup(of: Void.self) { group in
                for item in batch.ids {
                    group.addTask { try? await domains.requestDownload(id: id, item: item) }
                }
            }
            try? await Task.sleep(nanoseconds: batch.pauseMs * 1_000_000)
        }
    }

    /// Where the Finder shows a synced folder: its item in the hub (`~/Library/CloudStorage/…/<Name>`).
    func rootURL(_ folder: String) async -> URL? {
        let core = self.core
        let item = await Task.detached(priority: .utility) { core.syncHostHubItem(id: folder) }.value
        return try? await domains.itemURL(id: hub.id, item: item)
    }
}

/// The core's server tells the host from its own thread: hop to the main actor.
final class FileProviderListener: SyncHostListener, @unchecked Sendable {
    private weak var host: FileProviderHost?
    private let peers: PeerVerifier
    /// Waits on the core's thread for the host to signal that the domain went; `.timedOut` is the core giving up (it
    /// then refuses the removal or switch). Plumbing: the app waits `wait` seconds, a test says when the time is up.
    private let waitForHost: @Sendable (DispatchSemaphore) -> DispatchTimeoutResult

    convenience init(_ host: FileProviderHost, peers: PeerVerifier = PeerVerifier(), wait: TimeInterval = 120) {
        self.init(host, peers: peers, waitForHost: { $0.wait(timeout: .now() + wait) })
    }

    init(_ host: FileProviderHost, peers: PeerVerifier = PeerVerifier(), waitForHost: @escaping @Sendable (DispatchSemaphore) -> DispatchTimeoutResult) {
        self.host = host
        self.peers = peers
        self.waitForHost = waitForHost
    }

    /// Only this app's extension is served (asked on the server's thread, before a request is read).
    func verifyPeer(auditToken: Data) -> Bool { peers.verify(auditToken) }

    func feedMoved(folderId: String) {
        Task { @MainActor [weak host] in await host?.feedMoved(folderId) }
    }

    func domains(domains: [SyncHostDomain]) {
        Task { @MainActor [weak host] in await host?.reconcile(domains) }
    }

    /// The core is about to remove or switch a folder: blocks its thread until the domain is gone (never the main
    /// thread's: the core's removals run off it, on a thread of the core's own). Never called from a thread of Swift's
    /// cooperative pool either: the host's work runs there, and a blocked pool thread can starve it.
    func removeDomain(folderId: String) -> SyncHostRemoval {
        guard !Thread.isMainThread else { return SyncHostRemoval(error: syncHostText(t: .busy)) }
        let result = RemovalBox()
        let attempt = ReleaseAttempt()
        let done = DispatchSemaphore(value: 0)
        Task { @MainActor [weak host] in
            result.value = await host?.releaseDomain(folderId, attempt: attempt) ?? SyncHostRemoval(error: nil)
            done.signal()
        }
        guard waitForHost(done) == .success else {
            // The host may have finished just now, between its last look and the signal: the domain is gone, and the
            // core is told so (not a refusal that leaves the folder on demand without its domain).
            if attempt.giveUp() { return SyncHostRemoval(error: nil) }
            return SyncHostRemoval(error: syncHostText(t: .noAnswer))
        }
        return result.value
    }

    func notice(title: String, text: String) {
        Task { @MainActor [weak host] in host?.onNotice(title, text) }
    }

    /// Windows' entry in File Explorer's navigation pane: the Finder's one entry is the hub, a domain of its own.
    func navPane(entry: SyncNavPane) {}
}

/// A removal's outcome, handed from the main actor to the core's waiting thread.
final class RemovalBox: @unchecked Sendable {
    var value = SyncHostRemoval(error: nil)
}

/// Whether the core still waits for a domain's removal, decided once under one lock: the removal goes ahead
/// (`commit`) only while the core waits, and when it is done (`finish`) either the core still waits (and takes it) or
/// it gave up (and the host adds the domain back). A give-up and a finish never both win.
final class ReleaseAttempt: @unchecked Sendable {
    private let lock = NSLock()
    private var gaveUp = false
    private var committed = false
    private var finished = false

    /// The core stopped waiting. `true` when the host had finished the removal already (the domain is gone: the core
    /// takes that); `false` otherwise, and from now on `commit` and `finish` say no.
    func giveUp() -> Bool {
        lock.withLock {
            if finished { return true }
            gaveUp = true
            return false
        }
    }

    /// The domain may go now (the core still waits); `false` once it gave up.
    func commit() -> Bool {
        lock.withLock {
            if gaveUp { return false }
            committed = true
            return true
        }
    }

    /// The committed removal is done: `true` while the core still waits (it is told the domain went); `false` when it
    /// gave up meanwhile (the folder stays on demand, so the domain must come back).
    func finish() -> Bool {
        lock.withLock {
            precondition(committed, "finish follows commit")
            if gaveUp { return false }
            finished = true
            return true
        }
    }
}

/// System Settings at the File Providers list (Login Items & Extensions; `com.apple.ExtensionsPreferences` is that
/// pane's alias and takes the extension point), else at Login Items & Extensions.
@MainActor enum FileProviderSettings {
    static let pane = "x-apple.systempreferences:com.apple.ExtensionsPreferences?extensionPointIdentifier=com.apple.fileprovider-nonui"
    static let fallback = "x-apple.systempreferences:com.apple.LoginItems-Settings.extension"

    static func open() {
        if let url = URL(string: pane), NSWorkspace.shared.open(url) { return }
        if let url = URL(string: fallback) { NSWorkspace.shared.open(url) }
    }
}

/// The window that asks the user to allow Seedwell in the Finder; it notices by itself when the switch is on.
struct FileProviderSetupView: View {
    let host: FileProviderHost
    var done: () -> Void = {}

    var body: some View {
        let n = syncOnDemandNotice(enabled: host.disabled.isEmpty)
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 3) {
                ModalCaption(text: n.caption)
                Text(n.title).font(Theme.title)
            }
            Text(n.text).font(Theme.body).fixedSize(horizontal: false, vertical: true).padding(.top, 12)
            HStack(spacing: 8) {
                Spacer()
                if let b = n.button {
                    Button(b, action: FileProviderSettings.open).help(n.help).accessibilityIdentifier("fp-setup-open")
                }
                Button(n.done, action: done).buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction).help(n.doneHelp)
            }
            .padding(.top, 22)
        }
        .padding(22)
        .frame(width: 440)
        .background(.background)
        .task { await host.refreshEnabled() }
    }
}

/// A flag taken once, from any thread: of two racing ends, only the first goes on.
final class Once: @unchecked Sendable {
    private let lock = NSLock()
    private var taken = false

    func claim() -> Bool {
        lock.withLock {
            defer { taken = true }
            return !taken
        }
    }
}
