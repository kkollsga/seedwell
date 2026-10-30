import AppKit
import Testing
@testable import SeedwellApp

/// The QA renderer's windows take no input (SnapshotWindow). They are invisible, and key whenever macOS lets the
/// renderer be the active app, so keys the user typed for another app reached them: a Space replaced the selected
/// link of the add modal's focused field, and `sync-add-shared-light` came out empty. Checked by event type: no event
/// is made or sent (agents and tests never synthesize input).
@MainActor @Suite struct SnapshotWindowTests {
    @Test func onlyWhatTheUserDoesIsDropped() {
        for type in [NSEvent.EventType.keyDown, .keyUp, .flagsChanged, .leftMouseDown, .rightMouseUp, .scrollWheel, .mouseMoved, .mouseEntered] {
            #expect(SnapshotWindow.isInput(type), "\(type)")
        }
        for type in [NSEvent.EventType.appKitDefined, .systemDefined, .applicationDefined, .periodic] {
            #expect(!SnapshotWindow.isInput(type), "\(type)")
        }
    }
}
