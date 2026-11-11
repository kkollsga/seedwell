import Foundation
import SwiftUI
import Testing
@testable import SeedwellApp

/// Folder sync in the app (SPEC 7G.5, 8.2 item 5). The words, states and the submenu are the core's (seedwell-ffi
/// `sync_text.rs`, `sync_menu.rs`, tested there); these check that the app reaches them, and what stays here: the date
/// in the user's style and the Share dialog's target.
@Suite(.timeLimit(.minutes(1))) struct SyncTests {
    /// Until the Share dialog is drawn as it is now (its links read and drawn). `ready()`'s own limit (5 s, for the
    /// window and the renderer) passes first in a full parallel run (one pool thread, the main thread shared: 5 to 9 s
    /// there), and a test then counted the drawing that came after it as one "shown again" (2026-11-07).
    @MainActor func untilDrawn(_ m: SyncShareModel) async throws {
        await m.ready(within: .seconds(30))
        try #require(m.isReady, "the Share dialog was not drawn within 30 s")
    }

    static func folder(_ name: String = "Photos", _ state: SyncState = .upToDate, id: String? = nil, bytes: UInt64 = 1_000_000_000,
                       need: UInt64 = 0, rate: UInt64 = 0, connected: UInt32 = 0, conflicts: UInt32 = 0,
                       access: SyncAccess = .readWrite, role: SyncRole = .owner) -> SyncFolder {
        SyncFolder(id: id ?? "ID-\(name)", name: name, path: "/Users/me/Seedwell/\(name)", access: access, role: role, shareUntilS: nil, state: state,
                   files: 10, bytes: bytes, needFiles: need > 0 ? 3 : 0, needBytes: need, rateIn: rate, rateOut: 0,
                   connected: connected, members: connected, conflicts: conflicts, lanOnly: false, issues: [], error: nil, lastSyncedS: nil,
                   channelId: nil, onDemand: false, unavailable: 0, progress: nil, problem: nil, waiting: nil, gone: [], canReshare: false, removesEverywhere: false)
    }

    static func folder(_ state: SyncState, bytes: UInt64 = 1_000_000_000, need: UInt64 = 0, rate: UInt64 = 0,
                       connected: UInt32 = 0, conflicts: UInt32 = 0) -> SyncFolder {
        folder("Photos", state, bytes: bytes, need: need, rate: rate, connected: connected, conflicts: conflicts)
    }

    static func snap(_ folders: [SyncFolder], running: Bool = true) -> SyncSnapshot {
        SyncSnapshot(running: running, problem: nil, deviceId: "D", deviceName: "Mac", port: 22100, folders: folders, devices: [])
    }

    @Test func theAppReachesTheCoresSyncWords() {
        #expect(SyncText.subtitle(Self.folder(connected: 3)) == "Up to date · 3 devices")
        #expect(SyncText.badge(Self.folder(.syncing, need: 500_000_000)) == .progress(fraction: 0.5))
        #expect(SyncText.badge(Self.folder(.missing)).accessibilityWord == "folder missing")
        #expect(SyncText.role(.readWrite) == "Read & write" && SyncText.canShare(.admin) && !SyncText.canShare(.readOnly))
        #expect(SyncText.message(CoreError.NotFound(id: "F")) == "This folder is no longer synced on this Mac.")
        #expect(SyncText.message(CoreError.Failed(message: "Disk full.")) == "Disk full.")
        let guest = SyncMember(deviceId: "G", name: "G", user: "", role: .readWrite, online: false, isMe: false, expiresS: 1_800_000_000, removal: nil, removalWaiting: false)
        #expect(SyncText.memberLine(guest, device: nil) == "Offline · until \(Fmt.day(1_800_000_000))", "the day in the user's style")
        #expect(SyncText.linkLine("hello", info: nil) == "Not a Seedwell folder link")
    }

    @Test func aFolderSaysWhomItWaitsForAndOffersWhatTheUserCanDo() {
        var f = Self.folder()
        f.waiting = "Waiting for Mac mini (last seen 3 days ago: nothing answers at 192.168.68.57:55799)"
        #expect(SyncText.subtitle(f) == f.waiting, "the row and the page show the core's sentence")
        let gone = SyncGoneDevice(deviceId: "G", name: "Mac mini", lastSeenS: UInt64(Date().timeIntervalSince1970) - 3 * 86_400)
        let p = SyncText.gonePrompt(gone, name: f.name, channel: false)
        #expect(p.line == "Mac mini was last seen 3 days ago" && p.button == "Remove from Folder…" && p.title == "Remove Mac mini from “Photos”?")
        #expect(SyncText.gonePrompt(gone, name: "Plans", channel: true).button == "Remove from Channel…")
        let r = SyncText.resharePrompt(f)
        #expect(r.button == "Share Again from This Mac…" && r.title == "Share “Photos” again from this Mac?" && r.confirm == "Share Again")
    }

    @Test func aDayCarriesItsYearOnlyWhenItIsNotThisYear() {
        let now = Date(timeIntervalSince1970: 1_800_000_000)
        let later = Fmt.day(1_800_000_000 + 86_400, now: now)
        let year = Calendar.current.component(.year, from: now)
        #expect(!later.contains(String(year)))
        #expect(Fmt.day(1_800_000_000 + 400 * 86_400, now: now).contains(String(year + 1)))
    }

    @Test func theWindowAndTheMenuShowTheCoresSyncLine() {
        // The rules are the core's (menu.rs); this checks the app reaches them.
        let ok = Self.snap([Self.folder("A"), Self.folder("B"), Self.folder("C")])
        #expect(syncSummary(snapshot: ok) == "3 folders · Up to date")
        let m = menuModel(snapshot: Snapshot.empty, sync: ok, arrivals: Arrivals(), nowUnixMs: 0, updateVersion: nil, host: hostWindowsAll())
        #expect(m.folderSync?.title == "Folder Sync" && m.folderSync?.subtitle == "3 folders · Up to date")
        #expect(menuModel(snapshot: Snapshot.empty, sync: Self.snap([]), arrivals: Arrivals(), nowUnixMs: 0, updateVersion: nil, host: hostWindowsAll()).folderSync == nil)
    }

    @Test func theSubmenuDrawsTheCoresModel() {
        let removal = SyncRemoval()
        removal.toggle(id: "ID-B", onlyHere: 0)
        let m = syncMenuModel(snapshot: Self.snap([Self.folder("A"), Self.folder("B")]), removal: removal, host: hostWindowsAll())
        #expect(m.rows.map(\.title) == ["A", "B"] && m.rows.map(\.confirming) == [false, true])
        #expect(m.confirm?.question == "Stop syncing “B” on this Mac? Other devices keep their copy.")
        #expect(m.add?.title == "Add Folder…" && m.add?.help == "Sync a folder of this Mac, or one shared with you")
    }

    static func link(_ role: SyncRole, _ name: String? = nil, id: String = "MFZWI3D-BONSGYC", mine: Bool = false, expired: Bool = false,
                     chat: String? = nil, inChat: Bool = false) -> SyncLinkInfo {
        SyncLinkInfo(role: role, name: name, folderId: id, alreadySynced: mine, expiresS: nil, expired: expired, channelName: chat,
                     channelJoined: inChat)
    }

    @Test func theAddModalsFormIsTheCores() {
        var f = syncAddFormNew()
        #expect(f.mode == .local && !syncAddCanStart(form: f))
        f.folder = "/Users/me/Novel"
        #expect(syncAddCanStart(form: f))
        f.mode = .shared
        f = syncAddSetKey(form: f, text: "seedwell://sync/x", info: Self.link(.readWrite, "Photos", id: "F"),
                          defaultLocation: "/Users/me/Seedwell/Photos", problem: nil)
        #expect(syncAddCanStart(form: f) && syncAddKeyUsable(form: f) && syncAddKeyLine(form: f) == "Read & write · Photos")
        #expect(f.location == "/Users/me/Seedwell/Photos")
        f = syncAddChooseParent(form: f, parent: "/Volumes/Disk")
        #expect(f.location == "/Volumes/Disk/Photos" && f.locationChosen)
        #expect(syncAddModes().map { syncAddModeLabel(mode: $0) } == ["From this Mac", "Shared with me"])
    }

    /// SwiftUI hands the link field's text back when editing ends and in layout passes, also unchanged: that is not a
    /// new link, and the error of a failed Start stays (it went as soon as the field lost focus).
    @MainActor @Test func theLinkFieldHandingBackItsTextKeepsTheError() throws {
        let dir = NSTemporaryDirectory() + "seedwell-sync-add-\(getpid())"
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let core = try Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
        let m = SyncAddModel(core: core, prefill: "SWW-NOTAKEY")
        m.error = "Folders in iCloud Drive or other cloud storage cannot be synced."
        m.editKey("SWW-NOTAKEY")
        #expect(m.error != nil && m.form.key == "SWW-NOTAKEY")
        m.editKey("seedwell://sync/x")
        #expect(m.error == nil && m.form.key == "seedwell://sync/x", "a new link is a fresh start")
    }

    /// The Add dialog's calls on the main thread never look at the disk: a pasted link's places (where it goes, a folder
    /// of that name there already) are read off it and handed back. The failure this targets: that answer never reaches
    /// the form (the Location stays empty, Start disabled), or an answer for a link pasted before replaces the new one's.
    @MainActor @Test func theAddDialogShowsAPastedLinksPlaceReadOffTheMainThread() async throws {
        let dir = NSTemporaryDirectory() + "seedwell-sync-add-places-\(getpid())"
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let core = try Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
        var settings = core.syncSettings()
        settings.defaultLocation = dir
        try await core.setSyncSettings(settings: settings)
        let folder = try #require(core.syncSnapshot().folders.first { $0.role == .owner })
        let link = try await core.syncCreateInvite(id: folder.id, role: .readWrite, maxUses: 1, validDays: 7, channelId: nil).link
        _ = try await core.syncRemove(id: folder.id, trashFiles: false)
        let m = SyncAddModel(core: core, prefill: link)
        #expect(m.form.placesPending && !m.window.canStart, "the places are read apart")
        await m.ready()
        #expect(!m.form.placesPending && m.window.location.hasPrefix(dir + "/") && m.window.canStart, "\(m.form)")
        // A new link while the first one's places are read: only the new one's are kept.
        m.setKey("hello")
        m.setKey(link)
        m.setKey("SWW-NOTAKEY")
        await m.ready()
        #expect(m.form.key == "SWW-NOTAKEY" && m.window.location.isEmpty && !m.window.canStart, "\(m.form)")
    }

    @MainActor @Test func theShareMenuSharesAFolderNotSyncedYetInOneStep() async throws {
        let dir = NSTemporaryDirectory() + "seedwell-sync-share-\(getpid())"
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let core = try Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
        let m = SyncShareModel(core: core, newFolder: "/Users/me/Novel")
        try await untilDrawn(m)
        let w = try #require(m.window)
        #expect(m.id.isEmpty && w.title == "Share “Novel”" && w.newNote != nil && w.closeTitle == "Cancel" && w.copyTitle == "Copy Link")
    }

    /// The failure this targets: **the Share dialog asks the core for its whole drawing on every SwiftUI render** of its
    /// view, on the main thread (each a snapshot of every folder and every channel's messages counted, 2026-11-06: the
    /// user's app froze while a big folder was added and its Share window was open). Renders of the view must ask the
    /// core nothing; a change of the form is drawn once, off the main thread. Opening, the dialog says so until the
    /// first drawing is there.
    @MainActor @Test func theShareDialogIsDrawnAfterAChangeNeverOnARender() async throws {
        let dir = NSTemporaryDirectory() + "seedwell-sync-share-draws-\(getpid())"
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let core = try Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
        let id = try #require(core.syncSnapshot().folders.first { SyncText.canShare($0.role) }?.id)
        let m = SyncShareModel(core: core, id: id)
        #expect(m.window == nil, "opening: drawn once its form is read, off the main thread")
        try await untilDrawn(m)
        let w = try #require(m.window)
        #expect(w.gone == nil && w.copyTitle != nil, "\(w)")
        let drawn = m.draws
        // SwiftUI draws the view again and again (here: twenty renders, each after a change the view observes).
        let actions = SyncActions(core: core)
        for n in 0..<20 {
            m.copied = n.isMultiple(of: 2)
            let r = ImageRenderer(content: SyncShareView(model: m, actions: actions))
            #expect(r.nsImage != nil)
        }
        #expect(m.draws == drawn, "a render asked the core to draw the dialog: \(m.draws - drawn) times")
        // A change of the form is drawn (once), and the drawing follows it.
        m.form.singleUse = false
        try await untilDrawn(m)
        #expect(m.draws == drawn + 1 && m.window?.singleUse.on == false, "\(m.draws - drawn) drawings")
    }

    /// The failure this targets: **an open Share dialog keeps a stale drawing when its folder changes elsewhere** (Read
    /// only switched in another window or on the owner's other device, the folder removed): nothing in the dialog
    /// changes, and it was drawn after a change only. While it is open it reads the folder again as often as the core
    /// asks, off the main thread, and shows a drawing only when it differs (a refresh that finds nothing is not drawn).
    @MainActor @Test func anOpenShareDialogSeesAChangeMadeElsewhere() async throws {
        let dir = NSTemporaryDirectory() + "seedwell-sync-share-refresh-\(getpid())"
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let core = try Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
        let id = try #require(core.syncSnapshot().folders.first { $0.role == .owner }?.id)
        let m = SyncShareModel(core: core, id: id)
        // The core's minute, shortened: the test waits for the refresh, not for the clock.
        m.refreshAfter = { _ in .milliseconds(20) }
        try await untilDrawn(m)
        let w = try #require(m.window)
        #expect(w.refreshMs != nil && w.readOnly?.on == false, "\(w)")
        func until(_ what: String, _ done: () -> Bool) async throws {
            // 30 s, as the opening: on a loaded CI runner (191 tests at once on a few cores) every read waits its turn for a
            // thread, and three rounds have taken longer than 10 s. A dialog that never reads again still fails here.
            let start = ContinuousClock.now
            while !done(), ContinuousClock.now - start < .seconds(30) { try await Task.sleep(for: .milliseconds(10)) }
            #expect(done(), "\(what): \(String(describing: m.window))")
        }
        // Nothing changed: read again, nothing new shown.
        let (draws, shown) = (m.draws, m.shown)
        try await until("never read again") { m.draws >= draws + 3 }
        #expect(m.shown == shown, "an unchanged drawing was shown again: \(m.shown - shown) times")
        // Read only switched elsewhere: the open dialog shows it.
        _ = try await core.syncShareSetReadOnly(form: core.syncShareForm(id: id), on: true)
        try await until("Read only switched elsewhere never reached the dialog") { m.window?.readOnly?.on == true }
        // The folder removed: the dialog says it is gone.
        _ = try await core.syncRemove(id: id, trashFiles: false)
        try await until("the folder removed never reached the dialog") { m.window?.gone != nil }
    }

    /// A Share window shown while it was opening grows to its content from a corner; centring it again then is right
    /// unless the user moved it meanwhile. The failure this targets: the window left below centre, or one the user moved
    /// pulled back.
    @Test func aShareWindowIsCentredAgainOnlyIfTheUserDidNotMoveIt() {
        let placed = NSRect(x: 500, y: 400, width: 460, height: 160)
        #expect(SyncWindows.unmoved(NSRect(x: 500, y: 140, width: 460, height: 420), placed: placed), "grown from its top-left corner")
        #expect(SyncWindows.unmoved(NSRect(x: 500, y: 400, width: 460, height: 420), placed: placed), "grown from its bottom-left corner")
        #expect(!SyncWindows.unmoved(NSRect(x: 620, y: 300, width: 460, height: 160), placed: placed), "moved by the user")
    }

    /// The page draws the core's model: what the Mac passes keeps every item.
    @MainActor @Test func theFolderSyncPageIsTheCores() throws {
        let dir = NSTemporaryDirectory() + "seedwell-sync-page-\(getpid())"
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let core = try Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
        let m = LibraryModel(core: core, section: .folderSync)
        let page = m.folderSyncPage(onDemandOff: { _ in false })
        let p = try #require(page.page)
        #expect(page.add?.title == "Add Folder…" && page.selected == p.folderId)
        #expect(p.buttons.map(\.title).contains("Edit…") && p.buttons.last?.title == "Stop Syncing…")
        m.askSyncRemove(p.folderId)
        #expect(m.folderSyncPage(onDemandOff: { _ in false }).page?.removal?.removeTitle == "Remove")
        m.setSyncTrash(true)
        #expect(m.syncRemovalTrash && m.folderSyncPage(onDemandOff: { _ in false }).page?.removal?.trash == true)
        m.cancelSyncRemove()
        #expect(m.folderSyncPage(onDemandOff: { _ in false }).page?.removal == nil)
    }
}
