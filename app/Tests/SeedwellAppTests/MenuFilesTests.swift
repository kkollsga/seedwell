import Testing
@testable import SeedwellApp

/// The Files submenu of the menu bar menu (SPEC 8.2): what a level lists, tri-state, the cap and Select All / None.
@Suite struct MenuFilesTests {
    static func file(_ i: UInt32, _ path: String, _ len: UInt64 = 1_000_000, done: UInt64 = 0, _ p: FilePriority = .normal) -> TorrentFile {
        FileTreeTests.file(i, path, len, done: done, p)
    }

    /// Two seasons and a loose file under one wrapper folder.
    static var pack: FileTree {
        FileTree(files: [
            file(0, "Show/Season 1/e01.mkv", done: 500_000), file(1, "Show/Season 1/e02.mkv", 1_000_000, .skip),
            file(2, "Show/Season 2/e01.mkv", 1_000_000, .skip), file(3, "Show/Season 2/e02.mkv", 1_000_000, .skip),
            file(4, "Show/readme.txt", 10, done: 10),
        ])
    }

    @Test func aLoneWrapperFolderIsSteppedInto() {
        let t = Self.pack
        #expect(t.nodes[MenuFileList.start(of: t)].name == "Show")
        let list = MenuFileList(tree: t, open: [])
        #expect(list.entries.map(\.title) == ["Season 1", "Season 2", "readme.txt"])
        #expect(list.entries.map(\.isFolder) == [true, true, false])
        #expect(list.entries.allSatisfy { $0.depth == 0 })
        // Two top folders: nothing to step into.
        let two = FileTree(files: [Self.file(0, "a/x"), Self.file(1, "b/y")])
        #expect(MenuFileList.start(of: two) == 0)
        #expect(MenuFileList(tree: two, open: []).entries.map(\.title) == ["a", "b"])
    }

    @Test func openFoldersShowTheirContentIndented() {
        let t = Self.pack
        let s1 = MenuFileList(tree: t, open: []).entries[0].node
        let list = MenuFileList(tree: t, open: [s1])
        #expect(list.entries.map(\.title) == ["Season 1", "e01.mkv", "e02.mkv", "Season 2", "readme.txt"])
        #expect(list.entries.map(\.depth) == [0, 1, 1, 0, 0])
        #expect(list.entries[0].isOpen && !list.entries[3].isOpen)
    }

    @Test func foldersAreOnMixedOrOff() {
        let t = Self.pack
        let s1 = MenuFileList(tree: t, open: []).entries[0].node
        let list = MenuFileList(tree: t, open: [s1])
        #expect(list.entries.map(\.check) == [.mixed, .on, .off, .off, .on])
    }

    @Test func sizesAndProgressOfPartlyDownloadedFiles() {
        let t = Self.pack
        let s1 = MenuFileList(tree: t, open: []).entries[0].node
        let list = MenuFileList(tree: t, open: [s1])
        #expect(list.entries[1].size == "50% \u{B7} 1.0 MB")
        #expect(list.entries[2].size == "1.0 MB")   // skipped: no progress shown
        #expect(list.entries[4].size == "10 bytes")
    }

    @Test func footerCountsWantedFilesAndBytes() {
        let t = FileTree(files: (0..<13).map { Self.file(UInt32($0), "p/f\($0).mkv", 1_300_000, $0 < 8 ? .normal : .skip) })
        #expect(MenuFileList(tree: t, open: []).footer == "8 of 13 files \u{B7} 10 MB of 17 MB")
        #expect(MenuFileList.footer(FileTree(files: [Self.file(0, "a/x", 5)]), 0) == "1 of 1 file \u{B7} 5 bytes of 5 bytes")
    }

    @Test func aHugeListIsCappedAndSaysHowManyAreLeft() {
        let t = FileTree(files: (0..<650).map { Self.file(UInt32($0), "p/f\($0).mkv") })
        let list = MenuFileList(tree: t, open: [])
        #expect(list.entries.count == MenuFileList.cap && list.hidden == 650 - MenuFileList.cap)
        // Standard order: f2 before f10.
        #expect(list.entries[2].title == "f2.mkv")
        #expect(MenuFileList(tree: FileTree(files: (0..<40).map { Self.file(UInt32($0), "p/f\($0)") }), open: []).hidden == 0)
    }

    @Test func smallTorrentsStartWithEveryFolderOpen() {
        #expect(MenuFileList.initiallyOpen(Self.pack).count == 3)
        let big = FileTree(files: (0..<30).map { Self.file(UInt32($0), "a/b/f\($0)") })
        #expect(MenuFileList.initiallyOpen(big).isEmpty)
    }

    @Test func selectAllAndNoneChangeOnlyWhatDiffers() {
        var t = Self.pack
        let start = MenuFileList.start(of: t)
        let all = t.wantChanges(under: [start], true)
        #expect(Set(all.map(\.index)) == [1, 2, 3])
        #expect(all.allSatisfy { $0.priority == .normal })
        t.apply(all)
        #expect(t.wantedCount == 5 && t.check(start) == .on)
        #expect(t.wantChanges(under: [start], true).isEmpty)
        let none = t.wantChanges(under: [start], false)
        #expect(none.count == 5 && none.allSatisfy { $0.priority == .skip })
        t.apply(none)
        #expect(t.check(start) == .off && t.wantedCount == 0)
        // A folder level only touches its own files.
        let s2 = MenuFileList(tree: t, open: []).entries[1].node
        let some = t.wantChanges(under: [s2], true)
        #expect(Set(some.map(\.index)) == [2, 3])
    }

    @Test func aFolderClickTicksMixedAndUnticksFull() {
        var t = Self.pack
        let s1 = MenuFileList(tree: t, open: []).entries[0].node
        #expect(t.check(s1) == .mixed)
        t.apply(t.toggleChanges([s1]))
        #expect(t.check(s1) == .on)
        t.apply(t.toggleChanges([s1]))
        #expect(t.check(s1) == .off)
    }
}
