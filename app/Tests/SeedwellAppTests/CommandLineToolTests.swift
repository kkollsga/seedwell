import Foundation
import Testing
@testable import SeedwellApp

@Suite(.timeLimit(.minutes(1))) struct CommandLineToolTests {
    private func temp() throws -> String {
        let d = NSTemporaryDirectory() + "seedwell-cli-test-\(UUID().uuidString)"
        try FileManager.default.createDirectory(atPath: d, withIntermediateDirectories: true)
        return d
    }

    @Test func bundledPathInsideTheApp() {
        #expect(CommandLineTool.bundledPath(bundlePath: "/Applications/Seedwell.app") == "/Applications/Seedwell.app/Contents/MacOS/seedwell")
        #expect(CommandLineTool.bundledPath(bundlePath: "/x/debug").hasSuffix("/Seedwell.app/Contents/MacOS/seedwell"))
    }

    @Test func linksIntoAFolderWithoutOverwritingAnything() throws {
        let home = try temp()
        defer { try? FileManager.default.removeItem(atPath: home) }
        let dir = home + "/.local/bin"
        let target = home + "/Seedwell.app/Contents/MacOS/seedwell"
        let link = try CommandLineTool.link(into: dir, target: target)
        #expect(link == dir + "/seedwell")
        #expect((try? FileManager.default.destinationOfSymbolicLink(atPath: link)) == target)
        // Again: the same link is fine.
        #expect(try CommandLineTool.link(into: dir, target: target) == link)
        // A link to something else is left alone.
        #expect(throws: CommandLineTool.LinkError.exists(link)) { try CommandLineTool.link(into: dir, target: "/elsewhere") }
        #expect((try? FileManager.default.destinationOfSymbolicLink(atPath: link)) == target)
    }

    @Test func aLinkFolderIsOnThePathOrNot() {
        let bin = "/Users/x/.local/bin"
        #expect(CommandLineTool.isOnPath(bin, pathEnv: "/usr/bin:" + bin))
        #expect(!CommandLineTool.isOnPath(bin, pathEnv: "/usr/bin"))
    }
}
