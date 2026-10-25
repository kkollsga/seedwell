import AppKit
import SeedwellFinderShared
import Testing
@testable import SeedwellApp

/// Finder badges (SPEC 7G.5): a folder's own badge from its state, the badge of each item inside it, and the
/// status file the app and the extension share.
@Suite struct FinderBadgeTests {
    typealias Folder = FinderStatus.Folder

    static func status(_ folders: [Folder], running: Bool = true) -> FinderBadges {
        FinderBadges(FinderStatus(running: running, folders: folders))
    }

    @Test func aFolderBadgeFollowsItsState() {
        #expect(FinderStatusWriter.badge(SyncTests.folder(.upToDate, connected: 2)) == .synced)
        #expect(FinderStatusWriter.badge(SyncTests.folder(.upToDate)) == .offline, "up to date, but no device online")
        #expect(FinderStatusWriter.badge(SyncTests.folder(.waitingForDevices, need: 5)) == .offline)
        #expect(FinderStatusWriter.badge(SyncTests.folder(.syncing, need: 5, rate: 1_000, connected: 1)) == .downloading)
        #expect(FinderStatusWriter.badge(SyncTests.folder(.scanning)) == .syncing)
        #expect(FinderStatusWriter.badge(SyncTests.folder(.paused, connected: 2)) == .paused)
        #expect(FinderStatusWriter.badge(SyncTests.folder(.missing)) == .error)
        #expect(FinderStatusWriter.badge(SyncTests.folder(.error)) == .error)
        #expect(FinderStatusWriter.badge(SyncTests.folder(.upToDate, connected: 2, conflicts: 1)) == .conflict)
        #expect(FinderStatusWriter.badge(SyncTests.folder(.syncing, need: 5, rate: 1_000, connected: 1, conflicts: 1)) == .downloading,
                "moving files show while they move")
        var serving = SyncTests.folder(.upToDate, connected: 1)
        serving.rateOut = 2_000
        #expect(FinderStatusWriter.badge(serving) == .uploading)
        serving.state = .syncing
        serving.rateIn = 1_000
        #expect(FinderStatusWriter.badge(serving) == .syncing, "both ways at once")
    }

    @Test func itemsShowWhatIsPendingInConflictOrBroken() {
        let b = Self.status([Folder(id: "A", name: "Docs", path: "/Users/me/Docs", badge: .downloading,
                                    pending: ["Scans/2026/a.pdf"], conflicts: ["Plans/x (conflict, iMac, 2026-10-25 14.32).txt"],
                                    issues: ["bad:name"])])
        #expect(b.badge(for: "/Users/me/Docs") == .downloading)
        #expect(b.badge(for: "/Users/me/Docs/") == .downloading, "a trailing slash is the same folder")
        #expect(b.badge(for: "/Users/me/Docs/Scans") == .downloading, "a folder with pending files inside")
        #expect(b.badge(for: "/Users/me/Docs/Scans/2026/a.pdf") == .downloading)
        #expect(b.badge(for: "/Users/me/Docs/Plans") == .conflict)
        #expect(b.badge(for: "/Users/me/Docs/Plans/x (conflict, iMac, 2026-10-25 14.32).txt") == .conflict)
        #expect(b.badge(for: "/Users/me/Docs/bad:name") == .error)
        #expect(b.badge(for: "/Users/me/Docs/Notes.txt") == .synced)
        #expect(b.badge(for: "/Users/me/Docs/.DS_Store") == nil)
        #expect(b.badge(for: "/Users/me/Docs/.seedwell") == nil)
        #expect(b.badge(for: "/Users/me/Docs/a/._Report.docx") == nil)
        #expect(b.badge(for: "/Users/me/Docs/~$Report.docx") == nil)
        #expect(b.badge(for: "/Users/me/Documents") == nil, "a sibling with the same prefix is not inside")
        #expect(b.badge(for: "/Users/me") == nil)
    }

    @Test func aGreyFolderSaysItAll() {
        let paused = Folder(id: "A", name: "Docs", path: "/d", badge: .paused, pending: ["p"], conflicts: ["c"])
        let b = Self.status([paused])
        #expect(b.badge(for: "/d") == .paused)
        #expect(b.badge(for: "/d/p") == .paused, "what is pending waits")
        #expect(b.badge(for: "/d/c") == .conflict, "a conflict still shows")
        #expect(b.badge(for: "/d/other") == nil, "no tick while not syncing")
        let offline = Self.status([Folder(id: "A", name: "Docs", path: "/d", badge: .offline, pending: ["p"])])
        #expect(offline.badge(for: "/d/p") == .offline)
        #expect(offline.badge(for: "/d/other") == .synced, "as synced as it was")
        let broken = Self.status([Folder(id: "A", name: "Docs", path: "/d", badge: .error)])
        #expect(broken.badge(for: "/d/other") == nil)
        let quit = Self.status([Folder(id: "A", name: "Docs", path: "/d", badge: .synced)], running: false)
        #expect(quit.badge(for: "/d") == .offline, "the app is not running")
        #expect(quit.badge(for: "/d/other") == nil)
        #expect(FinderBadges(nil).badge(for: "/d") == nil, "no status file")
    }

    @Test func unknownItemsGetNoTick() {
        let ignores = Self.status([Folder(id: "A", name: "D", path: "/d", badge: .synced, complete: false)])
        #expect(ignores.badge(for: "/d/maybe-ignored.log") == nil)
        #expect(ignores.badge(for: "/d") == .synced)
        let many = Self.status([Folder(id: "A", name: "D", path: "/d", badge: .downloading, pending: ["a"], pendingMore: true)])
        #expect(many.badge(for: "/d/a") == .downloading)
        #expect(many.badge(for: "/d/b") == nil, "might be pending beyond the list")
    }

    @Test func theInnermostFolderAnswersAndUnicodeFormsMatch() {
        let b = Self.status([
            Folder(id: "O", name: "Outer", path: "/d", badge: .synced),
            Folder(id: "I", name: "Inner", path: "/d/in", badge: .paused),
            Folder(id: "U", name: "Café", path: "/u/Caf\u{E9}", badge: .synced, pending: ["r\u{E9}sum\u{E9}.txt"]),
        ])
        #expect(b.badge(for: "/d/in") == .paused)
        #expect(b.badge(for: "/d/in/x") == nil)
        #expect(b.badge(for: "/d/x") == .synced)
        // Finder may hand over decomposed names (e + combining acute).
        #expect(b.badge(for: "/u/Cafe\u{301}/re\u{301}sume\u{301}.txt") == .downloading)
        #expect(Set(b.roots) == ["/d", "/d/in", "/u/Caf\u{E9}"])
        #expect(b.badge(for: "/System/Volumes/Data/d/x") == .synced, "the data volume's own path")
        #expect(b.folder(containing: "/d/in/x")?.id == "I", "the innermost folder, for the menu")
        #expect(b.folder(containing: "/System/Volumes/Data/u/Cafe\u{301}")?.id == "U")
        #expect(b.folder(containing: "/elsewhere") == nil)
    }

    @Test func theStatusIsBuiltFromTheSnapshot() {
        var f = SyncTests.folder("Docs", .syncing, need: 5, rate: 1, connected: 1)
        f.path = "/private/tmp"
        f.issues = [SyncIssue(path: "bad", reason: "x"), SyncIssue(path: "", reason: "the archive could not be cleaned up")]
        let s = FinderStatusWriter.status(SyncTests.snap([f]), activity: { _ in
            SyncActivity(pending: ["a"], pendingMore: true, conflicts: ["c"], ownIgnores: true)
        })
        #expect(s.running)
        #expect(FinderStatusWriter.realPath("/tmp") == "/private/tmp")
        #expect(FinderStatusWriter.realPath("/no/such/folder") == "/no/such/folder")
        #expect(s.folders == [Folder(id: "ID-Docs", name: "Docs", path: "/private/tmp", badge: .downloading, pending: ["a"],
                                     pendingMore: true, conflicts: ["c"], issues: ["bad"], complete: false, canShare: true)])
    }

    @Test func theStatusFileRoundTripsAndAnUnknownVersionIsIgnored() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("finder-status-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        #expect(FinderStatus.read(from: dir) == nil)
        let s = FinderStatus(running: true, folders: [Folder(id: "A", name: "D", path: "/d", badge: .conflict, conflicts: ["c"])])
        try s.write(to: dir)
        #expect(FinderStatus.read(from: dir) == s)
        var future = s
        future.version = FinderStatus.currentVersion + 1
        try future.write(to: dir)
        #expect(FinderStatus.read(from: dir) == nil)
    }

    @Test func everyBadgeHasAnImageAndALabel() {
        for b in FinderBadge.allCases {
            #expect(!b.label.isEmpty)
            #expect(b.image.size == NSSize(width: 64, height: 64))
            // A ready bitmap: a glyph and nothing behind it (the corners are clear, no disc).
            let rep = b.image.representations.first as? NSBitmapImageRep
            #expect(rep?.pixelsWide == 128)
            var painted = 0
            for x in stride(from: 0, to: 128, by: 4) {
                for y in stride(from: 0, to: 128, by: 4) where (rep?.colorAt(x: x, y: y)?.alphaComponent ?? 0) > 0.9 {
                    painted += 1
                }
            }
            #expect(painted > 40, "\(b) has no glyph (\(painted))")
            #expect(rep?.colorAt(x: 6, y: 6)?.alphaComponent == 0, "\(b) has something behind the glyph")
        }
    }
}
