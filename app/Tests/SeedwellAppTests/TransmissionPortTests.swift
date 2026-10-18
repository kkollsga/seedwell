import Foundation
import Testing
@testable import SeedwellApp

// First launch only: Transmission's port is offered, from a fixture plist, and its file is never written.

private func tempDir() -> URL {
    let d = FileManager.default.temporaryDirectory.appendingPathComponent("seedwell-tport-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
    return d
}

private func fixture(in dir: URL, port: Any?, format: PropertyListSerialization.PropertyListFormat) throws -> URL {
    var d: [String: Any] = ["DownloadFolder": "/x", "SUEnableAutomaticChecks": true]
    if let port { d["BindPort"] = port }
    let url = dir.appendingPathComponent("org.m0k.transmission.plist")
    try PropertyListSerialization.data(fromPropertyList: d, format: format, options: 0).write(to: url)
    return url
}

@Suite("Transmission port offer")
struct TransmissionPortTests {
    @Test func readsBindPortFromBinaryAndXmlPlistsWithoutTouchingThem() throws {
        let dir = tempDir()
        for format in [PropertyListSerialization.PropertyListFormat.binary, .xml] {
            let url = try fixture(in: dir, port: 51413, format: format)
            let before = try Data(contentsOf: url)
            let mtime = try FileManager.default.attributesOfItem(atPath: url.path)[.modificationDate] as? Date
            #expect(TransmissionPort.readBindPort(url) == 51413)
            #expect(try Data(contentsOf: url) == before, "read-only")
            #expect(try FileManager.default.attributesOfItem(atPath: url.path)[.modificationDate] as? Date == mtime)
        }
    }

    @Test func missingOrUnusablePortsOfferNothing() throws {
        let dir = tempDir()
        #expect(TransmissionPort.readBindPort(dir.appendingPathComponent("nope.plist")) == nil)
        #expect(TransmissionPort.readBindPort(try fixture(in: dir, port: nil, format: .binary)) == nil)
        #expect(TransmissionPort.readBindPort(try fixture(in: dir, port: 80, format: .binary)) == nil, "privileged port")
        #expect(TransmissionPort.readBindPort(try fixture(in: dir, port: 70000, format: .binary)) == nil)
        #expect(TransmissionPort.readBindPort(try fixture(in: dir, port: "51413", format: .binary)) == nil, "a string is not a port")
        try Data("not a plist".utf8).write(to: dir.appendingPathComponent("junk.plist"))
        #expect(TransmissionPort.readBindPort(dir.appendingPathComponent("junk.plist")) == nil)
    }

    @Test func theOfferIsMadeOnTheFirstLaunchOfAFreshStateFolderOnly() throws {
        let prefs = try fixture(in: tempDir(), port: 51413, format: .binary)
        let state = tempDir()
        #expect(TransmissionPort.prepare(stateDir: state.path, prefs: prefs) == 51413)
        // It stays open across launches until it is taken ...
        #expect(TransmissionPort.pending(stateDir: state.path) == 51413)
        // ... and taking it closes it for good, even though the preferences still say the same.
        TransmissionPort.clear(stateDir: state.path)
        #expect(TransmissionPort.prepare(stateDir: state.path, prefs: prefs) == nil)
        // A state folder that already ran once (the database exists) gets no offer.
        let old = tempDir()
        try Data().write(to: old.appendingPathComponent("state.redb"))
        #expect(TransmissionPort.prepare(stateDir: old.path, prefs: prefs) == nil)
        // No Transmission: nothing to offer, and the question is not asked again later.
        let none = tempDir()
        #expect(TransmissionPort.prepare(stateDir: none.path, prefs: tempDir().appendingPathComponent("absent.plist")) == nil)
        #expect(TransmissionPort.prepare(stateDir: none.path, prefs: prefs) == nil, "decided on the first launch")
    }
}
