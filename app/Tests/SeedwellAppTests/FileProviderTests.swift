import FileProvider
import Foundation
import SeedwellFileProviderShared
import Testing
@testable import SeedwellApp

/// On-demand folders in the Finder (SPEC 7G.11): the extension's wire and its mapping onto File Provider's types, the
/// socket client against a fake app, and the app's domains against fake ones. No domain is ever registered here.
@Suite(.timeLimit(.minutes(1))) struct FileProviderTests {
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

    // MARK: The hub

    /// Fails if the Finder gets more than one entry: the host registers only the hub the core names (*Seedwell*),
    /// renames it when told, and asks the user to allow it once, when it is registered, never again per folder.
    @MainActor @Test func theHubIsTheOneDomainAndIsAskedForOnce() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        var asked = 0
        host.showSetup = { asked += 1 }
        let hub = core.syncHostHubDomain()
        #expect(hub.name == "Seedwell")
        await host.reconcile([hub])
        #expect(await fake.domains == [FileProviderDomainState(id: hub.id, name: "Seedwell", enabled: false)])
        #expect(host.disabled == [hub.id] && asked == 1, "a new domain starts switched off: the user is asked once")
        #expect(host.isOff("F"), "every on-demand folder's page says so")
        // A folder made on demand: no domain of its own, and nobody is asked again.
        var f = SyncTests.folder("Team", id: "F")
        f.onDemand = true
        await host.added(f)
        await host.reconcile([hub])
        let (adds, count) = (await fake.adds, await fake.domains.count)
        #expect(asked == 1 && adds == 1 && count == 1)
        // The user allowed it in System Settings.
        await fake.enable(hub.id)
        await host.refreshEnabled()
        #expect(host.disabled.isEmpty && !host.isOff("F"))
        host.stop()
    }

    /// Fails if the migration from the per-folder domains removes one before the system handed over its pending
    /// changes, registers the hub before every old domain went, gives up the rest when one cannot be removed (the next
    /// start must go on), or runs again once it is done.
    @MainActor @Test func theMigrationMovesTheOldDomainsOverOnce() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        let hub = core.syncHostHubDomain()
        await fake.set([FileProviderDomainState(id: "F", name: "Team", enabled: true), FileProviderDomainState(id: "G", name: "Off", enabled: false),
                        FileProviderDomainState(id: "H", name: "Stuck", enabled: true)])
        await fake.failRemovals(of: ["H"])
        host.beginMigration()
        await host.reconcile([hub])
        // F's pending changes were handed over first; G was switched off (nothing pending); H could not go.
        #expect(await fake.waited == ["F/root", "H/root"])
        let steps = await fake.steps
        #expect(steps.prefix(2) == ["wait F", "remove F"], "\(steps)")
        #expect(await fake.domains.map(\.id).sorted() == ["H", hub.id])
        #expect(await fake.steps.last == "add \(hub.id)", "the hub comes after the old domains went")
        #expect(core.syncHostUnfinishedReleases().isEmpty, "every removal was noted done")
        // The next start goes on with what is left.
        await fake.failRemovals(of: [])
        let next = FileProviderHost(core: core, domains: fake)
        next.beginMigration()
        await next.reconcile([hub])
        #expect(await fake.domains.map(\.id) == [hub.id])
        // Done: never again, also for a domain that turns up later.
        await fake.set(await fake.domains + [FileProviderDomainState(id: "X", name: "Later", enabled: true)])
        let third = FileProviderHost(core: core, domains: fake)
        third.beginMigration()
        await third.reconcile([hub])
        #expect(await fake.domains.map(\.id).contains("X"))
        host.stop()
        next.stop()
        third.stop()
    }

    /// The core lets go of a folder: fails if the hub's pending changes below the folder are not handed over before
    /// the core's let-go, or if the hub goes with the folder. Without a hub: nothing to do.
    @MainActor @Test func releasingAFolderHandsOverFirstAndKeepsTheHub() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        let hub = core.syncHostHubDomain()
        #expect(await host.releaseDomain("F") == SyncHostRemoval(error: nil), "no hub: nothing to hand over")
        let lets = Steps()
        host.letGo = { f, visible in lets.add("let go \(f) \(visible ?? "-")"); return nil }
        await fake.set([FileProviderDomainState(id: hub.id, name: hub.name, enabled: true)])
        await fake.setVisible("/tmp/hub/Team")
        let r = await host.releaseDomain("F")
        #expect(r.error == nil)
        #expect(await fake.waited == ["\(hub.id)/\(core.syncHostHubItem(id: "F"))"])
        #expect(lets.all == ["let go F /tmp/hub/Team"])
        #expect(await fake.domains.map(\.id) == [hub.id], "the hub stays")
        host.stop()
    }

    /// Fails if a let-go that was refused goes ahead late: the core gives up (the test says when) while the system is
    /// still handing over, and the folder must stay in the hub, never let go of after the core said no.
    @MainActor @Test func aRemovalTheCoreGaveUpOnLeavesTheFolder() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        let hub = core.syncHostHubDomain()
        let lets = Steps()
        host.letGo = { f, _ in lets.add("let go \(f)"); return nil }
        await fake.set([FileProviderDomainState(id: hub.id, name: hub.name, enabled: true)])
        await fake.hold(.handOver)
        let cutoff = DispatchSemaphore(value: 0)
        let listener = FileProviderListener(host, waitForHost: { _ in _ = cutoff.wait(timeout: .now() + 30); return .timedOut })
        let call = Task { await Self.fromCoreThread { listener.removeDomain(folderId: "F") } }
        let began = await Self.within("the hand-over started") { await fake.hasStarted(.handOver) }
        cutoff.signal()
        #expect(await call.value.error == syncHostText(t: .noAnswer))
        await fake.finish(.handOver)
        guard began else { return host.stop() }
        // The late hand-over finishes; nothing is let go of.
        try await Task.sleep(nanoseconds: 200_000_000)
        #expect(lets.all.isEmpty, "nothing was let go of late")
        host.stop()
    }

    /// Fails if a folder stays out of the hub after the core gave up during its let-go: the core refused, the folder
    /// stays, so the let-go must be undone. The give-up happens while the let-go runs by construction: the fake holds
    /// the signal that follows it, and the test ends the core's wait only once it started.
    @MainActor @Test func aFolderLetGoOfAfterTheCoreGaveUpComesBack() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        let hub = core.syncHostHubDomain()
        let lets = Steps()
        host.letGo = { f, _ in lets.add("let go \(f)"); return nil }
        host.undoLetGo = { f in lets.add("undo \(f)") }
        await fake.set([FileProviderDomainState(id: hub.id, name: hub.name, enabled: true)])
        await fake.hold(.signal)
        let cutoff = DispatchSemaphore(value: 0)
        let listener = FileProviderListener(host, waitForHost: { _ in _ = cutoff.wait(timeout: .now() + 30); return .timedOut })
        let call = Task { await Self.fromCoreThread { listener.removeDomain(folderId: "F") } }
        let began = await Self.within("the let-go started") { await fake.hasStarted(.signal) }
        cutoff.signal()
        #expect(await call.value.error == syncHostText(t: .noAnswer))
        await fake.finish(.signal)
        guard began else { return host.stop() }
        _ = await Self.within("the let-go undone") { lets.all.contains("undo F") }
        #expect(lets.all == ["let go F", "undo F"])
        host.stop()
    }

    /// Fails if the core is told "no answer" for a folder already let go of: the core's wait ends just as the host
    /// finished (after its last look at the give-up, before its signal reached the core). The core must take the
    /// removal, not refuse it and leave a folder out of the hub that it keeps.
    @MainActor @Test func aGiveUpRacingTheFinishedLetGoTakesIt() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        let hub = core.syncHostHubDomain()
        let lets = Steps()
        host.letGo = { f, _ in lets.add("let go \(f)"); return nil }
        host.undoLetGo = { f in lets.add("undo \(f)") }
        await fake.set([FileProviderDomainState(id: hub.id, name: hub.name, enabled: true)])
        // The host signals first; the core's wait reports a timeout all the same (the two crossed).
        let listener = FileProviderListener(host, waitForHost: { done in _ = done.wait(timeout: .now() + 30); return .timedOut })
        let r = await Self.fromCoreThread { listener.removeDomain(folderId: "F") }
        #expect(r.error == nil, "the folder was let go of: the core removes or switches it")
        #expect(lets.all == ["let go F"], "not undone")
        host.stop()
    }

    /// Fails if a let-go that could not keep something lets the core remove or switch the folder anyway (the system would
    /// delete what was not kept): the removal is refused with the core's words, and the let-go is undone.
    @MainActor @Test func aLetGoThatCannotKeepAFileRefusesTheRemoval() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        let hub = core.syncHostHubDomain()
        let lets = Steps()
        host.letGo = { f, _ in
            lets.add("let go \(f)")
            throw CoreError.Failed(message: "Seedwell could not keep “late.txt”, so the folder stays as it is.")
        }
        host.undoLetGo = { f in lets.add("undo \(f)") }
        await fake.set([FileProviderDomainState(id: hub.id, name: hub.name, enabled: true)])
        let listener = FileProviderListener(host, waitForHost: { done in done.wait(timeout: .now() + 30) })
        let r = await Self.fromCoreThread { listener.removeDomain(folderId: "F") }
        #expect(r.error?.contains("late.txt") == true, "\(String(describing: r.error))")
        #expect(lets.all == ["let go F", "undo F"])
        host.stop()
    }

    /// Fails if a hand-over the system never ends (a stuck extension: `waitForChanges` ignores cancellation) holds the
    /// migration, and so the hub's registration, past the cap.
    @MainActor @Test func aHandOverThatNeverEndsGivesUpAtTheCap() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        host.handOverWait = 50_000_000
        let hub = core.syncHostHubDomain()
        await fake.set([FileProviderDomainState(id: "F", name: "Team", enabled: true)])
        await fake.hold(.handOver)
        host.beginMigration()
        let registering = Task { @MainActor in await host.reconcile([hub]) }
        let done = await Self.within("the hub registered", seconds: 10) { await fake.domains.contains { $0.id == hub.id } }
        await fake.finish(.handOver)
        await registering.value
        #expect(done)
        #expect(await fake.domains.map(\.id) == [hub.id], "the old domain went (keeping what is unsynced)")
        host.stop()
    }

    /// Fails if a folder made on demand elsewhere (the command line, an agent) never has its held files shown
    /// downloaded: the core tells the hub again, and each telling looks at what is on disk again.
    @MainActor @Test func heldFilesAreShownAgainWhenTheCoreTellsTheHubAgain() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        let hub = core.syncHostHubDomain()
        await fake.set([FileProviderDomainState(id: hub.id, name: hub.name, enabled: true)])
        await host.reconcile([hub])
        _ = await Self.within("held files looked at") { await fake.materializedReads == 1 }
        await host.reconcile([hub])
        _ = await Self.within("looked at again") { await fake.materializedReads == 2 }
        #expect(await fake.materializedReads == 2)
        host.stop()
    }

    /// A domain the system refuses to add is tried again by itself, not only when the core's list changes.
    @MainActor @Test func aDomainTheSystemRefusesIsTriedAgain() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        host.addRetries = [20_000_000, 20_000_000, 20_000_000]
        let hub = core.syncHostHubDomain()
        await fake.failAdds(2)
        await host.reconcile([hub])
        for _ in 0..<50 {
            if await fake.domains.contains(where: { $0.id == hub.id }) { break }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        #expect(await fake.domains.map(\.id) == [hub.id])
        host.stop()
    }

    /// Reconciles run one at a time: a burst adds the hub once.
    @MainActor @Test func reconcilesRunInOrder() async throws {
        let (core, fake) = try Self.host()
        let host = FileProviderHost(core: core, domains: fake)
        let hub = core.syncHostHubDomain()
        let burst = (0..<5).map { _ in Task { @MainActor in await host.reconcile([hub]) } }
        for t in burst { await t.value }
        #expect(await fake.adds == 1)
        host.stop()
    }

    /// A full folder in the hub is a link: fails if the extension's item for it is not a symbolic link to its place
    /// (the Finder would show placeholders for files that are on this Mac), or may be changed from the Finder.
    @Test func aFullFolderIsALinkItem() {
        let link = FPProviderItem(FPItem(id: "F", parent: "root", name: "Music", kind: "symlink", symlinkTarget: "/Volumes/Disk/Music", caps: ["read"]))
        #expect(link.contentType == .symbolicLink && link.symlinkTargetPath == "/Volumes/Disk/Music")
        #expect(link.capabilities == [.allowsReading] && link.parentItemIdentifier == .rootContainer)
        let folder = FPProviderItem(FPItem(id: "F:root", parent: "root", name: "Team", kind: "dir", caps: ["read", "write"]))
        #expect(folder.itemIdentifier.rawValue == "F:root" && folder.contentType == .folder)
        #expect(!folder.capabilities.contains(.allowsRenaming) && !folder.capabilities.contains(.allowsDeleting))
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
        var form = syncAddFormNew()
        #expect(!form.onDemand, "full unless chosen")
        form.onDemand = true
        #expect(syncOnDemandLabel() == "Files on demand")
        #expect(syncOnDemandNote(change: .join, placeShown: "").contains("until you open it"))
        #expect(syncOnDemandNote(change: .add, placeShown: "").contains("nothing is moved"))
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
        #expect(m.changed && m.onDemandNote?.contains("nothing is moved") == true)
        // An on-demand folder switched back: everything is downloaded, into its place.
        _ = try await core.syncSetOnDemand(id: f.id, onDemand: true, path: nil)
        let back = SyncEditModel(core: core, id: f.id)
        #expect(back.onDemand && back.onDemandNote?.contains("Every file is listed") == true)
        back.onDemand = false
        #expect(back.onDemandNote?.contains("downloaded to this Mac again, into \(Notifier.abbreviate(f.path))") == true)
        #expect(syncPlaceLine(folder: core.syncSnapshot().folders.first { $0.id == f.id }!, pathShown: "x") == "In the Finder’s sidebar, under Locations")
    }

    /// A folder switched to on demand has its files here: they are asked for under the hub's identifiers, so the
    /// Finder shows them downloaded; what the system has on disk already, and what is online only, is not. Fails if a
    /// held file is asked for under its folder's own id (the hub would not know it) or not at all.
    @MainActor @Test func filesHeldHereAreShownDownloaded() async throws {
        let (core, fake) = try Self.host()
        var folders: [SyncFolder] = []
        for _ in 0..<100 where folders.isEmpty {
            folders = core.syncSnapshot().folders
            if folders.isEmpty { try await Task.sleep(nanoseconds: 50_000_000) }
        }
        let f = try await core.syncSetOnDemand(id: try #require(folders.first).id, onDemand: true, path: nil)
        // The server, in a temporary folder (no domain is registered: the domains are fake).
        let dir = try FakeApp.tempDir()
        defer { core.syncHostStop(); try? FileManager.default.removeItem(at: dir) }
        _ = try core.syncHostStart(dir: dir.path, listener: QuietListener(), mode: .hub)
        let host = FileProviderHost(core: core, domains: fake)
        let hub = core.syncHostHubDomain()
        await fake.set([FileProviderDomainState(id: hub.id, name: hub.name, enabled: true)])
        await fake.setOnDisk([core.syncHostHubItem(id: f.id), "\(f.id):m:notes.txt"])
        await host.reconcile([hub])
        for _ in 0..<100 {
            if await fake.requested.count >= 2 { break }
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        // The mock's held files: plan.md (downloaded), draft.pages (only here); notes.txt is on disk already.
        #expect(Set(await fake.requested) == ["\(f.id):m:Projects/plan.md", "\(f.id):m:Projects/draft.pages"])
        // Once a run: checking again asks for nothing more.
        await host.refreshEnabled()
        try await Task.sleep(nanoseconds: 100_000_000)
        #expect(await fake.requested.count == 2)
        host.stop()
    }

    /// The hub through the core's server, as the extension asks: fails if a full folder is not a link to its absolute
    /// place, or an on-demand folder's items are not under the hub's identifiers.
    @Test func theHubListsEveryFolder() async throws {
        let (core, _) = try Self.host()
        var folders: [SyncFolder] = []
        for _ in 0..<100 where folders.isEmpty {
            folders = core.syncSnapshot().folders
            if folders.isEmpty { try await Task.sleep(nanoseconds: 50_000_000) }
        }
        let f = try await core.syncSetOnDemand(id: try #require(folders.first).id, onDemand: true, path: nil)
        let dir = try FakeApp.tempDir()
        defer { core.syncHostStop(); try? FileManager.default.removeItem(at: dir) }
        _ = try core.syncHostStart(dir: dir.path, listener: QuietListener(), mode: .hub)
        let hub = core.syncHostHubDomain()
        var req = FPRequest("children", folder: hub.id, item: "root")
        req.limit = 500
        let top = try FPClient(dir: dir, gate: nil).ask(req, as: FPPage.self).get().items
        #expect(top.count == core.syncSnapshot().folders.count)
        let od = try #require(top.first { $0.id == core.syncHostHubItem(id: f.id) })
        #expect(od.kind == "dir" && od.parent == "root")
        for link in top where link.kind == "symlink" {
            #expect(link.symlinkTarget?.hasPrefix("/") == true && link.caps == ["read"])
        }
        let kids = try FPClient(dir: dir, gate: nil).ask(FPRequest("children", folder: hub.id, item: od.id), as: FPPage.self).get().items
        #expect(!kids.isEmpty && kids.allSatisfy { $0.id.hasPrefix("\(f.id):") && $0.parent == od.id })
    }

    /// Waits until `ok` holds, polling, at most `seconds`: when it does not, an issue names `what` (no wait in a test
    /// is unbounded: a signal that never comes fails the test instead of hanging the run).
    @MainActor static func within(_ what: String, seconds: Double = 30, _ ok: () async -> Bool) async -> Bool {
        let end = Date().addingTimeInterval(seconds)
        while Date() < end {
            if await ok() { return true }
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
        Issue.record("timed out after \(Int(seconds)) s waiting for: \(what)")
        return false
    }

    /// Calls the listener as the core's server does: from a thread of its own. Never from a thread of Swift's
    /// cooperative pool: the call blocks its thread until the host answers, and the host's work (the main actor, the
    /// fake's actor) runs on that pool. CI's Mac has three such threads; three tests blocking one each hung the run.
    static func fromCoreThread(_ call: @escaping @Sendable () -> SyncHostRemoval) async -> SyncHostRemoval {
        await withCheckedContinuation { c in Thread { c.resume(returning: call()) }.start() }
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
    /// What happened, in order: `wait <id>`, `remove <id>`, `add <id>`.
    private(set) var steps: [String] = []

    func set(_ d: [FileProviderDomainState]) { domains = d }

    func enable(_ id: String) {
        for i in domains.indices where domains[i].id == id { domains[i].enabled = true }
    }

    func list() async throws -> [FileProviderDomainState] { domains }

    private(set) var adds = 0
    /// Hand-overs waited for: `<domain>/<item>`.
    private(set) var waited: [String] = []
    private var preserved: URL?
    func setPreserved(_ u: URL?) { preserved = u }
    func waitForChanges(id: String, below item: String) async throws {
        waited.append("\(id)/\(item)")
        steps.append("wait \(id)")
        await pass(.handOver)
    }

    /// The system's steps a test holds: a held step waits for `finish` (30 s at most), and `hasStarted` says when it
    /// began. All in the actor, so the step and the test meet in a fixed order whatever the scheduling.
    enum Step { case handOver, signal }
    private struct Gate {
        var held = false
        var started = false
        var waiting: CheckedContinuation<Void, Never>?
    }
    private var gates: [Step: Gate] = [:]
    func hold(_ step: Step) { gates[step, default: Gate()].held = true }
    func hasStarted(_ step: Step) -> Bool { gates[step]?.started == true }
    func finish(_ step: Step) {
        gates[step, default: Gate()].held = false
        gates[step]?.waiting?.resume()
        gates[step]?.waiting = nil
    }
    private func pass(_ step: Step) async {
        gates[step, default: Gate()].started = true
        guard gates[step]?.held == true else { return }
        // Let go after 30 s whatever the test does (one that failed before it said so).
        Task {
            try? await Task.sleep(nanoseconds: 30_000_000_000)
            self.finish(step)
        }
        await withCheckedContinuation { gates[step]?.waiting = $0 }
    }

    private var failing = 0
    func failAdds(_ n: Int) { failing = n }
    private var stuck: Set<String> = []
    func failRemovals(of ids: Set<String>) { stuck = ids }
    private(set) var removals = 0

    func add(id: String, name: String) async throws {
        if failing > 0 {
            failing -= 1
            throw CocoaError(.fileWriteUnknown)
        }
        adds += 1
        steps.append("add \(id)")
        await Task.yield()
        if let i = domains.firstIndex(where: { $0.id == id }) { domains[i].name = name } else { domains.append(FileProviderDomainState(id: id, name: name, enabled: false)) }
    }

    func remove(id: String) async throws -> URL? {
        removals += 1
        steps.append("remove \(id)")
        if stuck.contains(id) { throw CocoaError(.fileWriteUnknown) }
        domains.removeAll { $0.id == id }
        defer { preserved = nil }
        return preserved
    }

    func signal(id: String) async throws { await pass(.signal) }

    /// Where the Finder shows the items (one path for all of them).
    private var visible: String?
    func setVisible(_ path: String?) { visible = path }
    func itemURL(id: String, item: String) async throws -> URL? { visible.map { URL(fileURLWithPath: $0) } }

    /// What the system has on disk, and what it was asked to download.
    var onDisk: [String] = []
    private(set) var requested: [String] = []
    func setOnDisk(_ ids: [String]) { onDisk = ids }
    private(set) var materializedReads = 0
    func materialized(id: String) async throws -> [String] {
        materializedReads += 1
        return onDisk
    }
    func requestDownload(id: String, item: String) async throws { requested.append(item) }
}

/// Steps recorded from any thread (the host's let-go runs off the main actor).
final class Steps: @unchecked Sendable {
    private let lock = NSLock()
    private var steps: [String] = []
    var all: [String] { lock.withLock { steps } }
    func add(_ s: String) { lock.withLock { steps.append(s) } }
}

/// A listener that hears nothing (a server started by a test).
final class QuietListener: SyncHostListener, @unchecked Sendable {
    func feedMoved(folderId: String) {}
    func domains(domains: [SyncHostDomain]) {}
    func verifyPeer(auditToken: Data) -> Bool { true }
    func removeDomain(folderId: String) -> SyncHostRemoval { SyncHostRemoval(error: nil) }
    func notice(title: String, text: String) {}
    func navPane(entry: SyncNavPane) {}
}

/// Only the app's own extension may use the socket.
@Suite(.timeLimit(.minutes(1))) struct PeerVerifierTests {
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
