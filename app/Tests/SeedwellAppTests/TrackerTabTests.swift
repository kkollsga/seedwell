import Foundation
import Testing
@testable import SeedwellApp

// The tracker tab (SPEC 8.3): its groups, fold keys and the shift-click range.

private func row(_ id: String, tracker: String = "t1") -> TransferRow {
    TransferRow(
        id: id, name: id, displayName: id, kind: .torrent, category: .movie,
        sizeBytes: 1, doneBytes: 0, downRate: 0, upRate: 0, etaSecs: nil, trackerId: tracker,
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
        let g = build(down: ["a"], recent: ["b", "c"], sharing: [.popular: ["p"], .dormant: ["d"]])
        #expect(g.map(\.id) == ["downloads", "downloading", "recent", "sharing", "popular", "dormant"])
        #expect(g.first?.isBlock == true && g.first?.trailing == "3")
        #expect(g.first { $0.id == "sharing" }?.trailing == "2")
        #expect(g.filter { $0.parent == "downloads" }.map(\.id) == ["downloading", "recent"])
    }

    @Test func emptyGroupsAndBlocksAreHidden() {
        #expect(build().isEmpty)
        #expect(build(sharing: [.active: ["x"]]).map(\.id) == ["sharing", "active"])
        #expect(build(queued: ["q"]).map(\.id) == ["downloads", "queued"])
    }

    @Test func aRecentlyFinishedTorrentIsListedOnce() {
        let g = build(recent: ["a"], sharing: [.active: ["a"]])
        #expect(g.map(\.id) == ["downloads", "recent"])
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
