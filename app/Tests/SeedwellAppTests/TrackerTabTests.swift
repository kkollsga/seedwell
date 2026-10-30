import Foundation
import Testing
@testable import SeedwellApp

// The tracker tab and the window (SPEC 8.3): what the window does with the core's model (its groups, folds, texts,
// selection, keys and takeover are tested in the core, library_tests.rs and library_window_tests.rs).

@MainActor @Suite("Tracker tab")
struct TrackerTabTests {
    @Test func lastWeekCoversTodayAndSixDaysBefore() {
        let d = (0..<30).map { DailyStat(daysAgo: UInt32($0), downBytes: 10, upBytes: 20) }
        let w = TrackerHeader.lastWeek(d)
        #expect(w.up == 140 && w.down == 70 && w.ratio == 2)
        #expect(TrackerHeader.lastWeek([]).ratio == 0)
    }

    @MainActor @Test func theCategoryPillFiltersAndClears() throws {
        let m = try #require(Self.model())
        m.select(.downloads)
        #expect(m.categoryFilter == nil)
        m.toggleCategory(.tv)  // a click on a TV pill
        #expect(m.categoryFilter == .tv)
        m.setSearch("a")  // combines with the search text
        #expect(m.categoryFilter == .tv)
        m.setSearch("")
        m.toggleCategory(.tv)  // the same pill again clears
        #expect(m.categoryFilter == nil)
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

    @MainActor @Test func aDeletedTrackersTabFallsBackToSharing() throws {
        let m = try #require(Self.model())
        m.select(.tracker(trackerId: "deleted-profile"))  // no such profile: the tab falls back to Sharing
        #expect(m.section == .sharing)
    }

    @MainActor @Test func aNewDownloadClosesTheTorrentsOfThePlaceItLeaves() throws {
        let m = try #require(Self.model())
        m.select(.sharing)
        FilesStore.shared.expand("x1")
        let id = try #require(m.snapshot.downloading.first?.id)
        #expect(m.newDownload(id) == .shown)
        #expect(m.section == .downloads && m.selection == [id] && m.view.scrollTo?.rowId == id)
        #expect(!FilesStore.shared.isExpanded("x1"))
    }

    /// One mock core for the suite (it holds its state directory open); the model starts on Downloads.
    @MainActor static let core: Core? = {
        let dir = NSTemporaryDirectory() + "seedwell-tracker-tab-\(getpid())"
        try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        return try? Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
    }()
    @MainActor static func model() -> LibraryModel? { core.map { LibraryModel(core: $0) } }
}
