import Foundation
import Testing
@testable import SeedwellApp

// Favorite trackers (SPEC 7E.1, 8.3): the inline editing of a favorite.

// The sidebar's order of the trackers is the core's (crates/ffi/src/sidebar.rs, library_tests.rs). A favorite's
// folders (rows, words, what a change does) are the core's too (tracker_folders.rs).

// The editing itself (fields, validation, texts, the removal question) is the core's `FavoriteEditor`, tested in
// trs-ffi's favorite_editor.rs and tracker_page_tests.rs. This checks what the window does with its answers.
@MainActor @Suite("Favorite editing", .timeLimit(.minutes(1)))
struct FavoriteEditingTests {
    static let core: Core? = {
        let dir = NSTemporaryDirectory() + "seedwell-favorites-\(getpid())"
        try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        return try? Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
    }()

    @Test func saveAsFavoriteOpensItsTabAndRemovingAsksOnIt() async throws {
        let core = try #require(Self.core)
        let e = FavoriteEditing(core: core)
        var opened: [String] = []
        e.openTab = { opened.append($0) }
        e.saveAsFavorite("tracker-z.cc")
        await CoreCalls.settled()
        let id = try #require(opened.first)
        #expect(e.record(id)?.urls == ["tracker-z.cc"])
        e.beginRename(id)
        #expect(e.target == .sidebarName(trackerId: id) && e.text == e.record(id)?.name)
        e.text = "Renamed"
        e.commit()
        await CoreCalls.settled()
        #expect(e.target == nil && e.record(id)?.name == "Renamed")
        e.askRemove(id)
        #expect(opened == [id, id] && e.removal?.message == "Remove “Renamed” from Favorites?")
        e.confirmRemoval()
        await CoreCalls.settled()
        #expect(e.removal == nil && e.record(id) == nil)
    }
}
