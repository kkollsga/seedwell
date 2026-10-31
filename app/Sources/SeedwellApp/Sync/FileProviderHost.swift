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
    func rootURL(id: String) async throws -> URL?
    /// Waits until the system has handed the extension every change below the domain's root.
    func waitForChanges(id: String) async throws
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

    func rootURL(id: String) async throws -> URL? {
        guard let d = try await domain(id), let m = NSFileProviderManager(for: d) else { return nil }
        return try await m.getUserVisibleURL(for: .rootContainer)
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

    func waitForChanges(id: String) async throws {
        guard let d = try await domain(id), let m = NSFileProviderManager(for: d) else { return }
        try await m.waitForChanges(below: .rootContainer)
    }

    func requestDownload(id: String, item: String) async throws {
        guard let d = try await domain(id), let m = NSFileProviderManager(for: d) else { return }
        try await m.requestDownloadForItem(withIdentifier: FPIdentifiers.provider(item), requestedRange: NSRange(location: NSNotFound, length: 0))
    }
}

/// On-demand folders in the Finder (SPEC 7G.11), the app's side: it serves the extension (the core's server, at the
/// socket in the app group's container), keeps one domain per on-demand folder (the core says which), signals the
/// system when a folder's change feed moves, and notices when macOS keeps a domain switched off (a new domain is,
/// until the user allows it in System Settings).
@MainActor @Observable final class FileProviderHost {
    @ObservationIgnored let core: Core
    @ObservationIgnored let domains: FileProviderDomains
    /// On-demand folders macOS keeps out of the Finder until the user allows Seedwell.
    private(set) var disabled: Set<String> = []
    /// Opens the window that asks the user to allow Seedwell (wired by the app delegate).
    @ObservationIgnored var showSetup: () -> Void = {}
    /// Tells the user something (what the system kept of a removed domain, and where it is now).
    @ObservationIgnored var onNotice: (_ title: String, _ text: String) -> Void = { _, _ in }
    /// One reconcile at a time: each waits for the one before (a stale list never undoes a newer one).
    @ObservationIgnored private var reconciling: Task<Void, Never>?
    /// The domains the core last asked for.
    @ObservationIgnored private var wanted: [SyncHostDomain] = []
    /// Domains whose held files were shown downloaded this run (once each, when enabled), and those doing it now.
    @ObservationIgnored private var shownHeld: Set<String> = []
    @ObservationIgnored private var showingHeld: Set<String> = []
    @ObservationIgnored private var poll: Timer?
    @ObservationIgnored private let log = Logger(subsystem: "io.github.kkollsga.Seedwell", category: "file-provider")

    init(core: Core, domains: FileProviderDomains = SystemDomains()) {
        self.core = core
        self.domains = domains
    }

    /// Serves the extension (the user's own app only: the core refuses an isolated run).
    func start() {
        guard let dir = FPGroup.container else {
            log.error("no app group container: on-demand folders stay out of the Finder")
            return
        }
        do {
            let socket = try core.syncHostStart(dir: dir.path, listener: FileProviderListener(self))
            log.info("serving the File Provider extension at \(socket, privacy: .public)")
            // Removals the app did not finish the last time it ran.
            for n in core.syncHostUnfinishedReleases() { onNotice(n.title, n.text) }
        } catch {
            log.error("File Provider server: \(SyncText.message(error), privacy: .public)")
        }
    }

    func stop() {
        poll?.invalidate()
        poll = nil
        core.syncHostStop()
    }

    // MARK: The core's calls

    /// The domains there should be (the core reports them only while folder sync runs): the missing ones are added,
    /// renamed ones take the new name. A domain the list lacks is never removed for that: only when the core says
    /// its folder was removed or is full (one left from before; a folder let go of now goes through `releaseDomain`).
    /// One at a time, in order.
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
        wanted = want
        var new: Set<String> = []
        do {
            let have = try await domains.list()
            for d in want where !have.contains(where: { $0.id == d.id && $0.name == d.name }) {
                // One that cannot be added now is tried again on its own (the others go on).
                guard await addDomain(d.id, name: d.name) else { continue }
                if !have.contains(where: { $0.id == d.id }) { new.insert(d.id) }
                log.info("domain added: \(d.name, privacy: .public)")
            }
            let core = self.core
            for h in have where !want.contains(where: { $0.id == h.id }) {
                let state = await Task.detached(priority: .utility) { core.syncHostFolderState(id: h.id) }.value
                guard state == .removed || state == .full else { continue }
                let preserved = try await domains.remove(id: h.id)
                log.info("domain of a folder no longer on demand removed: \(h.name, privacy: .public)")
                await keep(preserved, id: h.id, name: h.name)
            }
        } catch {
            log.error("domains: \(error.localizedDescription, privacy: .public)")
        }
        await refreshEnabled()
        // A folder made on demand elsewhere (the CLI, an agent): asked once, as when the user switches one here.
        if !new.isDisjoint(with: disabled) { showSetup() }
    }

    /// How long the system may take to hand over a domain's pending changes before it is removed (plumbing: what it
    /// still has then is kept by the removal anyway).
    static let handOverWait: UInt64 = 60_000_000_000

    /// The core lets go of a folder (it is about to be removed or switched to full, whoever asked): the system hands
    /// over its pending changes first, then the domain goes, and what the system kept is put where the user finds it.
    /// The removal is noted first (a start after the app died in the middle tells the user). `attempt` says whether the
    /// core still waits: once it gave up (its removal or switch refused), a late completion leaves the domain as it was
    /// (or adds it back if it went meanwhile); what the system kept is put away either way.
    func releaseDomain(_ id: String, attempt: ReleaseAttempt = ReleaseAttempt()) async -> SyncHostRemoval {
        do {
            guard let d = try await domains.list().first(where: { $0.id == id }) else { return SyncHostRemoval(error: nil) }
            let core = self.core
            core.syncHostReleaseBegin(id: id, name: d.name)
            defer { core.syncHostReleaseDone(id: id) }
            let domains = self.domains
            let waited = Task { try await domains.waitForChanges(id: id) }
            let timeout = Task { try await Task.sleep(nanoseconds: Self.handOverWait); waited.cancel() }
            _ = await waited.result
            timeout.cancel()
            guard attempt.commit() else {
                log.info("the core gave up waiting: the domain stays")
                return SyncHostRemoval(error: syncHostText(t: .noAnswer))
            }
            let preserved = try await domains.remove(id: id)
            await keep(preserved, id: id, name: d.name)
            if attempt.gaveUpAfterCommit {
                // The core refused meanwhile: the folder stays on demand, so its domain comes back (tried again until
                // it does), its held files shown downloaded again.
                await addDomain(id, name: d.name)
                log.info("the core gave up while the domain went: added back")
                return SyncHostRemoval(error: syncHostText(t: .noAnswer))
            }
            disabled.remove(id)
            wanted.removeAll { $0.id == id }
            log.info("domain removed before its folder was removed or switched")
            return SyncHostRemoval(error: nil)
        } catch {
            return SyncHostRemoval(error: error.localizedDescription)
        }
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
                    guard let self, self.wanted.contains(where: { $0.id == id }) || self.core.syncHostFolderState(id: id) == .onDemand else { return }
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

    /// The folder's change feed moved: the system asks for the changes.
    func feedMoved(_ id: String) async {
        do { try await domains.signal(id: id) } catch { log.info("signal \(id, privacy: .public): \(error.localizedDescription, privacy: .public)") }
    }

    /// Which of the wanted domains macOS keeps switched off. While any is, checked again every two seconds (System
    /// Settings has no callback); the folder pages and the setup window follow.
    func refreshEnabled() async {
        let now: Set<String>
        do {
            let list = try await domains.list()
            now = Set(list.filter { d in !d.enabled && wanted.contains { $0.id == d.id } }.map(\.id))
        } catch {
            return
        }
        if now != disabled { disabled = now }
        // Enabled: what the core holds shows downloaded (once a run, and again after a switch).
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

    // MARK: The app's calls

    /// A folder became on demand here (switched, joined): its domain now, and the setup window when macOS keeps it off.
    func added(_ f: SyncFolder) async {
        shownHeld.remove(f.id)
        if !wanted.contains(where: { $0.id == f.id }) { wanted.append(SyncHostDomain(id: f.id, name: f.name)) }
        await addDomain(f.id, name: f.name)
        await refreshEnabled()
        if disabled.contains(f.id) { showSetup() }
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

    /// Where the Finder shows the folder (`~/Library/CloudStorage/Seedwell-<Name>`).
    func rootURL(_ id: String) async -> URL? {
        try? await domains.rootURL(id: id)
    }
}

/// The core's server tells the host from its own thread: hop to the main actor.
final class FileProviderListener: SyncHostListener, @unchecked Sendable {
    private weak var host: FileProviderHost?
    private let peers: PeerVerifier
    /// How long the core's thread waits for a domain to go (plumbing; the core then refuses the removal or switch).
    private let wait: TimeInterval

    init(_ host: FileProviderHost, peers: PeerVerifier = PeerVerifier(), wait: TimeInterval = 120) {
        self.host = host
        self.peers = peers
        self.wait = wait
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
    /// thread's: the core's removals run off it).
    func removeDomain(folderId: String) -> SyncHostRemoval {
        guard !Thread.isMainThread else { return SyncHostRemoval(error: syncHostText(t: .busy)) }
        let result = RemovalBox()
        let attempt = ReleaseAttempt()
        let done = DispatchSemaphore(value: 0)
        Task { @MainActor [weak host] in
            result.value = await host?.releaseDomain(folderId, attempt: attempt) ?? SyncHostRemoval(error: nil)
            done.signal()
        }
        guard done.wait(timeout: .now() + wait) == .success else {
            attempt.giveUp()
            return SyncHostRemoval(error: syncHostText(t: .noAnswer))
        }
        return result.value
    }

    func notice(title: String, text: String) {
        Task { @MainActor [weak host] in host?.onNotice(title, text) }
    }
}

/// A removal's outcome, handed from the main actor to the core's waiting thread.
final class RemovalBox: @unchecked Sendable {
    var value = SyncHostRemoval(error: nil)
}

/// Whether the core still waits for a domain's removal: the removal goes ahead (`commit`) only while it does, and
/// the host learns if the core gave up after that.
final class ReleaseAttempt: @unchecked Sendable {
    private let lock = NSLock()
    private var gaveUp = false
    private var committed = false

    /// The core stopped waiting.
    func giveUp() { lock.withLock { gaveUp = true } }

    /// The domain may go now (the core still waits); `false` once it gave up.
    func commit() -> Bool {
        lock.withLock {
            if gaveUp { return false }
            committed = true
            return true
        }
    }

    /// The core gave up after the removal went ahead.
    var gaveUpAfterCommit: Bool { lock.withLock { committed && gaveUp } }
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
                Text(n.title).font(.system(size: 17, weight: .semibold))
            }
            Text(n.text).font(.system(size: 13)).fixedSize(horizontal: false, vertical: true).padding(.top, 12)
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
