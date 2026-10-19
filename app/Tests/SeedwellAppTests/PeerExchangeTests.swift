import Foundation
import Testing
@testable import SeedwellApp

// Peer exchange per private tracker (SPEC 7B.4): what the app says, and that nothing is on by default.

@Suite("Peer exchange")
struct PeerExchangeTests {
    @Test func aTrackerHasNoPeerExchangeSettingUntilItIsPrivate() {
        let t = TrackerSummary(id: "debian.org", name: "Debian", count: 3, favorite: false)
        #expect(t.private == false)
        let p = TrackerSummary(id: "example.org", name: "Example", count: 1, favorite: true, private: true)
        #expect(p.private)
    }

    @Test func aStoredProfileIsOffByDefault() {
        let r = ProfileRecord(id: "tl", name: "TL", urls: ["tl.example.org"], rule: nil, color: nil, icon: nil,
                              downLimit: nil, upLimit: nil, incompleteDir: nil, finishedDir: nil, categories: [],
                              hidden: false, hideOnlySeeding: false)
        #expect(r.allowPex == false)
    }

    @Test func theKnownPeersHoverTextSaysWhereTheyComeFrom() {
        #expect(Fmt.peersHelp.contains("tracker and incoming connections (private torrents; plus PEX when the tracker allows it); plus DHT and PEX for public ones"))
    }
}
