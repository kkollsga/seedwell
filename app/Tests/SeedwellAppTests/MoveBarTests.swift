import Foundation
import Testing
@testable import SeedwellApp

// The menu bar icon's green move bar (SPEC 8.1): hidden for a move that ends within 0.4 s (a rename
// takes about 30 ms), then the aggregate progress, gone when the moves are.

struct MoveBarTests {
    private let t0 = Date(timeIntervalSinceReferenceDate: 1000)
    private func at(_ s: TimeInterval) -> Date { t0.addingTimeInterval(s) }

    @Test func aRenameThatEndsQuicklyNeverShowsTheBar() {
        var bar = MoveBar()
        let a = bar.fraction(percent: 0, now: t0)
        let watching = bar.isWatching
        let b = bar.fraction(percent: 50, now: at(0.1))
        let c = bar.fraction(percent: nil, now: at(0.2))
        #expect(a == nil && b == nil && c == nil)
        #expect(watching)
        #expect(!bar.isWatching)
    }

    @Test func aMoveStillRunningAfterTheGraceShowsItsProgress() {
        var bar = MoveBar()
        _ = bar.fraction(percent: 0, now: t0)
        let early = bar.fraction(percent: 10, now: at(0.39))
        let mid = bar.fraction(percent: 40, now: at(0.5))
        let end = bar.fraction(percent: 100, now: at(3))
        #expect(early == nil)
        #expect(mid == 0.4)
        #expect(end == 1.0)
    }

    @Test func progressIsQuantisedSoEqualLooksCompareEqual() {
        var bar = MoveBar()
        _ = bar.fraction(percent: 1, now: t0)
        let a = bar.fraction(percent: 41, now: at(1))
        let b = bar.fraction(percent: 42, now: at(1.5))
        #expect(a != nil && a == b)
    }

    @Test func theBarGoesWhenTheMovesEndAndTheGraceStartsAgainForTheNext() {
        var bar = MoveBar()
        _ = bar.fraction(percent: 5, now: t0)
        let shown = bar.fraction(percent: 5, now: at(1))
        let gone = bar.fraction(percent: nil, now: at(2))
        let fresh = bar.fraction(percent: 0, now: at(5))  // a new move: grace again
        let later = bar.fraction(percent: 0, now: at(5.5))
        #expect(shown != nil)
        #expect(gone == nil && fresh == nil)
        #expect(later == 0)
    }
}
