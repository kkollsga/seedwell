import Foundation
import Testing
@testable import SeedwellApp

// File selection inside the notifier popover (SPEC 7A.6, 7A.7, 8.6): the shared checklist model, OK and
// Cancel, and what the notifier model shows for a torrent that waits for a choice.

private let t0 = Date(timeIntervalSince1970: 1_800_000_000)

private func file(_ label: String, _ bytes: UInt64, _ idx: UInt32, selected: Bool = true) -> ChoiceItemInfo {
    ChoiceItemInfo(label: label, bytes: bytes, files: [idx], selected: selected, inLibrary: false, otherQuality: false, quality: nil, thisMac: false)
}

/// A season of two episodes (ticked) and an Extras group with a sample (unticked, the smart default).
private func info(id: String = "t1") -> ChoiceInfo {
    ChoiceInfo(
        id: id, name: "Show.S01", displayName: "Show S01", tracker: "", awaiting: true, reasons: [], unsure: false,
        structure: "tv",
        groups: [
            ChoiceGroupInfo(label: "Season 1", kind: .season, items: [file("S01E01", 400, 0), file("S01E02", 600, 1)]),
            ChoiceGroupInfo(label: "Extras", kind: .extras, items: [file("sample.mkv", 50, 2, selected: false)]),
        ],
        fileCount: 3, totalBytes: 1050, selectedBytes: 1000, freeBytes: 1_000_000, destination: nil, category: .tv,
        confidence: 0.9, libraryChecked: false, ruleName: "", thenText: "", summaryText: "")
}

private func row(_ id: String = "t1", pending: PendingStart? = nil, awaiting: Bool, size: UInt64 = 1050) -> TransferRow {
    TransferRow(
        id: id, name: "Show.S01", displayName: "Show S01", kind: .torrent, category: .tv,
        sizeBytes: size, doneBytes: 0, downRate: 0, upRate: 0, etaSecs: nil, trackerId: "public",
        trackerName: "Public", ratio: 0, ratioGoal: nil, seedSecs: 0, state: .queued, finishedAt: nil, savePath: "/Users/me/Media/TV/Show",
        fileExt: "mkv", seedGroup: nil, error: nil, retryInSecs: nil, movingPercent: nil, awaitingChoice: awaiting,
        pending: pending, peers: 0, swarm: nil, knownPeers: 0, hasVideo: false, videoExt: "", fileCount: 3, lastActivity: nil, waitingNote: nil, waitingIsError: false, trackerHalted: false)
}

@MainActor private final class FakeEngine {
    var confirmed: [(String, [Bool])] = []
    var cancelled: [String] = []
    var loads = 0
    var failLoad = false
    func engine() -> ChoiceEngine {
        ChoiceEngine(
            load: { [self] id, _ in loads += 1; if failLoad { throw CoreError.NotFound(id: id) }; return info(id: id) },
            confirm: { [self] id, files in confirmed.append((id, files)) },
            cancel: { [self] id, _ in cancelled.append(id); return nil })
    }
}

@MainActor @Suite("Popover checklist")
struct StartingChoiceTests {
    @Test func theChecklistOfAHeldTorrentStartsWithEverythingButTheExtras() throws {
        let fake = FakeEngine()
        let store = ChoiceStore(engine: fake.engine())
        let m = try #require(store.model(for: "t1"))
        #expect(m.fileFlags() == [true, true, false], "Start without opening the list takes everything but the Extras")
        #expect(m.selectedBytes == 1000 && m.totalBytes == 1050)
        #expect(m.allState == .mixed && !m.edited)
        #expect(store.model(for: "t1") === m && fake.loads == 1, "one shared model per torrent")
    }

    @Test func selectAllAndNoneAndTheToggleBetweenThem() throws {
        let m = try #require(ChoiceStore(engine: FakeEngine().engine()).model(for: "t1"))
        m.selectAll()
        #expect(m.fileFlags() == [true, true, true] && m.allState == .on)
        m.toggleAll()
        #expect(m.fileFlags() == [false, false, false] && m.allState == .off && !m.canConfirm)
        m.toggleAll()
        #expect(m.allState == .on)
        m.selectNone()
        m.toggle(m.items[0].children[1])
        #expect(m.fileFlags() == [false, true, false] && m.state(m.items[0]) == .mixed)
    }

    @Test func okAppliesTheTicksAndForgetsTheModel() throws {
        let fake = FakeEngine()
        let store = ChoiceStore(engine: fake.engine())
        var resolved: [String] = []
        store.didResolve = { resolved.append($0) }
        let m = try #require(store.model(for: "t1"))
        m.toggle(m.items[0].children[0])   // untick E01
        try store.confirm("t1")
        #expect(fake.confirmed.count == 1 && fake.confirmed[0].0 == "t1" && fake.confirmed[0].1 == [false, true, false])
        #expect(store.models["t1"] == nil && resolved == ["t1"])
    }

    @Test func cancelRemovesTheHeldTorrentAndNothingElse() throws {
        let fake = FakeEngine()
        let store = ChoiceStore(engine: fake.engine())
        var resolved: [String] = []
        store.didResolve = { resolved.append($0) }
        _ = store.model(for: "t1")
        store.cancel("t1")
        #expect(fake.cancelled == ["t1"] && fake.confirmed.isEmpty && store.models["t1"] == nil && resolved == ["t1"])
    }

    @Test func aModelThatCannotBeLoadedYetIsNil() {
        let fake = FakeEngine()
        fake.failLoad = true
        #expect(ChoiceStore(engine: fake.engine()).model(for: "t1") == nil)
    }

    @Test func pruneKeepsOnlyTorrentsStillWaiting() {
        let store = ChoiceStore(engine: FakeEngine().engine())
        _ = store.model(for: "a"); _ = store.model(for: "b")
        store.prune(keeping: ["b"])
        #expect(Set(store.models.keys) == ["b"])
    }
}

@MainActor @Suite("Popover for a choice")
struct StartingChoiceModelTests {
    @Test func aTorrentAwaitingAChoiceIsInThePopoverWithoutACountdown() {
        let m = StartingModel()
        m.sync([row(awaiting: true)], now: t0, home: "/Users/me")
        #expect(m.items.count == 1 && m.items[0].awaiting && !m.items[0].fetching)
        #expect(m.items[0].line(at: t0) == "Choose what to download")
        #expect(m.wantsToShow && m.hasChoice && m.activeChoiceID == nil, "compact first")
    }

    @Test func chooseFilesEndsTheCountdownAndTheBlockBecomesAChecklist() {
        let m = StartingModel()
        let p = PendingStart(startsAtMs: UInt64((t0.timeIntervalSince1970 + 8) * 1000), remainingMs: 8000, paused: false)
        m.sync([row(pending: p, awaiting: false)], now: t0)
        #expect(!m.items[0].choosing && !m.hasChoice, "counting down: transient, no checklist")
        // the engine holds it for the chooser: awaiting, no deadline
        m.sync([row(awaiting: true)], now: t0.addingTimeInterval(60))
        #expect(m.items[0].awaiting && m.items[0].line(at: t0.addingTimeInterval(120)) == "Choose what to download")
        #expect(m.items[0].remaining(at: t0.addingTimeInterval(120)) >= 0)
        #expect(m.hasChoice)
    }

    @Test func aMagnetWaitsForItsFileListThenShowsTheChecklist() {
        let m = StartingModel()
        m.sync([row(pending: nil, awaiting: false, size: 0)], now: t0, requested: ["t1"])
        #expect(m.items.count == 1 && m.items[0].fetching && !m.items[0].awaiting)
        #expect(m.items[0].line(at: t0) == "Fetching file list…" && m.items[0].kind == "Magnet link")
        #expect(m.hasChoice, "the popover stays while the list is coming")
        m.sync([row(awaiting: true, size: 1050)], now: t0.addingTimeInterval(5), requested: [])
        #expect(m.items[0].awaiting && !m.items[0].fetching)
    }

    @Test func aMagnetNobodyAskedAboutStaysOutOfThePopover() {
        let m = StartingModel()
        m.sync([row(pending: nil, awaiting: false, size: 0)], now: t0)
        #expect(m.items.isEmpty)
    }

    @Test func closingLeavesTheTorrentWaitingAndDoesNotReopenIt() {
        let m = StartingModel()
        m.sync([row(awaiting: true)], now: t0)
        m.dismissAll()
        #expect(!m.wantsToShow && !m.hasChoice && m.items.count == 1)
        m.sync([row(awaiting: true), row("t2", awaiting: true)], now: t0)
        #expect(m.wantsToShow, "a new torrent brings it back")
    }

    @Test func torrentsAlreadyWaitingAtLaunchDoNotPopUp() {
        let m = StartingModel()
        m.sync([row(awaiting: true)], now: t0)
        m.dismissWaiting()
        #expect(!m.wantsToShow)
    }

    @Test func everyBlockStartsCompactAndSelectFilesOpensOnlyThatOne() {
        let m = StartingModel()
        m.sync([row("a", awaiting: true), row("b", awaiting: true)], now: t0)
        #expect(m.expandedID == nil && m.activeChoiceID == nil, "compact first: nothing owns Return and Escape")
        m.expand("a")
        #expect(m.isExpanded("a") && !m.isExpanded("b") && m.activeChoiceID == "a")
        m.expand("b")
        #expect(!m.isExpanded("a") && m.isExpanded("b") && m.activeChoiceID == "b", "only one block is open at a time")
        m.toggleExpanded("b")
        #expect(m.expandedID == nil, "the chevron returns it to compact")
        m.collapse("a")
        #expect(m.expandedID == nil)
    }

    @Test func anOpenBlockIsForgottenWithItsTorrent() {
        let m = StartingModel()
        m.sync([row("a", awaiting: true)], now: t0)
        m.expand("a")
        m.sync([row("a", awaiting: true)], now: t0.addingTimeInterval(1))
        #expect(m.isExpanded("a"), "a refresh keeps it open")
        // Between the click and the engine's answer the row is neither counting down nor awaiting: still open.
        m.sync([row("a", awaiting: false)], now: t0.addingTimeInterval(2))
        #expect(m.items.isEmpty && m.isExpanded("a"))
        m.sync([], now: t0)
        #expect(m.expandedID == nil)
    }

    @Test func aCountdownTorrentOpensItsChecklistOnceTheEngineHoldsIt() {
        let m = StartingModel()
        let p = PendingStart(startsAtMs: UInt64((t0.timeIntervalSince1970 + 8) * 1000), remainingMs: 8000, paused: false)
        m.sync([row(pending: p, awaiting: false)], now: t0)
        #expect(m.expandedID == nil && !m.items[0].choosing)
        // Select Files…: the engine holds it for the choice, then the block is expanded.
        m.sync([row(awaiting: true)], now: t0.addingTimeInterval(1))
        m.expand("t1")
        #expect(m.activeChoiceID == "t1" && m.hasChoice)
    }

    @Test func theCompactLineNamesCategorySizeAndFolder() {
        let m = StartingModel()
        m.sync([row(awaiting: true, size: 4_500_000_000)], now: t0, home: "/Users/me")
        #expect(m.items[0].detail == "TV · 4.5 GB · ~/Media/TV")
        m.sync([row(awaiting: true, size: 0)], now: t0, home: "/Users/me", requested: ["t1"])
        #expect(m.items[0].detail == "Magnet link · ~/Media/TV")
    }

    @Test func foldingAGroupNeverChangesTheTicks() throws {
        let m = try #require(ChoiceStore(engine: FakeEngine().engine()).model(for: "t1"))
        let season = m.items[0], extras = m.items[1]
        let ticks = m.checked
        m.expanded = []
        m.toggleExpanded(season)
        #expect(m.expanded == [season.id] && m.checked == ticks)
        m.toggleExpanded(extras)
        #expect(m.expanded == [season.id, extras.id] && m.checked == ticks)
        m.toggleExpanded(season); m.toggleExpanded(extras)
        #expect(m.expanded.isEmpty && m.checked == ticks)
        m.toggleExpanded(season.children[0])
        #expect(m.expanded.isEmpty, "a leaf has nothing to fold")
    }
}

// "I deselected every episode, then could not tick one": the model side of that path (the click side,
// an unticked box that ignored clicks, is HitAreaTests; qa/capture_chooser.sh replays the path in the app).
@MainActor @Suite("Deselect all, then pick one")
struct DeselectAllThenPickTests {
    private func model() throws -> ChooserModel {
        // The engine's own preselection (episodes ticked, the sample not), without the held torrent's "everything".
        ChooserModel(info: info(), origin: "")
    }

    @Test func untickingEveryEpisodeOneByOneLeavesNothingButStaysUsable() throws {
        let m = try model()
        let season = try #require(m.items.first)
        for ep in season.children { m.toggle(ep) }
        #expect(m.checked.isEmpty && m.state(season) == .off && m.allState == .off)
        #expect(!m.canConfirm && m.primaryTitle == "Start" && m.selectedCount == 0)
        #expect(m.fileFlags() == [false, false, false])
        m.toggle(season.children[1])
        #expect(m.canConfirm && m.primaryTitle == "Start")
        #expect(m.totalsLine.hasPrefix("1 \(m.childUnit.singular.lowercased()) · "))
        #expect(m.fileFlags() == [false, true, false])
        #expect(m.state(season) == .mixed)
        if season.children.count > 2 {
            m.toggle(season.children[2])
            #expect(m.primaryTitle == "Download 2 \(m.childUnit.plural)")
        }
    }

    @Test func theSeasonBoxAndSelectAllNoneRoundTripFromEmpty() throws {
        let m = try model()
        let season = try #require(m.items.first)
        m.toggle(season)                      // mixed (sample unticked is another group) / all -> ticked or off
        m.selectNone()
        #expect(m.checked.isEmpty && !m.canConfirm)
        m.toggle(season)
        #expect(m.state(season) == .on && m.canConfirm)
        m.toggle(season)
        #expect(m.state(season) == .off && !m.canConfirm)
        m.toggleAll()
        #expect(m.allState == .on)
        m.toggleAll()
        #expect(m.allState == .off && !m.canConfirm)
        m.toggle(season.children[0])
        #expect(m.fileFlags() == [true, false, false])
    }

    @Test func quickPicksAfterNothingTickedStillApply() throws {
        let m = try model()
        m.selectNone()
        m.apply(.all)
        #expect(m.canConfirm && m.selectedCount == 1 && m.pick == .all)
    }

    @Test func aRefreshDoesNotReplaceOrReapplyTheModelTheUserEmptied() throws {
        let fake = FakeEngine()
        let store = ChoiceStore(engine: fake.engine())
        let m = try #require(store.model(for: "t1"))
        m.selectNone()
        // The notifier's once-a-second refresh asks for the model again while the popover is up.
        for _ in 0..<5 { _ = store.model(for: "t1"); store.prune(keeping: ["t1"]) }
        #expect(store.model(for: "t1") === m && m.checked.isEmpty && fake.loads == 1)
        m.toggle(m.items[0].children[0])
        #expect(store.models["t1"]?.checked.count == 1)
    }
}

@MainActor @Suite("Chooser counts what is ticked")
struct ChooserPhraseTests {
    @Test func wholeSeasonsAndSingleEpisodesAreCountedSeparately() {
        let m = ChooserModel.sample()
        m.checked = []
        #expect(m.selectionPhrase.isEmpty && m.primaryTitle == "Start" && m.totalsLine.hasPrefix("Nothing selected"))
        let s3 = m.items[2], s4 = m.items[3]
        m.toggle(s3)
        #expect(m.primaryTitle == "Start" && m.totalsLine.hasPrefix("1 season · "))
        m.toggle(s4.children[0])
        #expect(m.primaryTitle == "Start" && m.totalsLine.hasPrefix("1 season + 1 episode · "))
        m.toggle(s4.children[1])
        #expect(m.totalsLine.hasPrefix("1 season + 2 episodes · "))
    }
}
