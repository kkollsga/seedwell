import Foundation
import Testing
@testable import SeedwellApp

// Cancel on a held torrent moves its .torrent file to the Trash; ⇧-Cancel keeps it (SPEC 7A.6, 7A.7, 8.6).
// Nothing here touches the real Trash: the move is a closure that records what it was asked to move.

@Suite("Cancel button text")
struct CancelTextTests {
    @Test func theButtonAlwaysReadsCancelAndShiftAddsACaptionWhereThereIsAFileToKeep() {
        #expect(CancelText.title == "Cancel")
        #expect(CancelText.caption(shift: false, hasSourceFile: true) == nil)
        #expect(CancelText.caption(shift: true, hasSourceFile: true) == "Keep .torrent file")
    }

    @Test func aMagnetLinkOrAnUnknownFileHasNoShiftVariant() {
        #expect(CancelText.caption(shift: true, hasSourceFile: false) == nil)
        #expect(CancelText.caption(shift: false, hasSourceFile: false) == nil)
        #expect(!CancelText.keepsFile(shift: true, hasSourceFile: false))
        #expect(CancelText.keepsFile(shift: true, hasSourceFile: true))
        #expect(!CancelText.keepsFile(shift: false, hasSourceFile: true))
    }

    @Test func theHoverTextSaysBothThings() {
        let help = CancelText.help(hasSourceFile: true)
        #expect(help == "Remove this torrent and move its .torrent file to the Trash. Hold ⇧ to keep the file.")
        #expect(!CancelText.help(hasSourceFile: false).contains("Trash"))
    }
}

private func chooser(id: String) -> ChoiceInfo {
    ChoiceInfo(
        id: id, name: "Show.S01", displayName: "Show S01", tracker: "", awaiting: true, reasons: [], unsure: false,
        structure: "tv",
        groups: [ChoiceGroupInfo(label: "Season 1", kind: .season, items: [
            ChoiceItemInfo(label: "S01E01", bytes: 400, files: [0], selected: true, inLibrary: false, otherQuality: false, quality: nil, thisMac: false)])],
        fileCount: 1, totalBytes: 400, selectedBytes: 400, freeBytes: 1_000_000, destination: nil, category: .tv,
        confidence: 0.9, libraryChecked: false, ruleName: "", thenText: "", summaryText: "")
}

/// A recording stand-in for the Trash.
private final class Bin: @unchecked Sendable {
    private let lock = NSLock()
    private var moved: [URL] = []
    var fail = false
    var urls: [URL] { lock.withLock { moved } }
    var trash: SourceTrash {
        SourceTrash { [self] url in
            if fail { throw CocoaError(.fileWriteNoPermission) }
            lock.withLock { moved.append(url) }
        }
    }
}

@MainActor private final class Held {
    var cancelled: [String] = []
    var sources: [String: String] = [:]
    func engine() -> ChoiceEngine {
        ChoiceEngine(
            load: { id, _ in chooser(id: id) }, confirm: { _, _ in },
            cancel: { [self] id in cancelled.append(id) },
            sourceFile: { [self] id in sources[id] },
            hasSource: { [self] id in sources[id] != nil })
    }
}

private func tempDir() throws -> URL {
    let d = FileManager.default.temporaryDirectory.appendingPathComponent("seedwell-cancel-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

@MainActor @Suite("Cancel routing")
struct CancelRoutingTests {
    private func setup(_ file: URL?) throws -> (ChoiceStore, Held, Bin, NoteLog, URL) {
        let dir = try tempDir()
        let held = Held(), bin = Bin(), notes = NoteLog()
        let path = file ?? dir.appendingPathComponent("a.torrent")
        if file == nil { try Data("d4:infode".utf8).write(to: path) }
        held.sources["t1"] = path.path
        let store = ChoiceStore(engine: held.engine(), trash: bin.trash, note: { notes.add($0) })
        return (store, held, bin, notes, path)
    }

    @Test func plainCancelRemovesTheTorrentAndTrashesItsFile() throws {
        let (store, held, bin, notes, path) = try setup(nil)
        try store.cancel("t1")
        #expect(held.cancelled == ["t1"])
        #expect(bin.urls.map(\.path) == [path.path])
        #expect(notes.all.isEmpty)
    }

    @Test func shiftCancelRemovesTheTorrentButKeepsTheFile() throws {
        let (store, held, bin, _, _) = try setup(nil)
        try store.cancel("t1", keepFile: true)
        #expect(held.cancelled == ["t1"] && bin.urls.isEmpty)
    }

    @Test func noSourceFileMeansNothingToTrash() throws {
        let held = Held(), bin = Bin()
        let store = ChoiceStore(engine: held.engine(), trash: bin.trash, note: { _ in })
        #expect(!store.hasSource("magnet"))
        try store.cancel("magnet")
        #expect(held.cancelled == ["magnet"] && bin.urls.isEmpty)
    }

    @Test func aFileThatCannotBeMovedNeverStopsTheRemovalAndLeavesAQuietNote() throws {
        let (store, held, bin, notes, _) = try setup(nil)
        bin.fail = true
        try store.cancel("t1")
        #expect(held.cancelled == ["t1"])
        #expect(notes.all == ["Couldn't move a.torrent to the Trash"])
    }

    @Test func aFailedRemovalTrashesNothing() throws {
        let (_, _, bin, _, path) = try setup(nil)
        let engine = ChoiceEngine(load: { id, _ in chooser(id: id) }, confirm: { _, _ in },
                                  cancel: { _ in throw CoreError.NotFound(id: "t1") }, sourceFile: { _ in path.path })
        let store = ChoiceStore(engine: engine, trash: bin.trash, note: { _ in })
        #expect(throws: CoreError.self) { try store.cancel("t1") }
        #expect(bin.urls.isEmpty, "the torrent is still there, so its file is too")
    }

    @Test func theNotifierAndTheChooserShareTheSameButtonState() throws {
        let (store, _, _, _, _) = try setup(nil)
        let m = StartingModel()
        m.sync([row("t1")], hasSource: { store.hasSource($0) })
        #expect(m.items.first?.hasSource == true)
        m.sync([row("t2")], hasSource: { store.hasSource($0) })
        #expect(m.items.first?.hasSource == false)
    }
}

@MainActor @Suite("Cancel and Start, the same everywhere")
struct CancelAndStartTests {
    @Test func cancelOnACountdownTorrentRemovesItAndTrashesItsFile() throws {
        let dir = try tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let f = dir.appendingPathComponent("a.torrent"); try Data("x".utf8).write(to: f)
        let bin = Bin()
        var removed: [String] = []
        // The engine refuses to cancel a torrent that does not wait for a choice: Cancel removes it instead.
        let engine = ChoiceEngine(load: { id, _ in chooser(id: id) }, confirm: { _, _ in },
                                  cancel: { _ in throw CoreError.NotFound(id: "t1") },
                                  sourceFile: { _ in f.path }, hasSource: { _ in true }, remove: { removed.append($0) })
        let store = ChoiceStore(engine: engine, trash: bin.trash, note: { _ in })
        try store.cancel("t1")
        #expect(removed == ["t1"] && bin.urls == [f])
        try store.cancel("t1", keepFile: true)
        #expect(bin.urls == [f], "⇧ keeps the file")
    }

    @Test func startOnAHeldTorrentTakesEverythingButExtrasUntilTheUserTicksOtherwise() throws {
        let held = Held()
        let store = ChoiceStore(engine: held.engine(), trash: Bin().trash, note: { _ in })
        let m = try #require(store.model(for: "t1"))
        #expect(m.fileFlags() == [true] && m.canConfirm)
        m.selectNone()
        #expect(!m.canConfirm, "Start is disabled while nothing is ticked")
    }

    @Test func aHeldTorrentWithSeveralFilesHasAListInTheWindowToo() throws {
        // The window's list is the popover's model: it exists once loaded and says how many files there are.
        let store = ChoiceStore(engine: FakeFiles().engine(), trash: Bin().trash, note: { _ in })
        let m = try #require(store.model(for: "t1"))
        #expect(m.fileFlags().count > 1)
        // A tick made "in the window" is the one the popover and the chooser window see.
        m.toggle(m.items[0].children[0])
        #expect(store.model(for: "t1")?.fileFlags().first == false)
    }

    @Test func returnAndEscapeBelongToTheOnlyBlockOrTheOpenChecklist() {
        let m = StartingModel()
        m.sync([row("a")], hasSource: { _ in false })
        #expect(m.activeBlockID == "a", "a lone block owns Return (Start) and Escape (Cancel)")
        m.sync([row("a"), row("b")], hasSource: { _ in false })
        #expect(m.activeBlockID == nil)
        m.expand("b")
        #expect(m.activeBlockID == "b")
    }

    @Test func thePillShowsTheCategoryAndAChangeRebuildsTheSharedModel() throws {
        #expect(Category.software.label == "App" && Category.game.label == "Game" && Category.tv.label == "TV")
        #expect(Category.game.symbol == "gamecontroller" && Category.all.contains(.game) && Category.all.contains(.audiobook))
        let fake = RecategorisingEngine()
        let store = ChoiceStore(engine: fake.engine(), trash: Bin().trash, note: { _ in })
        let m = try #require(store.model(for: "t1"))
        #expect(m.category == .tv && m.destination == "~/Media/TV")
        // Untouched ticks: the new plan's default (everything but Extras) applies again.
        store.changeCategory("t1", to: .other)
        #expect(fake.set == [.other] && store.models["t1"] === m, "the same shared model, rebuilt in place")
        #expect(m.category == .other && m.destination == "~/Media/Other")
        // Ticks the user changed stay where the same files are still there.
        m.toggle(m.leafItems[0])
        let kept = m.fileFlags()
        store.changeCategory("t1", to: .tv)
        #expect(m.category == .tv && m.fileFlags() == kept && m.edited)
        // Choosing the current category does nothing.
        store.changeCategory("t1", to: .tv)
        #expect(fake.set == [.other, .tv])
    }

    @Test func theConfirmWordIsStartEverywhereAHeldTorrentIsConfirmed() {
        let m = ChooserModel(info: chooser(id: "t1"), origin: "")
        #expect(m.primaryTitle == "Start")
    }
}

@MainActor private final class RecategorisingEngine {
    var set: [SeedwellApp.Category] = []
    private var category: SeedwellApp.Category = .tv
    func engine() -> ChoiceEngine {
        ChoiceEngine(load: { [self] id, _ in
            var i = chooser(id: id)
            i.category = category
            i.destination = category == .tv ? "~/Media/TV" : "~/Media/Other"
            return i
        }, confirm: { _, _ in }, cancel: { _ in }, setCategory: { [self] _, c in set.append(c); category = c })
    }
}

@MainActor private final class FakeFiles {
    func engine() -> ChoiceEngine {
        ChoiceEngine(load: { id, _ in
            var i = chooser(id: id)
            i.groups[0].items.append(ChoiceItemInfo(label: "S01E02", bytes: 300, files: [1], selected: true, inLibrary: false, otherQuality: false, quality: nil, thisMac: false))
            i.fileCount = 2
            return i
        }, confirm: { _, _ in }, cancel: { _ in })
    }
}

@MainActor private final class NoteLog {
    var all: [String] = []
    func add(_ s: String) { all.append(s) }
}

private func row(_ id: String) -> TransferRow {
    TransferRow(
        id: id, name: "Show.S01", displayName: "Show S01", kind: .torrent, category: .tv,
        sizeBytes: 1050, doneBytes: 0, downRate: 0, upRate: 0, etaSecs: nil, trackerId: "public",
        trackerName: "Public", ratio: 0, ratioGoal: nil, seedSecs: 0, state: .queued, finishedAt: nil, savePath: "/Users/me/Media/TV/Show",
        fileExt: "mkv", seedGroup: nil, error: nil, retryInSecs: nil, movingPercent: nil, awaitingChoice: true,
        pending: nil, peers: 0, swarm: nil, knownPeers: 0, hasVideo: false, videoExt: "", fileCount: 3, lastActivity: nil, waitingNote: nil, waitingIsError: false, trackerHalted: false)
}

@Suite("Only a plain .torrent file is trashed")
struct SourceTrashSafetyTests {
    @Test func aRegularTorrentFileIsMoved() throws {
        let dir = try tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let f = dir.appendingPathComponent("x.torrent")
        try Data("x".utf8).write(to: f)
        let bin = Bin()
        #expect(bin.trash.trash(f.path) == .trashed)
        #expect(bin.urls == [f])
        // The extension is not case sensitive.
        let g = dir.appendingPathComponent("Y.TORRENT")
        try Data("x".utf8).write(to: g)
        #expect(bin.trash.trash(g.path) == .trashed)
    }

    @Test func anythingElseIsLeftAlone() throws {
        let dir = try tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let bin = Bin()
        let other = dir.appendingPathComponent("movie.mkv")
        try Data("x".utf8).write(to: other)
        #expect(bin.trash.trash(other.path) == .skipped, "wrong extension")
        let real = dir.appendingPathComponent("real.dat")
        try Data("x".utf8).write(to: real)
        let link = dir.appendingPathComponent("link.torrent")
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: real)
        #expect(bin.trash.trash(link.path) == .skipped, "a link is never followed")
        let folder = dir.appendingPathComponent("folder.torrent")
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        #expect(bin.trash.trash(folder.path) == .skipped, "a folder is not a file")
        #expect(bin.trash.trash(dir.appendingPathComponent("gone.torrent").path) == .skipped)
        #expect(bin.urls.isEmpty)
    }

    @Test func aFailingMoveIsReportedByName() throws {
        let dir = try tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let f = dir.appendingPathComponent("x.torrent")
        try Data("x".utf8).write(to: f)
        let bin = Bin(); bin.fail = true
        #expect(bin.trash.trash(f.path) == .failed(name: "x.torrent"))
    }

    @Test func theQATrashIsHonouredOnlyUnderATempRoot() throws {
        let dir = try tempDir(); defer { try? FileManager.default.removeItem(at: dir) }
        let bin = dir.appendingPathComponent("bin")
        let f = dir.appendingPathComponent("x.torrent")
        try Data("x".utf8).write(to: f)
        let qa = SourceTrash.standard(environment: ["SEEDWELL_QA_TRASH_DIR": bin.path])
        #expect(qa.trash(f.path) == .trashed)
        #expect(FileManager.default.fileExists(atPath: bin.appendingPathComponent("x.torrent").path))
        #expect(!FileManager.default.fileExists(atPath: f.path))
        // A QA folder outside a temp root is ignored (the real Trash is used instead); not exercised here.
    }
}
