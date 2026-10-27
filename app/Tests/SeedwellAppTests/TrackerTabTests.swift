import Foundation
import Testing
@testable import SeedwellApp

// The tracker tab (SPEC 8.3): its groups, fold keys and the shift-click range.

private func row(_ id: String, tracker: String = "t1", kind: TransferKind = .torrent, down: UInt64 = 0, up: UInt64 = 0) -> TransferRow {
    TransferRow(
        id: id, name: id, displayName: id, kind: kind, category: .movie,
        sizeBytes: 1, doneBytes: 0, downRate: down, upRate: up, etaSecs: nil, trackerId: tracker,
        trackerName: tracker, ratio: 0, ratioGoal: nil, seedSecs: 0, state: .queued, finishedAt: nil, savePath: "/x",
        fileExt: "mkv", seedGroup: nil, error: nil, retryInSecs: nil, movingPercent: nil, awaitingChoice: false,
        pending: nil, peers: 0, swarm: nil, knownPeers: 0, hasVideo: false, videoExt: "", fileCount: 1, lastActivity: nil, waitingNote: nil, waitingIsError: false, trackerHalted: false)
}

private let counts = SharingCounts(popular: 1, active: 2, dormant: 1, error: 0, dead: 0, stopped: 0)

@MainActor private func build(down: [String] = [], queued: [String] = [], recent: [String] = [], sharing: [SeedGroup: [String]] = [:]) -> [RowGroup] {
    Lists.trackerGroups(downloading: down.map { row($0) }, queued: queued.map { row($0) }, recent: recent.map { row($0) },
                        sharing: sharing.mapValues { $0.map { row($0) } }, counts: counts, remaining: { _ in 0 }, showMore: { _ in })
}

@MainActor @Suite("Tracker tab")
struct TrackerTabTests {
    @Test func blocksAndSubgroupsInOrderWithCounts() {
        let g = build(down: ["a"], queued: ["q"], sharing: [.popular: ["p"], .dormant: ["d"]])
        #expect(g.map(\.id) == ["downloads", "downloading", "queued", "sharing", "popular", "dormant"])
        #expect(g.first?.isBlock == true && g.first?.trailing == "2")
        #expect(g.first { $0.id == "sharing" }?.trailing == "2")
        #expect(g.filter { $0.parent == "downloads" }.map(\.id) == ["downloading", "queued"])
    }

    @Test func emptyGroupsAndBlocksAreHidden() {
        #expect(build().isEmpty)
        #expect(build(sharing: [.active: ["x"]]).map(\.id) == ["sharing", "active"])
        #expect(build(queued: ["q"]).map(\.id) == ["downloads", "queued"])
    }

    @Test func aFinishedTorrentMovesToSharing() {
        // Just finished: still in the recent list, already a seed. It is listed once, as a seed.
        let g = build(recent: ["a"], sharing: [.active: ["a"]])
        #expect(g.map(\.id) == ["sharing", "active"])
        #expect(g.first { $0.id == "active" }?.rows.map(\.id) == ["a"])
        // No longer downloading, whatever the recent list says.
        #expect(!build(down: [], recent: ["a"], sharing: [.popular: ["a"]]).contains { $0.id == "recent" || $0.id == "downloads" })
    }

    @Test func aFinishedDirectDownloadStaysUnderRecentlyFinished() {
        let g = Lists.trackerGroups(downloading: [], queued: [], recent: [row("w", kind: .direct), row("t")],
                                    sharing: [:], counts: counts, remaining: { _ in 0 }, showMore: { _ in })
        #expect(g.map(\.id) == ["downloads", "recent"])
        #expect(g.last?.rows.map(\.id) == ["w"])
    }

    @Test func seedRowsShowTheirUploadRate() {
        let seeding = row("s", up: 2_000_000)
        #expect(seeding.rateSlot(.active)?.active == true && seeding.rateSlot(.active)?.text.hasPrefix("↑ ") == true)
        #expect(row("i").rateSlot(.active)?.text == "–" && row("i").rateSlot(.active)?.active == false)  // idle: still there
        #expect(row("d", down: 1_000_000).rateSlot(.download)?.text == "–")  // queued state: nothing flows
    }

    @Test func summaryTextShowsCombinedRates() {
        #expect(Fmt.liveRate(0, "↑") == "–" && Fmt.liveRate(2_100_000, "↑") == "↑ " + Fmt.rate(2_100_000))
        let rows = [row("a", down: 100, up: 5), row("a", down: 100, up: 5), row("b", up: 7), row("c", tracker: "t2", up: 99)]
        let r = LibraryModel.sumRates(rows, tracker: "t1")  // a duplicate counts once; other trackers do not
        #expect(r.down == 100 && r.up == 12)
    }

    @Test func lastWeekCoversTodayAndSixDaysBefore() {
        let d = (0..<30).map { DailyStat(daysAgo: UInt32($0), downBytes: 10, upBytes: 20) }
        let w = TrackerHeader.lastWeek(d)
        #expect(w.up == 140 && w.down == 70 && w.ratio == 2)
        #expect(TrackerHeader.lastWeek([]).ratio == 0)
    }

    @MainActor @Test func tickingFilesOfAnOpenListDoesNotTakeTheWindow() throws {
        let m = try #require(Self.model())
        let s = m.snapshot
        #expect(!LibraryModel.staysInPlace("zz", section: .downloads, trackerInfo: false, open: false, snapshot: s))  // list closed: normal takeover
        #expect(LibraryModel.staysInPlace("zz", section: .downloads, trackerInfo: false, open: true, snapshot: s))
        #expect(!LibraryModel.staysInPlace("zz", section: .sharing, trackerInfo: false, open: true, snapshot: s))  // it leaves Sharing: follow it
        #expect(!LibraryModel.staysInPlace("zz", section: .tracker("t1"), trackerInfo: false, open: true, snapshot: s))  // not that tracker's
    }

    @MainActor @Test func theCategoryPillFiltersAndClears() throws {
        let m = try #require(Self.model())
        var tv = row("a"); tv.category = .tv
        var movie = row("b"); movie.category = .movie
        #expect(m.matches(tv) && m.matches(movie))
        m.toggleCategory(.tv)  // a click on a TV pill
        #expect(m.categoryFilter == .tv && m.matches(tv) && !m.matches(movie))
        m.search = "a"  // combines with the search text
        #expect(m.matches(tv))
        m.search = ""
        m.toggleCategory(.tv)  // the same pill again clears
        #expect(m.categoryFilter == nil && m.matches(movie))
        m.toggleCategory(.music)
        m.select(.sharing)  // a tab switch clears it
        #expect(m.categoryFilter == nil)
        #expect(["Movie", "TV", "App"] == [Category.movie, .tv, .software].map(\.label))
    }

    @MainActor @Test func switchingTabsClosesEveryTorrent() throws {
        let m = try #require(Self.model())
        FilesStore.shared.expand("x1")
        #expect(FilesStore.shared.isExpanded("x1"))
        m.select(.downloads)  // same tab: stays open
        #expect(FilesStore.shared.isExpanded("x1"))
        m.select(.sharing)
        #expect(!FilesStore.shared.isExpanded("x1"))
        FilesStore.shared.expand("x2")
        m.select(.downloads)
        #expect(!FilesStore.shared.isExpanded("x2"))
        FilesStore.shared.expand("x3")  // a takeover that opens its row after the switch keeps it
        #expect(FilesStore.shared.isExpanded("x3"))
        FilesStore.shared.collapseAll()
    }

    @Test func rangeFollowsListOrderAcrossGroups() {
        let order = [("recent", "a"), ("recent", "b"), ("popular", "c"), ("popular", "d"), ("dormant", "e")].map { (group: $0.0, id: $0.1) }
        #expect(LibraryModel.range(order: order, anchor: "b", clicked: ("dormant", "e")) == ["b", "c", "d", "e"])
        #expect(LibraryModel.range(order: order, anchor: "d", clicked: ("recent", "a")) == ["a", "b", "c", "d"])
        #expect(LibraryModel.range(order: order, anchor: nil, clicked: ("popular", "c")) == ["c"])
    }

    @Test func aDuplicateUsesTheClickedOccurrence() {
        let order = [("recent", "a"), ("recent", "b"), ("popular", "c"), ("active", "a"), ("active", "d")].map { (group: $0.0, id: $0.1) }
        #expect(LibraryModel.range(order: order, anchor: "b", clicked: ("active", "a")) == ["b", "c", "a"])
        #expect(LibraryModel.range(order: order, anchor: "b", clicked: ("recent", "a")) == ["a", "b"])
    }

    @MainActor @Test func anchorFollowsPlainAndCommandClicks() throws {
        let m = try #require(Self.model())
        let order = ["a", "b", "c", "d"].map { (group: "g", id: $0) }
        m.click(group: "g", id: "b", command: false, shift: false, order: order)
        #expect(m.selection == ["b"] && m.selectionAnchor == "b")
        m.click(group: "g", id: "d", command: true, shift: false, order: order)
        #expect(m.selection == ["b", "d"] && m.selectionAnchor == "d")
        m.click(group: "g", id: "a", command: false, shift: true, order: order)
        #expect(m.selection == ["a", "b", "c", "d"] && m.selectionAnchor == "d")
        m.click(group: "g", id: "d", command: true, shift: false, order: order)  // deselects the anchor, which moves anyway
        #expect(!m.selection.contains("d") && m.selectionAnchor == "d")
        m.drop(["d", "x"])
        #expect(m.selectionAnchor == nil)
    }

    @MainActor @Test func collapseKeysAndFallback() throws {
        let m = try #require(Self.model())
        m.select(.tracker("deleted-profile"))  // no such profile: the tab falls back to Sharing
        #expect(m.section == .sharing)
        m.section = .tracker(m.snapshot.trackers.first?.id ?? "public")  // the key does not depend on the tracker
        #expect(m.collapseKey("dormant") == "tracker.dormant")
        #expect(m.collapseKey("downloads") == "tracker.downloads")
        m.select(.sharing)
        #expect(m.collapseKey("dormant") == "sharing.dormant")
        #expect(m.selectionAnchor == nil)
    }

    /// One mock core for the suite (it holds its state directory open); the model starts on Downloads.
    @MainActor static let core: Core? = {
        let dir = NSTemporaryDirectory() + "seedwell-tracker-tab-\(getpid())"
        try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        return try? Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
    }()
    @MainActor static func model() -> LibraryModel? { core.map { LibraryModel(core: $0) } }
}
