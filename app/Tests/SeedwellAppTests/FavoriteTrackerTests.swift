import Foundation
import Testing
@testable import SeedwellApp

// Favorite trackers (SPEC 7E.1, 8.3): the inline editing of a favorite.

// The sidebar's order of the trackers is the core's (crates/ffi/src/sidebar.rs, library_tests.rs).

@Suite("Tracker folders")
struct TrackerFoldersTests {
    @Test func settingFoldersForAContentTypeAddsAndClearsTheEntry() {
        var r = rec("tl", "TL", ["t.org"])
        #expect(!r.hasOwnFolders)
        r.setFolders(CategoryLocationSetting(category: .movie, incompleteDir: nil, finishedDir: "/Volumes/M/TL/Movies"))
        #expect(r.hasOwnFolders && r.folders(for: .movie).finishedDir == "/Volumes/M/TL/Movies" && r.folders(for: .tv).finishedDir == nil)
        r.setFolders(CategoryLocationSetting(category: .movie, incompleteDir: nil, finishedDir: nil))
        #expect(r.categories.isEmpty && !r.hasOwnFolders)
    }

}

private func rec(_ id: String, _ name: String, _ urls: [String]) -> ProfileRecord {
    ProfileRecord(id: id, name: name, urls: urls, rule: nil, color: nil, icon: nil, downLimit: nil, upLimit: nil,
                  incompleteDir: nil, finishedDir: nil, categories: [], hidden: false, hideOnlySeeding: false)
}

// The editing itself (fields, validation, texts, the removal question) is the core's `FavoriteEditor`, tested in
// trs-ffi's favorite_editor.rs and tracker_page_tests.rs. This checks what the window does with its answers.
@MainActor @Suite("Favorite editing")
struct FavoriteEditingTests {
    static let core: Core? = {
        let dir = NSTemporaryDirectory() + "seedwell-favorites-\(getpid())"
        try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        return try? Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
    }()

    @Test func saveAsFavoriteOpensItsTabAndRemovingAsksOnIt() throws {
        let core = try #require(Self.core)
        let e = FavoriteEditing(core: core)
        var opened: [String] = []
        e.openTab = { opened.append($0) }
        e.saveAsFavorite("tracker-z.cc")
        let id = try #require(opened.first)
        #expect(e.record(id)?.urls == ["tracker-z.cc"])
        e.beginRename(id)
        #expect(e.target == .sidebarName(trackerId: id) && e.text == e.record(id)?.name)
        e.text = "Renamed"
        e.commit()
        #expect(e.target == nil && e.record(id)?.name == "Renamed")
        e.askRemove(id)
        #expect(opened == [id, id] && e.removal?.message == "Remove “Renamed” from Favorites?")
        e.confirmRemoval()
        #expect(e.removal == nil && e.record(id) == nil)
    }
}
