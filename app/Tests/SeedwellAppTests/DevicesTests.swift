import Foundation
import Testing
@testable import SeedwellApp

// Settings › Devices (SPEC §7G.1 "Your devices"): the words and states are the core's (trs-ffi `devices_text.rs`,
// tested there); these check the tab's model: which sheet shows what, the removal's ticks, and the paste field. A mock
// core over a temporary folder (no service runs, so nothing is paired).

private func makeCore() throws -> (Core, URL) {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent("seedwell-devices-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let core = try Core(options: CoreOptions(configDir: dir.path, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
    return (core, dir)
}

private func row(_ id: String, me: Bool = false) -> DeviceRow {
    DeviceRow(id: id, title: id, detail: "", isMe: me, removable: !me, warning: nil)
}

private func request(_ id: String) -> PairRequestRow {
    PairRequestRow(id: id, question: "“\(id)” wants to join your devices.", idLine: "AAAAAAA-BBBBBBB-CCCCCCC-DDDDDDD", hint: "", pairTitle: "Pair", refuseTitle: "Refuse")
}

private func view(_ kind: DevicesKind, rows: [DeviceRow] = [], requests: [PairRequestRow] = []) -> DevicesView {
    DevicesView(kind: kind, intro: "", rows: rows, requests: requests, pairTitle: "Pair a Device…", pairEnabled: true, linkTitle: "Pair with a Link…",
                linkEnabled: true, leaveTitle: nil, leaveQuestion: "", leaveAction: "Leave", joiningLine: nil, joiningProblem: nil, myIdLine: "", note: "")
}

private let link = PairLinkView(link: "seedwell://devices/X", expiresS: 1_000 + 872, title: "Pair a Device", text: "", copyTitle: "Copy")

@MainActor @Suite("Settings › Devices")
struct DevicesTests {
    @Test func aMockCoreIsUnpairedAndCannotPair() throws {
        let (core, dir) = try makeCore()
        defer { try? FileManager.default.removeItem(at: dir) }
        let m = DevicesModel(core: core)
        #expect(m.view.kind == .unpaired && m.view.rows.isEmpty && m.view.leaveTitle == nil)
        #expect(m.view.intro.hasPrefix("Your devices trust each other"))
        m.pair()
        #expect(m.sheet == nil && m.error != nil, "no service: said, and nothing opens")
        #expect(core.syncFollowLineNew(channel: true).shown == false, "not paired: no toggle")
    }

    @Test func aRequestTurnsTheOpenLinkIntoItsConfirmation() throws {
        let (core, dir) = try makeCore()
        defer { try? FileManager.default.removeItem(at: dir) }
        let m = DevicesModel(core: core)
        m.apply(view(.alone, rows: [row("Me", me: true)]), now: 1_000)
        m.sheet = .pair(link)
        #expect(m.request == nil && m.countdown(link) == "Expires in 14:32")
        m.apply(view(.alone, rows: [row("Me", me: true)], requests: [request("Studio PC")]), now: 1_001)
        #expect(m.request?.id == "Studio PC", "the sheet shows the confirmation")
        #expect(m.waiting.isEmpty, "and not again in the tab")
        m.sheet = nil
        #expect(m.request == nil && m.waiting.map(\.id) == ["Studio PC"], "closed early: the tab shows it")
    }

    @Test func aRemovalKeepsItsTicksAndClosesWhenTheDeviceIsGone() throws {
        let (core, dir) = try makeCore()
        defer { try? FileManager.default.removeItem(at: dir) }
        let m = DevicesModel(core: core)
        let places = [RemovePlaceRow(id: "F", title: "Dev (folder)", ticked: true), RemovePlaceRow(id: "C", title: "Team (channel)", ticked: true)]
        let r = RemoveDeviceView(deviceId: "Old", title: "", text: "", placesTitle: "", places: places, askOwner: nil, paired: nil, removeTitle: "Remove")
        m.apply(view(.paired, rows: [row("Me", me: true), row("Old")]), now: 0)
        m.ticked = ["F", "C"]
        m.sheet = .remove(r)
        m.toggle(places[1])
        #expect(m.ticked == ["F"])
        m.toggle(places[1])
        #expect(m.ticked == ["F", "C"])
        m.apply(view(.paired, rows: [row("Me", me: true)]), now: 1)
        #expect(m.sheet == nil, "removed elsewhere: its sheet goes")
    }

    @Test func thePasteFieldTakesOnlyAPairingLink() throws {
        let (core, dir) = try makeCore()
        defer { try? FileManager.default.removeItem(at: dir) }
        let m = DevicesModel(core: core)
        m.showPaste()
        #expect(m.sheet == .paste && !m.canJoin)
        m.setPasted("seedwell://sync/ABC")
        #expect(m.pasteProblem != nil && !m.canJoin, "a folder link is not one: \(m.pasteProblem ?? "")")
        m.setPasted("   ")
        #expect(m.pasteProblem == nil && !m.canJoin)
    }
}
