import Foundation
import Testing
@testable import SeedwellApp

@Suite("Listening port")
struct ListenPortTests {
    @Test func anEmptyFieldIsAutomaticAndANumberIsManual() {
        #expect(PortEntry.parse("") == .automatic)
        #expect(PortEntry.parse("   ") == .automatic)
        #expect(PortEntry.parse("55252") == .manual(55252))
        #expect(PortEntry.parse(" 1024 ") == .manual(1024))
        #expect(PortEntry.parse("65535") == .manual(65535))
    }

    @Test func outOfRangeOrNotANumberIsInvalid() {
        for bad in ["80", "1023", "65536", "99999", "abc", "12 34", "-5"] {
            #expect(PortEntry.parse(bad) == .invalid, "\(bad)")
        }
    }

    @Test func automaticShowsAnEmptyFieldAndTheConflictIsInPlainWords() {
        #expect(PortEntry.text(0) == "")
        #expect(PortEntry.text(55252) == "55252")
        let f = PortFallbackInfo(wanted: 55252, using: 53781, message: "ignored")
        #expect(ActivePortLabel.conflict(f) == "Port 55252 is in use; using 53781")
    }

    @Test func reachabilityIsOneWordPerState() {
        #expect(ReachabilityLabel.look("open").0 == "Open")
        #expect(ReachabilityLabel.look("mapped").0 == "Opened on the router")
        #expect(ReachabilityLabel.look("checking").0 == "Checking")
        #expect(ReachabilityLabel.look("closed").0 == "Closed")
        #expect(ReachabilityLabel.look("unknown").0 == "Not known")
        #expect(ReachabilityLabel.look("something new").0 == "Not known")
    }
}
