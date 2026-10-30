import Foundation
import Testing
@testable import SeedwellApp

/// The updater's rules (versions, latest.json, the schedule, when to install, the installer command) are the core's
/// and tested there (crates/ffi/src/updates_tests.rs). What stays here is the host's part: starting the installer.
@Suite struct UpdateTests {
    @Test func spawnedShellGetsItsArgumentsAndVariablesIntactAndLogs() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("seedwell-spawn-\(getpid())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let log = dir.appendingPathComponent("update.log")
        try Spawn.detached(argv: ["/bin/bash", "-c", #"printf 'got [%s] [%s] stdin=%s\n' "$1" "$SEEDWELL_SPAWN_TEST" "$(cat)""#, "x", "a path/with spaces"],
                           env: [("SEEDWELL_SPAWN_TEST", "it's \"quoted\"")], log: log)
        var text = ""
        for _ in 0..<50 {
            text = (try? String(contentsOf: log, encoding: .utf8)) ?? ""
            if text.contains("stdin=") { break }
            Thread.sleep(forTimeInterval: 0.1)
        }
        #expect(text == "got [a path/with spaces] [it's \"quoted\"] stdin=\n", "\(text)")
    }
}
