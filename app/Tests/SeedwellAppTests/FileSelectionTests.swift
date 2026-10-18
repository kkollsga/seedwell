import Testing
@testable import SeedwellApp

/// The two selections of the list (rows in the model, files in FilesStore) must not strand each other.
@MainActor @Suite struct FileSelectionTests {
    private func store() -> FilesStore {
        let s = FilesStore()
        s.expand("a")
        s.expand("b")
        return s
    }

    @Test func fileClickSelectsOneNode() {
        let s = store()
        s.select(torrent: "a", node: 2, command: false, shift: false)
        #expect(s.selectedTorrent == "a" && s.selectedNodes == [2])
        s.select(torrent: "a", node: 3, command: false, shift: false)
        #expect(s.selectedNodes == [3])
    }

    @Test func commandClickTogglesOnAndOff() {
        let s = store()
        for _ in 0..<5 { s.select(torrent: "a", node: 2, command: true, shift: false) }
        #expect(s.selectedNodes == [2])
        s.select(torrent: "a", node: 2, command: true, shift: false)
        #expect(s.selectedNodes.isEmpty)
    }

    @Test func otherTorrentStartsFresh() {
        let s = store()
        s.select(torrent: "a", node: 2, command: false, shift: false)
        s.select(torrent: "b", node: 5, command: true, shift: false)
        #expect(s.selectedTorrent == "b" && s.selectedNodes == [5])
    }

    @Test func collapseDropsItsSelectionOnly() {
        let s = store()
        s.select(torrent: "a", node: 2, command: false, shift: false)
        s.collapse("b")
        #expect(s.selectedTorrent == "a")
        s.collapse("a")
        #expect(s.selectedTorrent == nil && s.selectedNodes.isEmpty)
    }

    @Test func expandCollapseChurnLeavesNoState() {
        let s = FilesStore()
        for _ in 0..<50 { s.toggle("a") }
        #expect(!s.isExpanded("a") && s.states["a"] == nil)
        s.toggle("a")
        #expect(s.isExpanded("a") && s.states["a"] != nil)
    }

    @Test func clearSelectionResetsAnchor() {
        let s = store()
        s.select(torrent: "a", node: 2, command: false, shift: false)
        s.clearSelection()
        #expect(s.selectedTorrent == nil && s.selectedNodes.isEmpty)
        s.select(torrent: "a", node: 4, command: false, shift: true)  // no anchor: plain select
        #expect(s.selectedNodes == [4])
    }
}
