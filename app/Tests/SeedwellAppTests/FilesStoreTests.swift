import Testing
@testable import SeedwellApp

/// The window's file lists draw what the core's `FileLists` says (its rules are tested in Rust: file_list_tests.rs,
/// chooser_tests.rs, starting_tests.rs). These check that the app passes clicks through and redraws from the core.
@MainActor @Suite struct FilesStoreTests {
    @Test func anOpenListShowsTheCoresLinesAndTheSelectionIsTheCores() {
        let s = FilesStore(place: .window)
        s.expand("a")
        #expect(s.isExpanded("a") && s.views["a"]?.status == "Loading files…")
        s.select(torrent: "a", node: 2, command: false, shift: false)
        #expect(s.selectedTorrent == "a" && s.selectedNodes == [2])
        s.select(torrent: "a", node: 2, command: true, shift: false)
        #expect(s.selection?.nodes.isEmpty == true)
        s.collapse("a")
        #expect(!s.isExpanded("a") && s.views["a"] == nil && s.selection == nil)
    }

    @Test func theNotifiersButtonsCarryTheCoresKeys() {
        let a = heldActions(hasSourceFile: true, active: true, canStart: true, startHelp: "Start")
        #expect(a.cancel.keys == [.escape] && a.keepFile?.keys == [.shiftEscape] && a.start?.keys == [.return])
        let quiet = heldActions(hasSourceFile: false, active: false, canStart: false, startHelp: "Start")
        #expect(quiet.cancel.keys.isEmpty && quiet.keepFile == nil && quiet.start?.enabled == false)
    }
}
