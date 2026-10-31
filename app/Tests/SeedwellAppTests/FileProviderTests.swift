import FileProvider
import Foundation
import SeedwellFileProviderShared
import Testing
@testable import SeedwellApp

/// On-demand folders in the Finder (SPEC 7G.11): the extension's wire and its mapping onto File Provider's types, the
/// socket client against a fake app, and the app's domains against fake ones. No domain is ever registered here.
@Suite struct FileProviderTests {
    /// The item `sync_host_tests.rs` (`an_item_on_the_wire`) writes: the two sides must stay in step.
    static let rustItem = #"{"id":"a","parent":"d","name":"a.txt","kind":"file","size":5,"mtime_ns":1792000000000000000,"content_version":"c-a","metadata_version":"m-a","state":"online_only","keep":true,"pinned":true,"exec":false,"symlink_target":null,"caps":["read","write","rename","reparent","delete"]}"#

    static func decoder() -> JSONDecoder {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }

    @Test func theAppsItemIsRead() throws {
        let i = try Self.decoder().decode(FPItem.self, from: Data(Self.rustItem.utf8))
        #expect(i == FPItem(id: "a", parent: "d", name: "a.txt", kind: "file", size: 5, mtimeNs: 1_792_000_000_000_000_000,
                            contentVersion: "c-a", metadataVersion: "m-a", state: "online_only", keep: true, pinned: true,
                            caps: ["read", "write", "rename", "reparent", "delete"]))
    }

    @Test func aRequestIsOneLineInSnakeCase() throws {
        var r = FPRequest("create", folder: "F")
        r.parent = "root"
        r.name = "a b.txt"
        r.mayExist = true
        let line = String(decoding: try r.line(), as: UTF8.self)
        #expect(line == #"{"v":1,"op":"create","folder":"F","parent":"root","name":"a b.txt","may_exist":true}"# + "\n")
    }

    @Test func aBigReportComesInNumberedParts() {
        let ids = (0..<12_001).map { "i\($0)" }
        let parts = FPRequest.materialized(folder: "F", ids: ids, report: 7)
        #expect(parts.map { $0.ids?.count ?? 0 } == [5000, 5000, 2001])
        #expect(parts.map(\.part) == [0, 1, 2] && parts.allSatisfy { $0.report == 7 && $0.parts == 3 })
        // An empty set is one part too.
        #expect(FPRequest.materialized(folder: "F", ids: [], report: 8).map(\.parts) == [1])
    }

    @Test func identifiers() {
        #expect(FPIdentifiers.provider("root") == .rootContainer)
        #expect(FPIdentifiers.core(.rootContainer) == "root")
        #expect(FPIdentifiers.core(NSFileProviderItemIdentifier("7f3a")) == "7f3a")
        #expect(FPIdentifiers.provider("7f3a").rawValue == "7f3a")
    }

    @Test func errorsMeanWhatTheAppSays() {
        func code(_ k: FPFailure.Kind) -> Int { FPErrors.error(FPFailure(k, "why")).code }
        #expect(code(.unreachable) == NSFileProviderError.Code.serverUnreachable.rawValue)
        #expect(code(.notRunning) == NSFileProviderError.Code.serverUnreachable.rawValue)
        #expect(code(.anchorExpired) == NSFileProviderError.Code.syncAnchorExpired.rawValue)
        #expect(code(.deletionRejected) == NSFileProviderError.Code.deletionRejected.rawValue)
        #expect(code(.notEmpty) == NSFileProviderError.Code.directoryNotEmpty.rawValue)
        #expect(code(.excluded) == NSFileProviderError.Code.excludedFromSync.rawValue)
        #expect(code(.failed) == NSFileProviderError.Code.cannotSynchronize.rawValue)
        #expect(code(.noSuchItem) == NSFileProviderError.Code.noSuchItem.rawValue)
        #expect(FPErrors.error(FPFailure(.cancelled, "x")).code == NSUserCancelledError)
        // The core's words are what the user reads.
        #expect(FPErrors.error(FPFailure(.failed, "Renaming needs a device that has these files to be connected.")).localizedDescription
            == "Renaming needs a device that has these files to be connected.")
    }

    @Test func capabilitiesAndPolicyFollowTheApp() {
        let file = FPProviderItem(FPItem(id: "a", parent: "d", name: "a.txt", kind: "file", size: 5, contentVersion: "c", metadataVersion: "m",
                                         caps: ["read", "write", "rename", "reparent", "delete"]))
        #expect(file.capabilities == [.allowsReading, .allowsWriting, .allowsRenaming, .allowsReparenting, .allowsDeleting])
        #expect(file.contentPolicy == .inherited && file.documentSize?.intValue == 5)
        #expect(file.itemVersion.contentVersion == Data("c".utf8) && file.itemVersion.metadataVersion == Data("m".utf8))
        #expect(file.parentItemIdentifier.rawValue == "d" && file.contentType == .plainText)
        // A read-only member: read, and a delete that frees.
        let ro = FPProviderItem(FPItem(id: "a", parent: "root", name: "a.txt", kind: "file", caps: ["read", "delete"]))
        #expect(ro.capabilities == [.allowsReading, .allowsDeleting] && !ro.fileSystemFlags.contains(.userWritable))
        #expect(ro.parentItemIdentifier == .rootContainer)
        let kept = FPProviderItem(FPItem(id: "d", parent: "root", name: "Docs", kind: "dir", keep: true, pinned: true, caps: ["read", "write"]))
        #expect(kept.contentPolicy == .downloadEagerlyAndKeepDownloaded && kept.contentType == .folder)
        #expect(kept.capabilities == [.allowsContentEnumerating, .allowsAddingSubItems])
        #expect(kept.userInfo?["pinned"] as? Bool == true)
        // A package is a folder the Finder shows as one file.
        #expect(FPProviderItem(FPItem(id: "p", parent: "root", name: "Tool.app", kind: "dir")).contentType.conforms(to: .package))
        #expect(FPProviderItem(FPItem(id: "q", parent: "root", name: "v1.2", kind: "dir")).contentType.conforms(to: .folder))
        let root = FPProviderItem(FPItem(id: "root", parent: "root", name: "", kind: "dir"), rootName: "Team")
        #expect(root.itemIdentifier == .rootContainer && root.filename == "Team" && root.contentPolicy == .downloadLazily)
    }

    @Test func pagesAndAnchors() {
        #expect(FPPages.token(FPPages.page("Docs/a.txt")) == "Docs/a.txt")
        #expect(FPPages.token(NSFileProviderPage.initialPageSortedByName as NSFileProviderPage) == nil)
        #expect(FPPages.token(NSFileProviderPage.initialPageSortedByDate as NSFileProviderPage) == nil)
        #expect(FPPages.anchor(FPPages.anchor("18446744073709551614")) == "18446744073709551614")
        #expect(FPPages.version(NSFileProviderItemVersion(contentVersion: Data("c-a".utf8), metadataVersion: Data())) == "c-a")
    }

    // MARK: The socket

    @Test func theClientReadsProgressAndTheAnswer() throws {
        let app = try FakeApp { req in
            #expect(req["op"] as? String == "fetch" && req["item"] as? String == "a")
            return [#"{"v":1,"progress":{"done":0,"total":5}}"#, #"{"v":1,"progress":{"done":3,"total":5}}"#,
                    #"{"v":1,"ok":{"item":\#(Self.rustItem),"path":"/tmp/x"}}"#]
        }
        var seen: [UInt64] = []
        let got = FPClient(dir: app.dir, gate: nil).ask(FPRequest("fetch", folder: "F", item: "a"), as: FPFetched.self,
                                                        progress: { done, _ in seen.append(done) })
        #expect(try got.get().path == "/tmp/x" && seen == [0, 3])
    }

    /// Long work sends a line now and then: the client keeps waiting, and takes the answer that follows.
    @Test func theClientWaitsThroughKeepAlives() throws {
        let app = try FakeApp { _ in [#"{"v":1,"alive":true}"#, #"{"v":1,"alive":true}"#, #"{"v":1,"ok":{"deleted":1}}"#] }
        #expect(try FPClient(dir: app.dir, gate: nil).ask(FPRequest("delete", folder: "F", item: "a"), as: FPDeleted.self).get().deleted == 1)
    }

    @Test func theAppsErrorsComeBackAsTheyWere() throws {
        let app = try FakeApp { _ in [#"{"v":1,"err":{"kind":"anchor_expired","message":"list again"}}"#] }
        let got = FPClient(dir: app.dir, gate: nil).ask(FPRequest("changes", folder: "F"), as: FPChanges.self)
        #expect(got == .failure(FPFailure(.anchorExpired, "list again")))
    }

    @Test func noAppIsNotRunning() {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let got = FPClient(dir: dir, gate: nil).ask(FPRequest("hello", folder: ""), as: FPHello.self)
        #expect(got == .failure(FPFailure(.notRunning, "Seedwell isn't running.")))
    }

    /// The app is started when nothing listens, once, and the request waits for it.
    @Test func theAppIsStartedWhenItIsNotRunning() throws {
        let dir = try FakeApp.tempDir()
        let launched = Counter()
        let holder = Holder()
        let gate = FPLaunchGate(every: 30, wait: 5) {
            launched.add()
            holder.app = try? FakeApp(dir: dir) { _ in [#"{"v":1,"ok":{"app":"Seedwell 0.1.11"}}"#] }
        }
        let client = FPClient(dir: dir, gate: gate)
        #expect(try client.ask(FPRequest("hello", folder: ""), as: FPHello.self).get().app == "Seedwell 0.1.11")
        #expect(launched.n == 1)
        // A moment later it is not started again: the request fails at once.
        holder.app = nil
        #expect(client.ask(FPRequest("hello", folder: ""), as: FPHello.self) == .failure(FPFailure(.notRunning, "Seedwell isn't running.")))
        #expect(launched.n == 1)
    }

    @Test func aCancelledRequestEnds() throws {
        let app = try FakeApp(hold: true) { _ in [#"{"v":1,"progress":{"done":0,"total":5}}"#] }
        let got = FPClient(dir: app.dir, gate: nil).ask(FPRequest("fetch", folder: "F", item: "a"), as: FPFetched.self,
                                                        progress: { _, _ in }, opened: { c in
                                                            DispatchQueue.global().asyncAfter(deadline: .now() + 0.2) { c.cancel() }
                                                        })
        #expect(got == .failure(FPFailure(.cancelled, "Cancelled.")))
    }

    /// A container whose socket path is longer than a socket path may be: reached relative to the folder.
    @Test func aSocketInADeepFolder() throws {
        let deep = try FakeApp.tempDir().appendingPathComponent(String(repeating: "a", count: 60)).appendingPathComponent(String(repeating: "b", count: 60))
        try FileManager.default.createDirectory(at: deep, withIntermediateDirectories: true)
        let app = try FakeApp(dir: deep) { _ in [#"{"v":1,"ok":{"app":"deep"}}"#] }
        #expect(app.dir.appendingPathComponent("fp").path.utf8.count > 104)
        #expect(try FPClient(dir: deep, gate: nil).ask(FPRequest("hello", folder: ""), as: FPHello.self).get().app == "deep")
    }

    // MARK: The app's domains

    @MainActor @Test func domainsFollowTheCore() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        var asked = 0
        host.showSetup = { asked += 1 }
        await fake.set([FileProviderDomainState(id: "old", name: "Gone", enabled: true)])
        await host.reconcile([SyncHostDomain(id: "F", name: "Team")])
        #expect(await fake.domains == [FileProviderDomainState(id: "F", name: "Team", enabled: false)])
        #expect(host.disabled == ["F"] && asked == 1, "a new domain starts switched off: the user is asked once")
        // Renamed: the same domain takes the new name (and nobody is asked again).
        await host.reconcile([SyncHostDomain(id: "F", name: "Team 2")])
        #expect(await fake.domains.map(\.name) == ["Team 2"] && asked == 1)
        // The user allowed it in System Settings.
        await fake.enable("F")
        await host.refreshEnabled()
        #expect(host.disabled.isEmpty)
        host.stop()
    }

    /// A domain the core's list lacks stays unless the core's configuration says its folder was removed or is full
    /// (the list is no evidence: a folder that could not open, or is stopped for a move, is missing from it).
    @MainActor @Test func aDomainGoesOnlyWhenTheCoreSaysSo() async throws {
        let (core, fake) = try Self.host()
        var folders: [SyncFolder] = []
        for _ in 0..<100 where folders.isEmpty {
            folders = core.syncSnapshot().folders
            if folders.isEmpty { try await Task.sleep(nanoseconds: 50_000_000) }
        }
        let f = try core.syncSetOnDemand(id: try #require(folders.first).id, onDemand: true, path: nil)
        let host = FileProviderHost(core: core, domains: fake)
        let full = try #require(folders.dropFirst().first)
        await fake.set([FileProviderDomainState(id: f.id, name: f.name, enabled: true), FileProviderDomainState(id: "gone", name: "Old", enabled: true),
                        FileProviderDomainState(id: full.id, name: full.name, enabled: true)])
        // An empty list: the on-demand folder's domain stays; the removed one's and the full one's go.
        await host.reconcile([])
        #expect(await fake.domains.map(\.id) == [f.id])
        host.stop()
    }

    /// The core lets go of a folder: the system hands over its changes first, then the domain goes, and what it kept
    /// comes back for the core. A folder without a domain: nothing to do.
    @MainActor @Test func releasingADomainHandsOverFirst() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        await fake.set([FileProviderDomainState(id: "F", name: "Team", enabled: true)])
        let r = await host.releaseDomain("F")
        #expect(r.error == nil)
        #expect(await fake.waited == ["F"])
        #expect(await fake.domains.isEmpty)
        #expect(await host.releaseDomain("F") == SyncHostRemoval(error: nil))
        host.stop()
    }

    /// The core gave up waiting (the system was slow to hand over): the removal it refused does not happen late, and
    /// the domain stays as it was.
    @MainActor @Test func aRemovalTheCoreGaveUpOnLeavesTheDomain() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        await fake.set([FileProviderDomainState(id: "F", name: "Team", enabled: true)])
        await fake.setSlowHandOver(500_000_000)
        let listener = FileProviderListener(host, wait: 0.1)
        let r = await Task.detached { listener.removeDomain(folderId: "F") }.value
        #expect(r.error == syncHostText(t: .noAnswer))
        try await Task.sleep(nanoseconds: 900_000_000)
        #expect(await fake.domains.map(\.id) == ["F"], "still there")
        #expect(core.syncHostUnfinishedReleases().isEmpty, "the note of the removal went with it")
        host.stop()
    }

    /// The core gave up while the domain was going (the system's removal slow, its hand-over finished regardless of
    /// the cutoff): the domain comes back.
    @MainActor @Test func aDomainRemovedAfterTheCoreGaveUpComesBack() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        await fake.set([FileProviderDomainState(id: "F", name: "Team", enabled: true)])
        // The removal starts well inside the core's wait and outlasts it, with room for a busy machine: the test is
        // about a give-up during the removal (one before it starts removes nothing, as it should).
        await fake.setSlowRemoval(3_000_000_000)
        let listener = FileProviderListener(host, wait: 1.0)
        let r = await Task.detached { listener.removeDomain(folderId: "F") }.value
        #expect(r.error == syncHostText(t: .noAnswer))
        for _ in 0..<160 {
            let (back, gone) = (await fake.domains.contains(where: { $0.id == "F" }), await fake.removals)
            if back && gone == 1 { break }
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        #expect(await fake.removals == 1, "it did go")
        #expect(await fake.domains.map(\.id) == ["F"], "and came back")
        host.stop()
    }

    /// A domain the system refuses to add is tried again by itself, not only when the core's list changes.
    @MainActor @Test func aDomainTheSystemRefusesIsTriedAgain() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        host.addRetries = [20_000_000, 20_000_000, 20_000_000]
        await fake.failAdds(2)
        await host.reconcile([SyncHostDomain(id: "F", name: "Team")])
        for _ in 0..<50 {
            if await fake.domains.contains(where: { $0.id == "F" }) { break }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        #expect(await fake.domains.map(\.id) == ["F"])
        host.stop()
    }

    /// Reconciles run one at a time: a burst adds each domain once.
    @MainActor @Test func reconcilesRunInOrder() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        let burst = (0..<5).map { _ in Task { @MainActor in await host.reconcile([SyncHostDomain(id: "F", name: "Team")]) } }
        for t in burst { await t.value }
        #expect(await fake.adds == 1)
        host.stop()
    }

    @MainActor @Test func aNewOnDemandFolderAsksToBeAllowed() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        var asked = 0
        host.showSetup = { asked += 1 }
        var f = SyncTests.folder("Team", id: "F")
        f.onDemand = true
        await host.added(f)
        #expect(asked == 1 && host.disabled == ["F"])
        // Allowed already: nothing to ask.
        await fake.enable("G")
        f.id = "G"
        await fake.set(await fake.domains + [FileProviderDomainState(id: "G", name: "Team", enabled: true)])
        await host.added(f)
        #expect(asked == 1)
        host.stop()
    }

    @Test func theNoticeIsTheCores() {
        let off = syncOnDemandNotice(enabled: false)
        #expect(off.title == "Allow Seedwell in the Finder" && off.button == "Open Settings")
        #expect(off.caption == "Folder Sync" && off.done == "Done" && !off.help.isEmpty)
        #expect(syncOnDemandNotice(enabled: true).button == nil)
        #expect(syncOnDemandOffLine().contains("System Settings"))
    }

    // MARK: The setup choice

    @Test func theAddModalChoosesFilesOnDemand() {
        var form = SyncAddForm()
        #expect(!form.onDemand, "full unless chosen")
        form.onDemand = true
        #expect(syncOnDemandLabel() == "Files on demand")
        #expect(syncOnDemandNote(change: .join, placeShown: "").contains("until you open it"))
        #expect(syncOnDemandNote(change: .add, placeShown: "").contains("asks you once"))
    }

    @MainActor @Test func theEditModalSaysWhatASwitchDoes() async throws {
        let (core, _) = try Self.host()
        var folders: [SyncFolder] = []
        for _ in 0..<100 where folders.isEmpty {
            folders = core.syncSnapshot().folders
            if folders.isEmpty { try await Task.sleep(nanoseconds: 50_000_000) }
        }
        let f = try #require(folders.first)
        let m = SyncEditModel(core: core, id: f.id)
        #expect(!m.onDemand && m.onDemandNote == nil && !m.changed)
        m.onDemand = true
        #expect(m.changed && m.onDemandNote?.contains("Remove Download") == true)
        // An on-demand folder switched back: everything is downloaded, into its place.
        _ = try core.syncSetOnDemand(id: f.id, onDemand: true, path: nil)
        let back = SyncEditModel(core: core, id: f.id)
        #expect(back.onDemand && back.onDemandNote?.contains("Every file is listed") == true)
        back.onDemand = false
        #expect(back.onDemandNote?.contains("downloaded to this Mac again, into \(Notifier.abbreviate(f.path))") == true)
        #expect(syncPlaceLine(folder: core.syncSnapshot().folders.first { $0.id == f.id }!, pathShown: "x") == "In the Finder’s sidebar, under Locations")
    }

    /// A folder switched to on demand has its files here: they are asked for, so the Finder shows them downloaded;
    /// what the system has on disk already, and what is online only, is not.
    @MainActor @Test func filesHeldHereAreShownDownloaded() async throws {
        let (core, fake) = try Self.host()
        var folders: [SyncFolder] = []
        for _ in 0..<100 where folders.isEmpty {
            folders = core.syncSnapshot().folders
            if folders.isEmpty { try await Task.sleep(nanoseconds: 50_000_000) }
        }
        let f = try core.syncSetOnDemand(id: try #require(folders.first).id, onDemand: true, path: nil)
        // The server, in a temporary folder (no domain is registered: the domains are fake).
        let dir = try FakeApp.tempDir()
        defer { core.syncHostStop(); try? FileManager.default.removeItem(at: dir) }
        _ = try core.syncHostStart(dir: dir.path, listener: QuietListener())
        let host = FileProviderHost(core: core, domains: fake)
        await fake.set([FileProviderDomainState(id: f.id, name: f.name, enabled: true)])
        await fake.setOnDisk(["root", "m:notes.txt"])
        await host.reconcile([SyncHostDomain(id: f.id, name: f.name)])
        for _ in 0..<100 {
            if await fake.requested.count >= 2 { break }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        // The mock's held files: plan.md (downloaded), draft.pages (only here); notes.txt is on disk already.
        #expect(Set(await fake.requested) == ["m:Projects/plan.md", "m:Projects/draft.pages"])
        // Once a run: checking again asks for nothing more.
        await host.refreshEnabled()
        try await Task.sleep(nanoseconds: 100_000_000)
        #expect(await fake.requested.count == 2)
        host.stop()
    }

    static func host() throws -> (Core, FakeDomains) {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("seedwell-fp-\(UUID().uuidString)").path
        let core = try Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
        return (core, FakeDomains())
    }
}

/// Domains in memory: added switched off, as macOS adds them.
actor FakeDomains: FileProviderDomains {
    private(set) var domains: [FileProviderDomainState] = []

    func set(_ d: [FileProviderDomainState]) { domains = d }

    func enable(_ id: String) {
        for i in domains.indices where domains[i].id == id { domains[i].enabled = true }
    }

    func list() async throws -> [FileProviderDomainState] { domains }

    private(set) var adds = 0
    private(set) var waited: [String] = []
    private var preserved: URL?
    func setPreserved(_ u: URL?) { preserved = u }
    private var handOver: UInt64 = 0
    func setSlowHandOver(_ ns: UInt64) { handOver = ns }
    func waitForChanges(id: String) async throws {
        waited.append(id)
        if handOver > 0 { try await Task.sleep(nanoseconds: handOver) }
    }

    private var failing = 0
    func failAdds(_ n: Int) { failing = n }
    private var removalTakes: UInt64 = 0
    func setSlowRemoval(_ ns: UInt64) { removalTakes = ns }
    private(set) var removals = 0

    func add(id: String, name: String) async throws {
        if failing > 0 {
            failing -= 1
            throw CocoaError(.fileWriteUnknown)
        }
        adds += 1
        await Task.yield()
        if let i = domains.firstIndex(where: { $0.id == id }) { domains[i].name = name } else { domains.append(FileProviderDomainState(id: id, name: name, enabled: false)) }
    }

    func remove(id: String) async throws -> URL? {
        removals += 1
        if removalTakes > 0 {
            // Not cancellable, as the system's own removal.
            let t = removalTakes
            await Task.detached { try? await Task.sleep(nanoseconds: t) }.value
        }
        domains.removeAll { $0.id == id }
        defer { preserved = nil }
        return preserved
    }

    func signal(id: String) async throws {}
    func rootURL(id: String) async throws -> URL? { nil }

    /// What the system has on disk, and what it was asked to download.
    var onDisk: [String] = []
    private(set) var requested: [String] = []
    func setOnDisk(_ ids: [String]) { onDisk = ids }
    func materialized(id: String) async throws -> [String] { onDisk }
    func requestDownload(id: String, item: String) async throws { requested.append(item) }
}

/// A listener that hears nothing (a server started by a test).
final class QuietListener: SyncHostListener, @unchecked Sendable {
    func feedMoved(folderId: String) {}
    func domains(domains: [SyncHostDomain]) {}
    func verifyPeer(auditToken: Data) -> Bool { true }
    func removeDomain(folderId: String) -> SyncHostRemoval { SyncHostRemoval(error: nil) }
    func notice(title: String, text: String) {}
}

/// Only the app's own extension may use the socket.
@Suite struct PeerVerifierTests {
    @Test func theRequirementIsTheTeamsExtensionOrThisVeryOne() {
        #expect(PeerVerifier.requirementText(teamSigned: true, cdhash: nil)
            == #"identifier "io.github.kkollsga.Seedwell.FileProvider" and anchor apple generic and certificate leaf[subject.OU] = "6GND8Z2C7Z""#)
        #expect(PeerVerifier.requirementText(teamSigned: false, cdhash: Data([0xab, 0x01]))
            == #"identifier "io.github.kkollsga.Seedwell.FileProvider" and cdhash H"ab01""#)
        // Neither: nobody is served.
        #expect(PeerVerifier.requirementText(teamSigned: false, cdhash: nil) == nil)
    }

    @Test func anotherProcessIsRefused() {
        // The test runner: not the extension (and no extension in its bundle to pin).
        var token = audit_token_t()
        var count = mach_msg_type_number_t(MemoryLayout<audit_token_t>.size / MemoryLayout<natural_t>.size)
        let r = withUnsafeMutablePointer(to: &token) {
            $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) { task_info(mach_task_self_, task_flavor_t(TASK_AUDIT_TOKEN), $0, &count) }
        }
        #expect(r == KERN_SUCCESS)
        let mine = withUnsafeBytes(of: &token) { Data($0) }
        // A real requirement (the team's extension), which the test runner does not satisfy.
        let v = PeerVerifier(text: PeerVerifier.requirementText(teamSigned: true, cdhash: nil))
        #expect(v.hasRequirement)
        #expect(!v.verify(mine))
        #expect(!v.verify(Data(count: 32)) && !v.verify(Data([1, 2, 3])))
        // Pinned to another build's cdhash: refused too.
        let pinned = PeerVerifier(text: PeerVerifier.requirementText(teamSigned: false, cdhash: Data(repeating: 0xab, count: 20)))
        #expect(pinned.hasRequirement && !pinned.verify(mine))
        // No requirement at all (no team, nothing to pin): nobody.
        #expect(!PeerVerifier(appBundle: URL(fileURLWithPath: "/nonexistent.app")).verify(mine))
    }
}

final class Counter: @unchecked Sendable {
    private let lock = NSLock()
    private var value = 0
    var n: Int { lock.withLock { value } }
    func add() { lock.withLock { value += 1 } }
}

final class Holder: @unchecked Sendable {
    var app: FakeApp?
}

/// The app's side of the socket, faked: each connection reads one request and writes the lines `answer` gives (and,
/// with `hold`, keeps the connection open until the client closes it).
final class FakeApp: @unchecked Sendable {
    let dir: URL
    private let fd: Int32

    static func tempDir() throws -> URL {
        // A short folder: /tmp, not the per-user temporary folder (its path is long already).
        let d = URL(fileURLWithPath: "/tmp/swfp-\(UUID().uuidString.prefix(8))")
        try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
        return d
    }

    convenience init(hold: Bool = false, answer: @escaping @Sendable ([String: Any]) -> [String]) throws {
        try self.init(dir: try Self.tempDir(), hold: hold, answer: answer)
    }

    init(dir: URL, hold: Bool = false, answer: @escaping @Sendable ([String: Any]) -> [String]) throws {
        self.dir = dir
        let s = socket(AF_UNIX, SOCK_STREAM, 0)
        fd = s
        let path = dir.appendingPathComponent("fp").path
        unlink(path)
        @Sendable func bindName(_ name: String) -> Bool {
            var a = sockaddr_un()
            a.sun_family = sa_family_t(AF_UNIX)
            withUnsafeMutableBytes(of: &a.sun_path) { p in
                for (i, b) in name.utf8.enumerated() { p[i] = b }
            }
            return withUnsafePointer(to: &a) { $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(s, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) } } == 0
        }
        // A long path is bound from a thread whose own working folder is the container, as the app does.
        let ok = path.utf8.count <= 103 ? bindName(path) : FPTestSupport.inFolder(dir.path) { bindName("fp") }
        guard ok, listen(s, 8) == 0 else { throw POSIXError(.EADDRINUSE) }
        Thread {
            while true {
                let c = accept(s, nil, nil)
                if c < 0 { return }
                var line = [UInt8]()
                var b: UInt8 = 0
                while read(c, &b, 1) == 1, b != 10 { line.append(b) }
                let req = (try? JSONSerialization.jsonObject(with: Data(line))) as? [String: Any] ?? [:]
                for l in answer(req) {
                    let d = Array((l + "\n").utf8)
                    _ = d.withUnsafeBytes { write(c, $0.baseAddress, $0.count) }
                }
                if hold { while read(c, &b, 1) == 1 {} }
                close(c)
            }
        }.start()
    }

    deinit {
        shutdown(fd, SHUT_RDWR)
        close(fd)
        unlink(dir.appendingPathComponent("fp").path)
    }
}

/// Binding by a name relative to a folder, from a thread of its own (the client's way, for the fake app).
enum FPTestSupport {
    static func inFolder(_ folder: String, _ body: @escaping @Sendable () -> Bool) -> Bool {
        typealias FChdir = @convention(c) (Int32) -> Int32
        guard let sym = dlsym(UnsafeMutableRawPointer(bitPattern: -2), "pthread_fchdir_np") else { return false }
        let fchdir = unsafeBitCast(sym, to: FChdir.self)
        let dirfd = open(folder, O_RDONLY | O_DIRECTORY)
        defer { close(dirfd) }
        let done = DispatchSemaphore(value: 0)
        let result = Counter()
        Thread {
            if fchdir(dirfd) == 0, body() { result.add() }
            done.signal()
        }.start()
        done.wait()
        return result.n == 1
    }
}
