import Foundation
import Testing
@testable import SeedwellApp

@Suite struct UpdateTests {
    static let good = """
    {"version":"0.2.0","tag":"v0.2.0","notes_url":"https://github.com/kkollsga/seedwell/releases/tag/v0.2.0",
     "minimum_macos":"15.0","zip":"Seedwell-0.2.0.zip",
     "sha256":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}
    """

    @Test func versionsCompareNumerically() {
        #expect(SemVer(plain: "0.10.0")! > SemVer(plain: "0.9.9")!)
        #expect(SemVer(plain: "1.0.0")! > SemVer(plain: "0.99.99")!)
        #expect(SemVer(plain: "0.1.0")! == SemVer(plain: "0.1.0")!)
        #expect(SemVer(plain: "0.1.1")! > SemVer(plain: "0.1.0")!)
    }

    @Test func onlyPlainVersionsParse() {
        for bad in ["0.2.0-beta", "v0.2.0", "0.2", "0.2.0.1", "", "a.b.c", "0..1", "1.2.3+4"] {
            #expect(SemVer(plain: bad) == nil, "\(bad)")
        }
        #expect(SemVer(osVersion: "15") == SemVer(15, 0, 0))
        #expect(SemVer(osVersion: "15.4") == SemVer(15, 4, 0))
        #expect(SemVer(osVersion: "x") == nil)
    }

    @Test func parsesTheFixture() throws {
        let r = try #require(LatestRelease.parse(Data(Self.good.utf8)))
        #expect(r.version == SemVer(0, 2, 0))
        #expect(r.tag == "v0.2.0")
        #expect(r.notesURL?.absoluteString == "https://github.com/kkollsga/seedwell/releases/tag/v0.2.0")
        #expect(r.minimumMacOS == SemVer(15, 0, 0))
        #expect(r.zip == "Seedwell-0.2.0.zip")
    }

    @Test func missingOrGarbageFieldsAreNotOffered() {
        func parse(_ s: String) -> LatestRelease? { LatestRelease.parse(Data(s.utf8)) }
        #expect(parse("") == nil)
        #expect(parse("not json") == nil)
        #expect(parse("[]") == nil)
        #expect(parse("{}") == nil)
        for (from, to) in [
            ("\"version\":\"0.2.0\"", "\"version\":\"0.2.0-rc1\""),   // pre-release
            ("\"version\":\"0.2.0\"", "\"version\":2"),
            ("\"tag\":\"v0.2.0\"", "\"tag\":\"v0.3.0\""),             // tag disagrees
            ("\"zip\":\"Seedwell-0.2.0.zip\"", "\"zip\":\"../evil.zip\""),
            ("\"sha256\":\"0123", "\"sha256\":\"zzzz"),
            ("\"minimum_macos\":\"15.0\"", "\"minimum_macos\":\"new\""),
            ("https://github.com", "javascript:alert"),
        ] {
            #expect(parse(Self.good.replacingOccurrences(of: from, with: to)) == nil, "\(to)")
        }
        for key in ["version", "tag", "zip", "sha256"] {
            var obj = try! JSONSerialization.jsonObject(with: Data(Self.good.utf8)) as! [String: Any]
            obj[key] = nil
            #expect(LatestRelease.parse(try! JSONSerialization.data(withJSONObject: obj)) == nil, "missing \(key)")
        }
        // notes and minimum_macos are optional
        var obj = try! JSONSerialization.jsonObject(with: Data(Self.good.utf8)) as! [String: Any]
        obj["notes_url"] = nil; obj["minimum_macos"] = nil
        #expect(LatestRelease.parse(try! JSONSerialization.data(withJSONObject: obj)) != nil)
    }

    @Test func offerNeedsNewerVersionAndNewEnoughMac() throws {
        let r = try #require(LatestRelease.parse(Data(Self.good.utf8)))
        #expect(r.isOffered(current: SemVer(0, 1, 0), macOS: SemVer(15, 0, 0)))
        #expect(r.isOffered(current: SemVer(0, 1, 9), macOS: SemVer(26, 1, 0)))
        #expect(!r.isOffered(current: SemVer(0, 2, 0), macOS: SemVer(26, 0, 0)), "same version")
        #expect(!r.isOffered(current: SemVer(0, 10, 0), macOS: SemVer(26, 0, 0)), "ours is newer")
        #expect(!r.isOffered(current: SemVer(0, 1, 0), macOS: SemVer(14, 7, 0)), "Mac too old")
    }

    @Test func scheduleIsDailyWithAFirstCheck() {
        let now = Date(timeIntervalSince1970: 1_000_000)
        #expect(UpdateSchedule.isDue(lastCheck: nil, now: now))
        #expect(!UpdateSchedule.isDue(lastCheck: now.addingTimeInterval(-3600), now: now))
        #expect(!UpdateSchedule.isDue(lastCheck: now.addingTimeInterval(-86_399), now: now))
        #expect(UpdateSchedule.isDue(lastCheck: now.addingTimeInterval(-86_400), now: now))
        #expect(UpdateSchedule.isDue(lastCheck: now.addingTimeInterval(+500), now: now), "clock went back")
    }

    @Test func autoInstallWaitsForIdleAndOnlyInReleaseBuilds() {
        func go(release: Bool = true, check: Bool = true, auto: Bool = true, available: Bool = true, busy: Bool = false, installing: Bool = false) -> Bool {
            AutoInstall.shouldInstall(releaseBuild: release, checking: check, autoInstall: auto, available: available, busy: busy, installing: installing)
        }
        #expect(go())
        #expect(!go(busy: true), "downloading or moving: wait")
        #expect(!go(release: false), "development builds never install by themselves")
        #expect(!go(check: false))
        #expect(!go(auto: false))
        #expect(!go(available: false))
        #expect(!go(installing: true))
    }

    @Test func installArgumentsKeepValuesOutOfTheScript() {
        let bundle = URL(fileURLWithPath: "/Users/me/My Apps/it's \"x\"/Seedwell.app")
        let argv = UpdateInstall.arguments(repo: "https://github.com/kkollsga/seedwell/", version: "0.2.0", bundleURL: bundle)
        #expect(argv.count == 8)
        #expect(argv[0] == "/bin/bash" && argv[1] == "-c")
        #expect(argv[2] == UpdateInstall.script)
        #expect(argv[3] == "seedwell-update")
        #expect(argv[4] == "https://github.com/kkollsga/seedwell/releases/download/v0.2.0/install.sh")
        #expect(argv[5] == "/Users/me/My Apps/it's \"x\"")
        #expect(argv[6] == "0.2.0")
        #expect(argv[7] == "https://github.com/kkollsga/seedwell")
        for value in argv[4...] where value.count > 3 { #expect(!argv[2].contains(value)) }
        #expect(!argv[2].contains("My Apps") && !argv[2].contains("0.2.0"))
        #expect(UpdateInstall.latestURL(repo: "https://github.com/kkollsga/seedwell")?.absoluteString
                == "https://github.com/kkollsga/seedwell/releases/latest/download/latest.json")
    }

    @Test func spawnedShellGetsItsArgumentsIntactAndLogs() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("seedwell-spawn-\(getpid())")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let log = dir.appendingPathComponent("update.log")
        try Spawn.detached(argv: ["/bin/bash", "-c", #"printf 'got [%s] stdin=%s\n' "$1" "$(cat)""#, "x", "a path/with spaces"], log: log)
        var text = ""
        for _ in 0..<50 {
            text = (try? String(contentsOf: log, encoding: .utf8)) ?? ""
            if text.contains("stdin=") { break }
            Thread.sleep(forTimeInterval: 0.1)
        }
        #expect(text == "got [a path/with spaces] stdin=\n", "\(text)")
    }
}
