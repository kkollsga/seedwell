import Foundation
import Testing
@testable import SeedwellApp

// "Stop seeding when less than [1] [GB] uploaded in [150] hours": the threshold is typed as a number
// and a unit and stored in bytes (0 = off); the window is whole hours.

@Suite("Inactive seed input")
struct IdleInputTests {
    @Test func gigabytesAreDecimal() {
        #expect(IdleText.parse("1", unit: .gb) == 1_000_000_000)
        #expect(IdleText.parse("0,5", unit: .gb) == 500_000_000)
        #expect(IdleText.parse("2.5", unit: .tb) == 2_500_000_000_000)
        #expect(IdleText.parse("300", unit: .mb) == 300_000_000)
    }

    @Test func emptyIsOffAndGarbageIsRefused() {
        #expect(IdleText.parse("", unit: .gb) == 0)
        #expect(IdleText.parse("  ", unit: .gb) == 0)
        #expect(IdleText.parse("-1", unit: .gb) == nil)
        #expect(IdleText.parse("1 GB", unit: .gb) == nil)
        #expect(IdleText.parse("1.2.3", unit: .gb) == nil)
    }

    @Test func storedValuesShowInAFittingUnit() {
        #expect(IdleText.unit(for: 0) == .gb)
        #expect(IdleText.unit(for: 1_000_000_000) == .gb)
        #expect(IdleText.unit(for: 250_000_000) == .gb)
        #expect(IdleText.unit(for: 50_000_000) == .mb)
        #expect(IdleText.unit(for: 3_000_000_000_000) == .tb)
        #expect(IdleText.number(1_500_000_000, unit: .gb) == "1.5")
        #expect(IdleText.number(0, unit: .gb) == "")
    }

    @Test func hoursAreWholeAndBounded() {
        #expect(IdleText.parseHours("150") == 150)
        #expect(IdleText.parseHours(" 24 ") == 24)
        #expect(IdleText.parseHours("0") == nil)
        #expect(IdleText.parseHours("9000") == nil)
        #expect(IdleText.parseHours("1.5") == nil)
        #expect(IdleText.parseHours("") == nil)
    }

    @Test func summaryReadsLikeTheEnginesWords() {
        #expect(IdleText.summary(bytes: 1_000_000_000, hours: 150) == "1 GB in 150 h")
        #expect(IdleText.summary(bytes: 500_000_000, hours: 72) == "0.5 GB in 72 h")
    }
}
