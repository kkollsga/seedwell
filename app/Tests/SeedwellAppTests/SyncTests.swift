import Testing
@testable import SeedwellApp

/// Folder sync in the app (SPEC 7G.5, 8.2 item 5): the words of the rows and the menu line, the menu's structure,
/// pasted keys, the delete confirmation and the add modal's Start.
@Suite struct SyncTests {
    static func folder(_ name: String = "Photos", _ state: SyncState = .upToDate, id: String? = nil, bytes: UInt64 = 1_000_000_000,
                       need: UInt64 = 0, rate: UInt64 = 0, connected: UInt32 = 0, conflicts: UInt32 = 0,
                       access: SyncAccess = .readWrite) -> SyncFolder {
        SyncFolder(id: id ?? "ID-\(name)", name: name, path: "/Users/me/Seedwell/\(name)", access: access, state: state,
                   files: 10, bytes: bytes, needFiles: need > 0 ? 3 : 0, needBytes: need, rateIn: rate, rateOut: 0,
                   connected: connected, members: connected, conflicts: conflicts, lanOnly: false, issues: [], error: nil, lastSyncedS: nil)
    }

    static func folder(_ state: SyncState, bytes: UInt64 = 1_000_000_000, need: UInt64 = 0, rate: UInt64 = 0,
                       connected: UInt32 = 0, conflicts: UInt32 = 0) -> SyncFolder {
        folder("Photos", state, bytes: bytes, need: need, rate: rate, connected: connected, conflicts: conflicts)
    }

    static func snap(_ folders: [SyncFolder], running: Bool = true) -> SyncSnapshot {
        SyncSnapshot(running: running, problem: nil, deviceId: "D", deviceName: "Mac", port: 22100, folders: folders, devices: [])
    }

    @Test func rowSubtitlesFollowTheState() {
        #expect(SyncText.subtitle(Self.folder(connected: 3)) == "Up to date · 3 devices")
        #expect(SyncText.subtitle(Self.folder(connected: 1)) == "Up to date · 1 device")
        #expect(SyncText.subtitle(Self.folder()) == "Up to date")
        #expect(SyncText.subtitle(Self.folder(.syncing, need: 580_000_000, rate: 12_000_000)) == "Syncing · 42% · 12 MB/s")
        #expect(SyncText.subtitle(Self.folder(.syncing, bytes: 0, need: 0)) == "Syncing", "nothing counted yet")
        #expect(SyncText.subtitle(Self.folder(.syncing, need: 1)) == "Syncing · 99%", "never 100 % while syncing")
        #expect(SyncText.subtitle(Self.folder(.waitingForDevices)) == "Waiting for devices")
        #expect(SyncText.subtitle(Self.folder(.missing)) == "Folder missing")
        #expect(SyncText.subtitle(Self.folder(.paused, connected: 2)) == "Paused")
        #expect(SyncText.subtitle(Self.folder(connected: 2, conflicts: 1)) == "1 conflict · 2 devices")
        #expect(SyncText.badge(Self.folder(connected: 2, conflicts: 1)) == .attention)
        #expect(SyncText.badge(Self.folder(.syncing, need: 500_000_000)) == .progress(0.5))
        #expect(SyncText.badge(Self.folder(.missing)) == .missing)
        #expect(SyncText.badge(Self.folder(.paused)) == .paused)
        #expect(SyncText.badge(Self.folder()) == .upToDate)
    }

    @Test func theMenuLineSaysWhatNeedsTheUserFirst() {
        let ok = [Self.folder("A"), Self.folder("B"), Self.folder("C")]
        #expect(SyncText.summary(Self.snap(ok)) == "3 folders · Up to date")
        let syncing = [Self.folder("A", .syncing, rate: 7_000_000), Self.folder("B", .syncing, rate: 5_000_000), Self.folder("C")]
        #expect(SyncText.summary(Self.snap(syncing)) == "Syncing 2 · 12 MB/s")
        #expect(SyncText.summary(Self.snap(syncing + [Self.folder("D", conflicts: 1)])) == "1 conflict")
        #expect(SyncText.summary(Self.snap(syncing + [Self.folder("D", conflicts: 1), Self.folder("E", .missing)])) == "1 folder missing")
        #expect(SyncText.summary(Self.snap([])) == "No folders")
        #expect(SyncText.summary(Self.snap([], running: false)) == "Starting…")
        #expect(SyncText.needsAttention(Self.snap([Self.folder("E", .missing)])))
        #expect(!SyncText.needsAttention(Self.snap(ok)))
    }

    @Test func theMenuSignatureHasTheLineAndTheSubmenuItsFolders() {
        let starting = Self.snap([], running: false)
        #expect(!StatusMenuController.showsSync(starting))
        #expect(StatusMenuController.syncSignature(starting) == "")
        // The line appears once sync runs: the menu is rebuilt.
        #expect(StatusMenuController.syncSignature(Self.snap([])) == "sync")
        #expect(StatusMenuController.syncSignature(Self.snap([Self.folder()])) == StatusMenuController.syncSignature(Self.snap([])),
                "folders are the submenu's, not the menu's structure")
        // The submenu rebuilds for a folder added, removed or renamed; a state change only updates the rows.
        let one = MenuSync.structureOf(Self.snap([Self.folder("A")]))
        #expect(MenuSync.structureOf(Self.snap([Self.folder("A", .syncing, need: 5)])) == one)
        #expect(MenuSync.structureOf(Self.snap([Self.folder("A"), Self.folder("B")])) != one)
        #expect(MenuSync.structureOf(Self.snap([Self.folder("A2", id: "ID-A")])) != one)
    }

    @Test func pastedKeysAreShownForWhatTheyAre() {
        let rw = SyncLinkInfo(access: .readWrite, name: "Photos", folderId: "MFZWI3D-BONSGYC", alreadySynced: false)
        #expect(SyncText.linkLine("seedwell://sync/SWW-…", info: rw) == "Read & write · Photos")
        let ro = SyncLinkInfo(access: .readOnly, name: nil, folderId: "MFZWI3D-BONSGYC", alreadySynced: false)
        #expect(SyncText.linkLine("SWR-…", info: ro) == "Read only")
        #expect(SyncText.suggestedName(ro) == "MFZWI3D")
        #expect(SyncText.linkLine("hello", info: nil) == "Not a folder key or link")
        #expect(SyncText.linkLine("  ", info: nil) == "")
        var mine = rw
        mine.alreadySynced = true
        #expect(SyncText.linkLine("x", info: mine) == "This folder is already synced on this Mac")
        #expect(SyncText.linkLine("x", info: SyncLinkInfo(access: .encrypted, name: nil, folderId: "X", alreadySynced: false)) == "Encrypted keys are not supported yet")
        #expect(SyncText.looksLikeKey(" seedwell://sync/SWW-ABC?name=x\n"))
        #expect(SyncText.looksLikeKey("swr-abc"))
        #expect(!SyncText.looksLikeKey("https://example.com"))
        #expect(!SyncText.looksLikeKey("magnet:?xt=urn:btih:abc"))
    }

    @Test func oneDeleteConfirmationIsOpenAtATime() {
        var r = SyncRemoval()
        #expect(r.open == nil)
        r.toggle("A")
        #expect(r.isOpen("A"))
        r.trash = true
        r.toggle("B")
        #expect(r.isOpen("B") && !r.isOpen("A"), "another row closes the first")
        #expect(!r.trash, "each opening starts without the Trash")
        r.toggle("B")
        #expect(r.open == nil, "the trash button again closes it")
        r.toggle("C")
        r.keep(only: ["A", "B"])
        #expect(r.open == nil, "a folder removed elsewhere takes its question with it")
        r.toggle("A")
        r.close()  // every opening of the menu
        #expect(r.open == nil)
        #expect(SyncRemoval.question("Photos") == "Stop syncing “Photos” on this Mac? Other devices keep their copy.")
    }

    @Test func theAddModalStartsOnlyWithWhatItNeeds() {
        var f = SyncAddForm()
        #expect(f.mode == .local && !f.canStart)
        f.folder = "/Users/me/Novel"
        #expect(f.canStart)
        f.mode = .shared
        #expect(!f.canStart)
        f.setKey("nonsense", info: nil, defaultLocation: nil)
        #expect(!f.canStart && f.keyLine == "Not a folder key or link" && f.location.isEmpty)
        let rw = SyncLinkInfo(access: .readWrite, name: "Photos", folderId: "F", alreadySynced: false)
        f.setKey("SWW-x", info: rw, defaultLocation: "/Users/me/Seedwell/Photos")
        #expect(f.canStart && f.location == "/Users/me/Seedwell/Photos")
        // A location the user chose stays when the key changes.
        f.chooseParent("/Volumes/Disk")
        #expect(f.location == "/Volumes/Disk/Photos" && f.locationChosen)
        f.setKey("SWW-y", info: SyncLinkInfo(access: .readOnly, name: "Other", folderId: "G", alreadySynced: false), defaultLocation: "/Users/me/Seedwell/Other")
        #expect(f.location == "/Volumes/Disk/Photos")
        f.setKey("SWW-z", info: SyncLinkInfo(access: .readWrite, name: nil, folderId: "H", alreadySynced: true), defaultLocation: nil)
        #expect(!f.canStart, "already synced")
        f.setKey("SWE-z", info: SyncLinkInfo(access: .encrypted, name: nil, folderId: "H", alreadySynced: false), defaultLocation: nil)
        #expect(!f.canStart, "encrypted keys are for later")
        #expect(SyncEditModel.nameProblem(" ") != nil)
        #expect(SyncEditModel.nameProblem("a/b") != nil)
        #expect(SyncEditModel.nameProblem(".x") != nil)
        #expect(SyncEditModel.nameProblem("Photos 2026") == nil)
    }
}
