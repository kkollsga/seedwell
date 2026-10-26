import Testing
@testable import SeedwellApp

/// Folder sync in the app (SPEC 7G.5, 8.2 item 5): the words of the rows and the menu line, the menu's structure,
/// pasted keys, the delete confirmation and the add modal's Start.
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

    static func link(_ role: SyncRole, _ name: String? = nil, id: String = "MFZWI3D-BONSGYC", mine: Bool = false, expired: Bool = false,
                     chat: String? = nil, inChat: Bool = false) -> SyncLinkInfo {
        SyncLinkInfo(role: role, name: name, folderId: id, alreadySynced: mine, expiresS: nil, expired: expired, channelName: chat,
                     channelJoined: inChat)
    }

    @Test func pastedLinksAreShownForWhatTheyAre() {
        let rw = Self.link(.readWrite, "Photos")
        #expect(SyncText.linkLine("seedwell://sync/ABC", info: rw) == "Read & write · Photos")
        #expect(SyncText.linkLine("seedwell://sync/ABC", info: Self.link(.readWrite, "Photos", chat: "Family"))
                == "Read & write · Photos · with the channel “Family”", "one link joins the folder and its chat")
        #expect(SyncText.linkLine("seedwell://sync/ABC", info: Self.link(.readOnly, chat: "Family", inChat: true))
                == "Read only · with the channel “Family” (joined already)")
        let ro = Self.link(.readOnly)
        #expect(SyncText.linkLine("seedwell://sync/ABC", info: ro) == "Read only")
        #expect(SyncText.suggestedName(ro) == "MFZWI3D")
        #expect(SyncText.linkLine("hello", info: nil) == "Not a Seedwell folder link")
        #expect(SyncText.linkLine("SWW-ABCDEF", info: nil) == "Folders are shared with links now: ask for a seedwell:// link")
        #expect(SyncText.linkLine("  ", info: nil) == "")
        #expect(SyncText.linkLine("seedwell://sync/9ABC", info: nil, problem: "this link was made by a newer Seedwell: update Seedwell to use it.")
                == "This link was made by a newer Seedwell: update Seedwell to use it", "the core's reason, as a line")
        #expect(SyncText.linkLine("x", info: Self.link(.readWrite, mine: true)) == "This folder is already synced on this Mac")
        #expect(SyncText.linkLine("x", info: Self.link(.readWrite, expired: true)) == "This link has expired: ask for a new one")
        #expect(SyncText.looksLikeLink(" seedwell://sync/ABC?name=x\n"))
        #expect(!SyncText.looksLikeLink("SWR-abc"), "bare keys are not links")
        #expect(!SyncText.looksLikeLink("https://example.com"))
        #expect(!SyncText.looksLikeLink("magnet:?xt=urn:btih:abc"))
    }

    @Test func invitationLinksSayWhatIsLeft() {
        let now: UInt64 = 1_800_000_000
        func invite(_ uses: UInt32, of max: UInt32, expires: UInt64? = nil, revoked: Bool = false, chat: String? = nil) -> SyncInvite {
            SyncInvite(id: "I", link: "seedwell://sync/I", role: .readWrite, maxUses: max, uses: uses, expiresS: expires, createdS: now - 60,
                       revoked: revoked, channelId: chat.map { "C-\($0)" }, channelName: chat)
        }
        #expect(SyncText.inviteState(invite(0, of: 1), now: now) == .open)
        #expect(SyncText.inviteLine(invite(0, of: 1), now: now) == "Read & write · 0 of 1 used")
        #expect(SyncText.inviteState(invite(1, of: 1), now: now) == .used, "a single-use link is spent once used")
        #expect(SyncText.inviteLine(invite(1, of: 1), now: now) == "Read & write · Used")
        #expect(SyncText.inviteLine(invite(3, of: 0), now: now) == "Read & write · 3 uses, unlimited")
        #expect(SyncText.inviteState(invite(0, of: 1, expires: now), now: now) == .expired)
        #expect(SyncText.inviteState(invite(0, of: 1, revoked: true), now: now) == .revoked)
        #expect(SyncText.inviteDetail(invite(0, of: 5, expires: now + 86_400)).hasPrefix("Link until "))
        #expect(SyncText.inviteDetail(invite(0, of: 1)) == "", "no dates, no line")
        #expect(SyncText.inviteDetail(invite(0, of: 1, chat: "Family")) == "with the channel “Family”")
    }

    @Test func theShareDialogSaysWhatTheNextLinkDoes() {
        #expect(SyncText.shareSummary(role: .readWrite, uses: 1, linkDays: 7) == "One Mac can join with this link within 7 days, as Read & write.")
        #expect(SyncText.shareSummary(role: .readOnly, uses: 0, linkDays: 0) == "Any number of Macs can join with this link, as Read only.")
        #expect(SyncText.shareSummary(role: .admin, uses: 5, linkDays: 1) == "Up to 5 Macs can join with this link within 1 day, as Admin.")
        #expect(SyncText.shareSummary(role: .readWrite, uses: 1, linkDays: 7, chat: "Photos")
                == "One Mac can join with this link within 7 days, as Read & write. It joins the channel “Photos” too.")
        #expect(SyncText.sharingLine(until: nil, owner: false) == "Shared until the owner stops it.")
        let t: UInt64 = 1_800_000_000
        #expect(SyncText.countdown(until: t + 6 * 86_400 + 23 * 3_600 + 59 * 60, now: t) == "Sharing ends in 6 days, 23 hours")
        #expect(SyncText.countdown(until: t + 5 * 3_600 + 12 * 60, now: t) == "Sharing ends in 5 hours, 12 minutes")
        #expect(SyncText.countdown(until: t + 30, now: t) == "Sharing ends in 1 minute")
        #expect(SyncText.countdown(until: t, now: t) == "Sharing has ended")
        #expect(SyncText.sharingLine(until: 1_800_000_000, owner: true).hasPrefix("When it ends ("))
    }

    @Test func rolesDecideWhoSharesAndMembersAreListedMineFirstThenByRole() {
        #expect(SyncText.canShare(.owner) && SyncText.canShare(.admin))
        #expect(!SyncText.canShare(.readWrite) && !SyncText.canShare(.readOnly))
        let m = { (name: String, role: SyncRole, me: Bool) in SyncMember(deviceId: name, name: name, role: role, online: true, isMe: me, expiresS: nil) }
        let ordered = SyncText.ordered([m("Zed", .readOnly, false), m("Bo", .admin, false), m("Me", .readWrite, true), m("Al", .owner, false), m("Ann", .admin, false)])
        #expect(ordered.map(\.name) == ["Me", "Al", "Ann", "Bo", "Zed"])
        #expect(SyncText.memberLine(m("Me", .readWrite, true), device: nil) == "", "this Mac's role is on the right already")
        #expect(SyncText.memberLine(SyncMember(deviceId: "X", name: "X", role: .readOnly, online: false, isMe: false, expiresS: nil), device: nil) == "Offline")
        let guest = SyncMember(deviceId: "G", name: "G", role: .readWrite, online: false, isMe: false, expiresS: 1_800_000_000)
        #expect(SyncText.memberLine(guest, device: nil).hasPrefix("Offline · until "), "time-limited access shows its end")
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
        #expect(!f.canStart && f.keyLine == "Not a Seedwell folder link" && f.location.isEmpty)
        f.setKey("seedwell://sync/x", info: Self.link(.readWrite, "Photos", id: "F"), defaultLocation: "/Users/me/Seedwell/Photos")
        #expect(f.canStart && f.location == "/Users/me/Seedwell/Photos")
        // A location the user chose stays when the link changes.
        f.chooseParent("/Volumes/Disk")
        #expect(f.location == "/Volumes/Disk/Photos" && f.locationChosen)
        f.setKey("seedwell://sync/y", info: Self.link(.readOnly, "Other", id: "G"), defaultLocation: "/Users/me/Seedwell/Other")
        #expect(f.location == "/Volumes/Disk/Photos")
        f.setKey("seedwell://sync/z", info: Self.link(.readWrite, id: "H", mine: true), defaultLocation: nil)
        #expect(!f.canStart, "already synced")
        f.setKey("seedwell://sync/z", info: Self.link(.readWrite, id: "H", expired: true), defaultLocation: nil)
        #expect(!f.canStart, "an expired link admits nobody")
        #expect(SyncEditModel.nameProblem(" ") != nil)
        #expect(SyncEditModel.nameProblem("a/b") != nil)
        #expect(SyncEditModel.nameProblem(".x") != nil)
        #expect(SyncEditModel.nameProblem("Photos 2026") == nil)
    }

    @Test func joiningOffersAFolderOfThatNameThatIsThereAlready() {
        let there: (String) -> Bool = { $0 == "/Users/me/Seedwell/Team" }
        let next = "/Users/me/Seedwell/Team 2"
        #expect(SyncAddForm.existing(name: "Team", next: next, synced: [], isFolder: there) == "/Users/me/Seedwell/Team")
        #expect(SyncAddForm.existing(name: "Team", next: next, synced: ["/Users/me/Seedwell/Team"], isFolder: there) == nil,
                "a folder synced already is not offered")
        #expect(SyncAddForm.existing(name: "Team", next: "/Users/me/Seedwell/Team", synced: [], isFolder: there) == nil,
                "the name was free: nothing to offer")
        #expect(SyncAddForm.existing(name: "Team", next: next, synced: [], isFolder: { _ in false }) == nil)
        let same: (String) -> String = { $0 }
        #expect(SyncText.locationNote(next, existing: nil, abbreviate: same) == nil)
        #expect(SyncText.locationNote(next, existing: "~/Seedwell/Team", abbreviate: same)
                == "~/Seedwell/Team exists already. Choose it in Location to sync into it instead of a new folder.")
        #expect(SyncText.locationNote("~/Seedwell/Team", existing: "~/Seedwell/Team", abbreviate: same)?.hasPrefix("~/Seedwell/Team exists already. The files in it stay") == true)
    }

    @Test func theShareMenuSharesAFolderNotSyncedYetInOneStep() {
        let t = ShareTarget(newFolder: "/Users/me/Novel")
        #expect(t.name == "Novel" && t.role == .owner && !t.isChannel && t.id.isEmpty && t.shareUntilS == nil)
        #expect(SyncText.shareNewNote("~/Novel")
                == "~/Novel is not synced yet. Copy Link starts syncing it from this Mac, with you as its owner, and makes the link.")
    }
}
