import Foundation
import Testing
@testable import SeedwellApp

// The size capsule of the file list: how full it is, and the per-file rates.

private func file(_ i: UInt32, _ path: String, length: UInt64, done: UInt64, up: UInt64 = 0, prio: FilePriority = .normal) -> TorrentFile {
    TorrentFile(index: i, path: path, diskPath: "/x/" + path, name: path.split(separator: "/").last.map(String.init) ?? path,
                length: length, done: done, uploaded: up, wanted: prio != .skip, priority: prio, isVideo: false, isMp4Family: false)
}

@Suite("File capsule")
struct FileCapsuleTests {
    @Test func aFileFillsWithItsDownloadedPart() {
        let t = FileTree(files: [file(0, "S01/E01.mkv", length: 592, done: 592), file(1, "S01/E02.mkv", length: 400, done: 100),
                                 file(2, "S01/E03.mkv", length: 400, done: 0), file(3, "S01/E04.mkv", length: 400, done: 0, prio: .skip)])
        func f(_ name: String) -> Double { t.fillFraction(t.nodes.firstIndex { $0.name == name }!) }
        #expect(f("E01.mkv") == 1)
        #expect(f("E02.mkv") == 0.25)
        #expect(f("E03.mkv") == 0)
        #expect(f("E04.mkv") == 0, "skipped with nothing on disk: no fill")
    }

    @Test func aFolderAggregatesAllItsFiles() {
        let t = FileTree(files: [file(0, "S01/a.mkv", length: 500, done: 500), file(1, "S01/b.mkv", length: 500, done: 0),
                                 file(2, "S01/c.mkv", length: 500, done: 0, prio: .skip)])
        let season = t.nodes.firstIndex { $0.name == "S01" }!
        #expect(abs(t.fillFraction(season) - 1.0 / 3.0) < 1e-9, "skipped files count in the total")
        let held = FileTree(files: [file(0, "S01/a.mkv", length: 500, done: 500), file(1, "S01/b.mkv", length: 500, done: 500, prio: .skip)])
        #expect(held.fillFraction(held.nodes.firstIndex { $0.name == "S01" }!) == 1)
    }

    @Test func ratesComeFromTheChangeBetweenReads() {
        var t = FileTree(files: [file(0, "a.mkv", length: 100_000_000, done: 10_000_000, up: 0)])
        let t0 = Date()
        // First read after the build: baseline set at build time, so only the delta counts.
        let ok = t.refresh(from: [file(0, "a.mkv", length: 100_000_000, done: 12_000_000, up: 1_000_000)], keepPriorities: false, at: t0.addingTimeInterval(1))
        #expect(ok)
        let n = t.nodes[1]
        #expect(n.downRate > 0 && n.upRate > 0)
        #expect(t.rateLabel(1) != nil)
        // Nothing moves for a while: the rates fade to nothing and the label goes.
        var at = t0.addingTimeInterval(1)
        for _ in 0..<20 {
            at = at.addingTimeInterval(1)
            _ = t.refresh(from: [file(0, "a.mkv", length: 100_000_000, done: 12_000_000, up: 1_000_000)], keepPriorities: false, at: at)
        }
        #expect(t.rateLabel(1) == nil)
    }

    @Test func rateLabelShowsDownloadFirstAndHidesQuietFiles() {
        #expect(FileTree.rateLabel(down: 1_200_000, up: 5_000) == "↓ 1.2 MB/s")
        #expect(FileTree.rateLabel(down: 0, up: 340_000) == "↑ 340 KB/s")
        #expect(FileTree.rateLabel(down: 500, up: 900) == nil)
    }

    @Test func smoothingIgnoresAShrinkingCount() {
        #expect(FileTree.smoothed(0, moved: 2_000_000, dt: 1) == 1_000_000)
        #expect(FileTree.smoothed(1_000_000, moved: 0, dt: 1) == 500_000)
        #expect(FileTree.smoothed(300, moved: 0, dt: 1) == 0)
    }
}
