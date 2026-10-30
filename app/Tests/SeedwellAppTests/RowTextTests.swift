import Foundation
import Testing
@testable import SeedwellApp

// What the app writes itself next to the rows: long names cut in the middle. The rows' lines are the core's
// (crates/ffi/src/library_rows.rs), and so is Get Info with the trackers' live status (crates/ffi/src/info_card.rs).

@Suite("Long names")
struct LongNameTests {
    @Test func theMiddleOfALongNameGivesWay() {
        let n = Fmt.middleTruncated(String(repeating: "a", count: 30) + String(repeating: "b", count: 30), to: 20)
        #expect(n.count == 20 && n.contains("…") && n.hasPrefix("aaaa") && n.hasSuffix("bbbb"))
        #expect(Fmt.middleTruncated("short", to: 20) == "short")
    }
}
