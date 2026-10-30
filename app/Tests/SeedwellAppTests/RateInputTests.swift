import Foundation
import Testing
@testable import SeedwellApp

// Speed limits are typed as a number and a unit (MB/s by default, KB/s optionally); the config keeps
// bytes per second and 0 means unlimited.

@Suite("Rate input")
struct RateInputTests {
    @Test func megabytesAreTheDefaultUnit() {
        #expect(RateText.parse("5", unit: .mb) == 5_000_000)
        #expect(RateText.parse("2.5", unit: .mb) == 2_500_000)
        #expect(RateText.parse("0.25", unit: .mb) == 250_000)
    }

    @Test func kilobytesAreOptional() {
        #expect(RateText.parse("500", unit: .kb) == 500_000)
        #expect(RateText.parse("1.5", unit: .kb) == 1_500)
        #expect(RateText.parse("1500", unit: .kb) == 1_500_000)
    }

    @Test func aCommaIsADecimalMarkToo() {
        #expect(RateText.parse("2,5", unit: .mb) == 2_500_000)
        #expect(RateText.parse(" 3 ", unit: .mb) == 3_000_000)
    }

    @Test func emptyOrZeroIsUnlimited() {
        #expect(RateText.parse("", unit: .mb) == 0)
        #expect(RateText.parse("   ", unit: .kb) == 0)
        #expect(RateText.parse("0", unit: .mb) == 0)
    }

    @Test func notANumberIsRefused() {
        #expect(RateText.parse("abc", unit: .mb) == nil)
        #expect(RateText.parse("-5", unit: .mb) == nil)
        #expect(RateText.parse("1.2.3", unit: .mb) == nil)
        #expect(RateText.parse("5 MB", unit: .mb) == nil)
        #expect(RateText.parse("inf", unit: .mb) == nil)
    }

    @Test func absurdLimitsStopAtTheCoresCeiling() {
        #expect(RateText.parse("99999999999", unit: .mb) == RateText.maxBytes)
    }

    @Test func storedBytesComeBackInTheirUnit() {
        #expect(RateText.unit(for: 5_000_000) == .mb)
        #expect(RateText.unit(for: 2_500_000) == .mb)
        #expect(RateText.unit(for: 500_000) == .kb)
        #expect(RateText.unit(for: 0) == .mb)
        #expect(RateText.number(5_000_000, unit: .mb) == "5")
        #expect(RateText.number(2_500_000, unit: .mb) == "2.5")
        #expect(RateText.number(500_000, unit: .kb) == "500")
        #expect(RateText.number(0, unit: .mb) == "")
        // Existing values convert: what the old MB/s field wrote reads back the same.
        #expect(RateText.parse(RateText.number(1_230_000, unit: .mb), unit: .mb) == 1_230_000)
    }
    // The labels, the speed line and its tooltip are the core's (crates/ffi/src/display.rs).
}
