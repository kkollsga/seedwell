import Testing
@testable import SeedwellApp

@Suite struct FileTreeTests {
    static func file(_ i: UInt32, _ path: String, _ len: UInt64 = 100, done: UInt64 = 0, _ p: FilePriority = .normal) -> TorrentFile {
        let name = path.split(separator: "/").last.map(String.init) ?? path
        return TorrentFile(index: i, path: path, diskPath: "/d/" + path, name: name, length: len, done: done, wanted: p != .skip, priority: p,
                           isVideo: name.hasSuffix(".mkv"), isMp4Family: false)
    }

    static var season: [TorrentFile] {
        [file(0, "Show S01/Show.S01E10.mkv", 300, done: 300), file(1, "Show S01/Show.S01E02.mkv", 200, done: 100),
         file(2, "Show S01/Extras/sample.mkv", 50, .skip), file(4, "Show S01/info.nfo", 1, done: 1, .skip)]
    }

    @Test func buildsFoldersFromPathsAndSortsFoldersFirstThenStandardOrder() {
        let t = FileTree(files: Self.season)
        #expect(t.top.count == 1)
        let top = t.top[0]
        #expect(t.nodes[top].isFolder && t.nodes[top].name == "Show S01")
        let names = t.nodes[top].children.map { t.nodes[$0].name }
        #expect(names == ["Extras", "info.nfo", "Show.S01E02.mkv", "Show.S01E10.mkv"])
        #expect(t.fileCount == 4)
    }

    @Test func aTopFolderOnlyOpensWhenItIsTheOnlyOne() {
        #expect(FileTree(files: Self.season).initiallyExpanded.count == 1)
        let two = FileTree(files: [Self.file(0, "a/x"), Self.file(1, "b/y")])
        #expect(two.initiallyExpanded.isEmpty)
        let flat = FileTree(files: [Self.file(0, "x.mkv")])
        #expect(flat.initiallyExpanded.isEmpty)
    }

    @Test func aggregatesCountWantedFilesOnly() {
        let t = FileTree(files: Self.season)
        let top = t.top[0]
        #expect(t.nodes[top].length == 551)
        #expect(t.nodes[top].wantedLength == 500)
        #expect(t.wantedCount == 2 && t.fileCount == 4)
        #expect(t.progress(top) == 0.8)
        #expect(!t.isComplete(top))
    }

    @Test func folderCheckIsTriState() {
        var t = FileTree(files: Self.season)
        let top = t.top[0]
        let extras = t.nodes[top].children[0]
        #expect(t.check(top) == .mixed)
        #expect(t.check(extras) == .off)
        t.apply(t.toggleChanges([extras]))
        #expect(t.check(extras) == .on && t.check(top) == .mixed)
        t.apply(t.toggleChanges([top]))           // mixed -> everything
        #expect(t.check(top) == .on)
        t.apply(t.toggleChanges([top]))           // all on -> none
        #expect(t.check(top) == .off && t.wantedCount == 0)
    }

    @Test func checkingKeepsAWantedFilesPriority() {
        var t = FileTree(files: [Self.file(0, "a/x", 10, .high), Self.file(1, "a/y", 10, .skip)])
        let changes = t.toggleChanges([t.top[0]])
        #expect(changes.count == 1 && changes[0].index == 1 && changes[0].priority == .normal)
        t.apply(changes)
        #expect(t.shownPriority(t.top[0]) == nil)       // high and normal: mixed
    }

    @Test func priorityOnAFolderReachesWantedFilesOrEverythingWhenNoneIsWanted() {
        let t = FileTree(files: Self.season)
        let top = t.top[0]
        let high = t.priorityChanges([top], .high)
        #expect(Set(high.map(\.index)) == [0, 1])
        let extras = t.nodes[top].children[0]
        let all = t.priorityChanges([extras], .low)
        #expect(all.map(\.index) == [2])
        #expect(t.priorityChanges([top], .normal).isEmpty)  // the wanted ones are Normal already
    }

    @Test func refreshUpdatesProgressAndKeepsPendingPriorities() {
        var t = FileTree(files: Self.season)
        var next = Self.season
        next[1] = Self.file(1, "Show S01/Show.S01E02.mkv", 200, done: 200, .skip)
        t.apply([(1, .high)])
        let ok1 = t.refresh(from: next, keepPriorities: true)
        #expect(ok1)
        #expect(t.shownPriority(t.nodes[t.top[0]].children[3]) == .normal)
        #expect(t.nodes[t.nodes[t.top[0]].children[2]].priority == .high)
        #expect(t.nodes[t.nodes[t.top[0]].children[2]].done == 200)
        let ok2 = t.refresh(from: next, keepPriorities: false)
        #expect(ok2)
        #expect(t.nodes[t.nodes[t.top[0]].children[2]].priority == .skip)
        // A list of another shape asks for a rebuild.
        let ok3 = t.refresh(from: [Self.file(0, "other", 5)], keepPriorities: false)
        #expect(!ok3)
    }

    @Test func filterShowsMatchingFilesWithTheirFoldersOpen() {
        let t = FileTree(files: Self.season)
        let v = t.visible(expanded: [], filter: "e02")
        #expect(v.map { t.nodes[$0.id].name } == ["Show S01", "Show.S01E02.mkv"])
        #expect(v.map(\.depth) == [0, 1])
        #expect(t.visible(expanded: [], filter: "zzz").isEmpty)
        // A folder name matches everything inside it.
        #expect(t.visible(expanded: [], filter: "extras").count == 3)
    }

    @Test func visibleRowsFollowExpandedFolders() {
        let t = FileTree(files: Self.season)
        #expect(t.visible(expanded: [], filter: "").count == 1)
        let open = t.visible(expanded: t.initiallyExpanded, filter: "")
        #expect(open.count == 5)                       // folder, Extras (closed), info, E02, E10
        #expect(open.map(\.depth) == [0, 1, 1, 1, 1])
    }

    @Test func largeTreeBuildsQuickly() {
        let files = (0..<5000).map { Self.file(UInt32($0), "Pack/D\($0 % 40)/file \($0).bin", 10) }
        let t = FileTree(files: files)
        #expect(t.fileCount == 5000)
        #expect(t.visible(expanded: Set(t.folderIDs), filter: "").count == 5000 + 40 + 1)
    }

    @Test func missingIndicesAreFineBecausePaddingKeepsItsNumber() {
        var t = FileTree(files: Self.season)    // indices 0, 1, 2, 4
        t.apply([(4, .normal)])
        #expect(t.wantedCount == 3)
    }
}
