import Foundation
import Testing
@testable import SeedwellApp

// Cancel on a held torrent moves its .torrent file to the Trash; ⇧-Cancel keeps it (SPEC 7A.6, 7A.7, 8.6). The core
// does the removal and the move (`cancel_held`, tested in Rust); here the app only asks it and shows its note. Nothing
// here touches the real Trash.

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
        // Worded in the core (`cancel_help`); the button shows it as it comes.
        let help = cancelHelp(hasSourceFile: true)
        #expect(help == "Remove this torrent and move its .torrent file to the Trash. Hold ⇧ to keep the file.")
        #expect(!cancelHelp(hasSourceFile: false).contains("Trash"))
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

/// A stand-in for the core's Cancel: records what it was asked and answers with `note` (the file could not be moved).
@MainActor private final class Held {
    var cancelled: [String] = []
    var kept: [Bool] = []
    var sources: Set<String> = []
    var note: CancelNote?
    func engine() -> ChoiceEngine {
        ChoiceEngine(
            load: { id, _ in chooser(id: id) }, confirm: { _, _ in },
            cancel: { [self] id, keepFile in cancelled.append(id); kept.append(keepFile); return note },
            hasSource: { [self] id in sources.contains(id) })
    }
}

@MainActor @Suite("Cancel routing")
struct CancelRoutingTests {
    private func setup() -> (ChoiceStore, Held, NoteLog) {
        let held = Held(), notes = NoteLog()
        held.sources = ["t1"]
        let store = ChoiceStore(engine: held.engine(), note: { notes.add($0) })
        return (store, held, notes)
    }

    @Test func plainCancelAsksTheCoreToRemoveTheTorrentAndTrashItsFile() {
        let (store, held, notes) = setup()
        var resolved: [String] = []
        store.didResolve = { resolved.append($0) }
        _ = store.model(for: "t1")
        store.cancel("t1")
        #expect(held.cancelled == ["t1"] && held.kept == [false])
        #expect(resolved == ["t1"] && store.models["t1"] == nil)
        #expect(notes.all.isEmpty)
    }

    @Test func shiftCancelKeepsTheFile() {
        let (store, held, _) = setup()
        store.cancel("t1", keepFile: true)
        #expect(held.cancelled == ["t1"] && held.kept == [true])
    }

    @Test func noSourceFileMeansNoShiftVariant() {
        let (store, _, _) = setup()
        #expect(store.hasSource("t1"))
        #expect(!store.hasSource("magnet"))
    }

    @Test func aFileThatCouldNotBeMovedLeavesTheCoresQuietNote() {
        let (store, held, notes) = setup()
        let note = CancelNote(title: "Couldn't move a.torrent to the Trash", body: "The torrent was removed.")
        held.note = note
        store.cancel("t1")
        #expect(held.cancelled == ["t1"])
        #expect(notes.all == [note])
    }

    @Test func theNotifierAndTheChooserShareTheSameButtonState() {
        let (store, _, _) = setup()
        let m = StartingModel()
        m.sync([row("t1")], hasSource: { store.hasSource($0) })
        #expect(m.items.first?.hasSource == true)
        m.sync([row("t2")], hasSource: { store.hasSource($0) })
        #expect(m.items.first?.hasSource == false)
    }
}

@MainActor @Suite("Cancel and Start, the same everywhere")
struct CancelAndStartTests {
    @Test func startOnAHeldTorrentTakesEverythingButExtrasUntilTheUserTicksOtherwise() throws {
        let held = Held()
        let store = ChoiceStore(engine: held.engine(), note: { _ in })
        let m = try #require(store.model(for: "t1"))
        #expect(m.fileFlags() == [true] && m.canConfirm)
        m.selectNone()
        #expect(!m.canConfirm, "Start is disabled while nothing is ticked")
    }

    @Test func aHeldTorrentWithSeveralFilesHasAListInTheWindowToo() throws {
        // The window's list is the popover's model: it exists once loaded and says how many files there are.
        let store = ChoiceStore(engine: FakeFiles().engine(), note: { _ in })
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
        let store = ChoiceStore(engine: fake.engine(), note: { _ in })
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
        }, confirm: { _, _ in }, cancel: { _, _ in nil }, setCategory: { [self] _, c in set.append(c); category = c })
    }
}

@MainActor private final class FakeFiles {
    func engine() -> ChoiceEngine {
        ChoiceEngine(load: { id, _ in
            var i = chooser(id: id)
            i.groups[0].items.append(ChoiceItemInfo(label: "S01E02", bytes: 300, files: [1], selected: true, inLibrary: false, otherQuality: false, quality: nil, thisMac: false))
            i.fileCount = 2
            return i
        }, confirm: { _, _ in }, cancel: { _, _ in nil })
    }
}

@MainActor private final class NoteLog {
    var all: [CancelNote] = []
    func add(_ n: CancelNote) { all.append(n) }
}

private func row(_ id: String) -> TransferRow {
    TransferRow(
        id: id, name: "Show.S01", displayName: "Show S01", kind: .torrent, category: .tv,
        sizeBytes: 1050, doneBytes: 0, downRate: 0, upRate: 0, etaSecs: nil, trackerId: "public",
        trackerName: "Public", ratio: 0, ratioGoal: nil, seedSecs: 0, state: .queued, finishedAt: nil, savePath: "/Users/me/Media/TV/Show",
        fileExt: "mkv", seedGroup: nil, error: nil, retryInSecs: nil, movingPercent: nil, awaitingChoice: true,
        pending: nil, peers: 0, swarm: nil, knownPeers: 0, hasVideo: false, videoExt: "", fileCount: 3, lastActivity: nil, waitingNote: nil, waitingIsError: false, trackerHalted: false)
}
