import Foundation
import Testing
@testable import SeedwellApp

/// Folder sync in the app (SPEC 7G.5, 8.2 item 5). The words, states and the submenu are the core's (trs-ffi
/// `sync_text.rs`, `sync_menu.rs`, tested there); these check that the app reaches them, and what stays here: the date
/// in the user's style and the Share dialog's target.
@Suite struct SyncTests {
    static func folder(_ name: String = "Photos", _ state: SyncState = .upToDate, id: String? = nil, bytes: UInt64 = 1_000_000_000,
                       need: UInt64 = 0, rate: UInt64 = 0, connected: UInt32 = 0, conflicts: UInt32 = 0,
                       access: SyncAccess = .readWrite, role: SyncRole = .owner) -> SyncFolder {
        SyncFolder(id: id ?? "ID-\(name)", name: name, path: "/Users/me/Seedwell/\(name)", access: access, role: role, shareUntilS: nil, state: state,
                   files: 10, bytes: bytes, needFiles: need > 0 ? 3 : 0, needBytes: need, rateIn: rate, rateOut: 0,
                   connected: connected, members: connected, conflicts: conflicts, lanOnly: false, issues: [], error: nil, lastSyncedS: nil,
                   channelId: nil)
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
        let guest = SyncMember(deviceId: "G", name: "G", user: "", role: .readWrite, online: false, isMe: false, expiresS: 1_800_000_000)
        #expect(SyncText.memberLine(guest, device: nil) == "Offline · until \(Fmt.day(1_800_000_000))", "the day in the user's style")
        #expect(SyncText.linkLine("hello", info: nil) == "Not a Seedwell folder link")
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
        let m = menuModel(snapshot: Snapshot.empty, sync: ok, arrivals: Arrivals(), nowUnixMs: 0, updateVersion: nil)
        #expect(m.folderSync?.title == "Folder Sync" && m.folderSync?.subtitle == "3 folders · Up to date")
        #expect(menuModel(snapshot: Snapshot.empty, sync: Self.snap([]), arrivals: Arrivals(), nowUnixMs: 0, updateVersion: nil).folderSync == nil)
    }

    @Test func theSubmenuDrawsTheCoresModel() {
        let removal = SyncRemoval()
        removal.toggle(id: "ID-B")
        let m = syncMenuModel(snapshot: Self.snap([Self.folder("A"), Self.folder("B")]), removal: removal)
        #expect(m.rows.map(\.title) == ["A", "B"] && m.rows.map(\.confirming) == [false, true])
        #expect(m.confirm?.question == "Stop syncing “B” on this Mac? Other devices keep their copy.")
        #expect(m.add.title == "Add Folder…" && m.add.help == "Sync a folder of this Mac, or one shared with you")
    }

    static func link(_ role: SyncRole, _ name: String? = nil, id: String = "MFZWI3D-BONSGYC", mine: Bool = false, expired: Bool = false,
                     chat: String? = nil, inChat: Bool = false) -> SyncLinkInfo {
        SyncLinkInfo(role: role, name: name, folderId: id, alreadySynced: mine, expiresS: nil, expired: expired, channelName: chat,
                     channelJoined: inChat)
    }

    @Test func theAddModalsFormIsTheCores() {
        var f = SyncAddForm()
        #expect(f.mode == .local && !f.canStart)
        f.folder = "/Users/me/Novel"
        #expect(f.canStart)
        f.mode = .shared
        f.setKey("seedwell://sync/x", info: Self.link(.readWrite, "Photos", id: "F"), defaultLocation: "/Users/me/Seedwell/Photos")
        #expect(f.canStart && f.keyUsable && f.keyLine == "Read & write · Photos" && f.location == "/Users/me/Seedwell/Photos")
        f.chooseParent("/Volumes/Disk")
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

    @Test func theShareMenuSharesAFolderNotSyncedYetInOneStep() {
        let t = ShareTarget(newFolder: "/Users/me/Novel")
        #expect(t.name == "Novel" && t.role == .owner && !t.isChannel && t.id.isEmpty && t.shareUntilS == nil)
    }
}
