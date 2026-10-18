import Testing
@testable import SeedwellApp

@Suite struct PrivateFloorTests {
    @Test func labelsTheChoicesAndCustomValues() {
        #expect(PrivateFloorPicker.label(0) == "Off")
        #expect(PrivateFloorPicker.label(86_400) == "1 day")
        #expect(PrivateFloorPicker.label(10 * 86_400) == "10 days")
        #expect(PrivateFloorPicker.label(36 * 3600) == "36 hours")
        #expect(PrivateFloorPicker.label(90 * 60) == "90 min")
    }
}
