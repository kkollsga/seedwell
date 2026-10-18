import Foundation
import Testing
@testable import SeedwellApp

// Developer mode (SPEC 8.7): what the viewer gets from the core. The log file is written by the
// engine with every secret already masked; these tests use a mock core over a temporary folder.

private let passkey = "0f3a9c52e7b14d68a1c0d9e5f2b7a346"

private func line(_ ts: Int, event: String, error: Bool) -> String {
    let resp = error
        ? #","response":{"fields":{},"error":"timeout: no answer in time (the tracker may have received it)"}"#
        : #","response":{"status":200,"fields":{"interval":"1800","complete":"4","incomplete":"2","peers":"6"}}"#
    return #"{"v":1,"ts_ms":\#(ts),"kind":"announce","torrent":"aabbccddeeff00112233445566778899aabbccdd","id":"aabbccdd","title":"Ubuntu 24.04","tracker":"https://t.example/announce?passkey=••••••","host":"t.example","transport":"https","event":"\#(event)","request":{"url":"https://t.example/announce?passkey=••••••&info_hash=%AA&key=••••••","headers":["Host: t.example"],"fields":{}}\#(resp),"duration_ms":143,"next":{"at_ms":\#(ts + 1_800_000),"in_s":1800,"why":"interval 1800 s"}}"#
}

private func makeCore(log: [String]) throws -> (Core, URL) {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent("seedwell-devlog-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: dir.appendingPathComponent("logs"), withIntermediateDirectories: true)
    try (log.joined(separator: "\n") + "\n").write(to: dir.appendingPathComponent("logs/trackers.log"), atomically: true, encoding: .utf8)
    let core = try Core(options: CoreOptions(configDir: dir.path, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
    return (core, dir)
}

@Suite("Developer mode")
struct DeveloperLogTests {
    @Test func theViewerGetsRowsWithSummariesAndDetails() throws {
        let (core, dir) = try makeCore(log: [line(1_700_000_000_000, event: "started", error: false), line(1_700_000_100_000, event: "regular", error: true)])
        defer { try? FileManager.default.removeItem(at: dir) }
        let rows = core.trackerLog(query: TrackerLogQuery(text: "", tracker: "", event: "", errorsOnly: false, limit: 0))
        #expect(rows.count == 2)
        #expect(rows[0].event == "started" && rows[0].transport == "HTTPS" && !rows[0].error)
        #expect(rows[0].summary.contains("4 seeders, 2 leechers") && rows[0].summary.contains("143 ms"))
        #expect(rows[1].error && rows[1].summary.contains("timeout"))
        #expect(rows[0].detail.contains("GET https://t.example/announce?passkey=••••••"))
        #expect(rows[0].detail.contains("next: in 30 min"))
        for r in rows { #expect(!r.detail.contains(passkey) && !r.summary.contains(passkey)) }
    }

    @Test func filtersByTextEventAndErrors() throws {
        let (core, dir) = try makeCore(log: [line(1_000, event: "started", error: false), line(2_000, event: "regular", error: true), line(3_000, event: "regular", error: false)])
        defer { try? FileManager.default.removeItem(at: dir) }
        func count(text: String = "", event: String = "", errors: Bool = false, limit: UInt32 = 0) -> Int {
            core.trackerLog(query: TrackerLogQuery(text: text, tracker: "", event: event, errorsOnly: errors, limit: limit)).count
        }
        #expect(count() == 3)
        #expect(count(text: "ubuntu") == 3)
        #expect(count(text: "aabb") == 3)
        #expect(count(text: "t.example") == 3)
        #expect(count(text: "debian") == 0)
        #expect(count(event: "regular") == 2)
        #expect(count(errors: true) == 1)
        #expect(count(limit: 2) == 2)
    }

    @Test func clearingDeletesTheFileAndTheViewerGetsNothing() throws {
        let (core, dir) = try makeCore(log: [line(1_000, event: "started", error: false)])
        defer { try? FileManager.default.removeItem(at: dir) }
        #expect(core.trackerLogPath().hasSuffix("logs/trackers.log"))
        #expect(FileManager.default.fileExists(atPath: core.trackerLogPath()))
        core.trackerLogClear()
        #expect(!FileManager.default.fileExists(atPath: core.trackerLogPath()))
        #expect(core.trackerLog(query: TrackerLogQuery(text: "", tracker: "", event: "", errorsOnly: false, limit: 0)).isEmpty)
    }

    @Test func theSettingIsOffByDefaultAndSurvivesASave() throws {
        let (core, dir) = try makeCore(log: [])
        defer { try? FileManager.default.removeItem(at: dir) }
        var s = core.settingsLoad()
        #expect(!s.advanced.developerMode)
        s.advanced.developerMode = true
        try core.settingsSave(settings: s)
        #expect(core.settingsLoad().advanced.developerMode)
        s.advanced.developerMode = false
        try core.settingsSave(settings: s)
        #expect(!core.settingsLoad().advanced.developerMode)
    }

    @MainActor @Test func theModelKeepsItsFiltersAndFollowsTheFile() async throws {
        let (core, dir) = try makeCore(log: [line(1_000, event: "started", error: false)])
        defer { try? FileManager.default.removeItem(at: dir) }
        let m = TrackerLogModel(core: core)
        await m.refresh()
        #expect(m.items.count == 1)
        let first = m.items[0].id
        m.toggle(first)
        #expect(m.expanded.contains(first))
        // a new line arrives: the first row keeps its identity (and its expansion)
        let h = try FileHandle(forWritingTo: dir.appendingPathComponent("logs/trackers.log"))
        try h.seekToEnd()
        try h.write(contentsOf: Data((line(2_000, event: "regular", error: true) + "\n").utf8))
        try h.close()
        await m.refresh()
        #expect(m.items.count == 2)
        #expect(m.items[0].id == first && m.expanded.contains(first))
        m.errorsOnly = true
        await m.refresh()
        #expect(m.items.count == 1 && m.items[0].row.error)
    }
}
