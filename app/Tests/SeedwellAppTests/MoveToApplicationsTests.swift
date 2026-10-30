import Foundation
import Testing
@testable import SeedwellApp

// Opened from its disk image, Seedwell offers to move itself to Applications (the rule and words are the core's;
// crates/ffi/src/app_place.rs tests them). Here: the host's part, in temporary folders only (never the real
// Applications folder, and never a replacement, which would put an app in the user's Trash).

@Suite("Move to Applications")
@MainActor
struct MoveToApplicationsTests {
    private func tempDir() throws -> URL {
        let d = FileManager.default.temporaryDirectory.appendingPathComponent("seedwell-move-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true)
        return d
    }

    private func standInApp(in dir: URL) throws -> URL {
        let app = dir.appendingPathComponent("Seedwell.app")
        try FileManager.default.createDirectory(at: app.appendingPathComponent("Contents/MacOS"), withIntermediateDirectories: true)
        try Data("#!/bin/sh\n".utf8).write(to: app.appendingPathComponent("Contents/MacOS/SeedwellApp"))
        return app
    }

    @Test func theDiskImageToEjectIsTheVolumeTheAppIsOn() {
        #expect(MoveToApplications.diskImageVolume(of: URL(fileURLWithPath: "/Volumes/Seedwell 0.1.11/Seedwell.app"), readOnly: true)
            == "/Volumes/Seedwell 0.1.11")
        // A writable disk is someone's own, never ejected; a translocated copy's disk image is not known.
        #expect(MoveToApplications.diskImageVolume(of: URL(fileURLWithPath: "/Volumes/Work/Seedwell.app"), readOnly: false) == nil)
        #expect(MoveToApplications.diskImageVolume(
            of: URL(fileURLWithPath: "/private/var/folders/x/T/AppTranslocation/0A1B/d/Seedwell.app"), readOnly: true) == nil)
    }

    @Test func theAppIsCopiedIntoAnEmptyApplicationsFolderWithoutTheDownloadMark() throws {
        let from = try tempDir(), to = try tempDir()
        defer { try? FileManager.default.removeItem(at: from); try? FileManager.default.removeItem(at: to) }
        let app = try standInApp(in: from)
        _ = app.withUnsafeFileSystemRepresentation { p in
            setxattr(p!, "com.apple.quarantine", "0081;00000000;Safari;", 21, 0, 0)
        }
        let target = to.appendingPathComponent("Seedwell.app")
        try MoveToApplications.move(app, to: target, replacing: false)
        #expect(FileManager.default.fileExists(atPath: target.appendingPathComponent("Contents/MacOS/SeedwellApp").path))
        let marked = target.withUnsafeFileSystemRepresentation { p in getxattr(p!, "com.apple.quarantine", nil, 0, 0, 0) }
        #expect(marked < 0, "the copy keeps no quarantine mark")
        // The source was not in Downloads: it stays where it was.
        #expect(FileManager.default.fileExists(atPath: app.path))
        // Nothing half-made is left beside it.
        #expect(try FileManager.default.contentsOfDirectory(atPath: to.path) == ["Seedwell.app"])
    }

    @Test func anAppAlreadyThereIsNeverReplacedWithoutTheUsersYes() throws {
        let from = try tempDir(), to = try tempDir()
        defer { try? FileManager.default.removeItem(at: from); try? FileManager.default.removeItem(at: to) }
        let app = try standInApp(in: from)
        let target = try standInApp(in: to)
        try Data("the installed one".utf8).write(to: target.appendingPathComponent("marker"))
        #expect(throws: (any Error).self) { try MoveToApplications.move(app, to: target, replacing: false) }
        #expect(FileManager.default.fileExists(atPath: target.appendingPathComponent("marker").path), "the installed app is untouched")
        #expect(try FileManager.default.contentsOfDirectory(atPath: to.path) == ["Seedwell.app"], "no staging copy is left")
    }
}
