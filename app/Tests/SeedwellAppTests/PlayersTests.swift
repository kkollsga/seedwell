import Foundation
import Testing
@testable import SeedwellApp

// Run with: swift test --package-path app  (SDKROOT as in build.sh when only the Command Line Tools exist)

// MARK: Mach-O headers

private func be32(_ v: UInt32) -> [UInt8] { [UInt8(v >> 24), UInt8((v >> 16) & 255), UInt8((v >> 8) & 255), UInt8(v & 255)] }
private func le32(_ v: UInt32) -> [UInt8] { be32(v).reversed() }

private let cpuArm64: UInt32 = 0x0100_000C
private let cpuX86: UInt32 = 0x0100_0007

private func thin(_ cpu: UInt32) -> Data {
    Data(le32(0xFEED_FACF) + le32(cpu) + [UInt8](repeating: 0, count: 24))
}

private func fat(_ cpus: [UInt32], magic: UInt32 = 0xCAFE_BABE) -> Data {
    var d = be32(magic) + be32(UInt32(cpus.count))
    for c in cpus {
        d += be32(c) + be32(0)
        d += magic == 0xCAFE_BABE ? [UInt8](repeating: 0, count: 12) : [UInt8](repeating: 0, count: 24)
    }
    return Data(d)
}

@Suite("Mach-O architectures")
struct MachOTests {
    @Test func thinFiles() {
        #expect(MachO.architectures(in: thin(cpuArm64)) == [.arm64])
        #expect(MachO.architectures(in: thin(cpuX86)) == [.x86_64])
        #expect(MachO.architectures(in: thin(0x0000_0012)) == [.other])  // 32-bit PowerPC cputype
    }

    @Test func fatFiles() {
        #expect(MachO.architectures(in: fat([cpuX86, cpuArm64])) == [.x86_64, .arm64])
        #expect(MachO.architectures(in: fat([cpuX86])) == [.x86_64])
        #expect(MachO.architectures(in: fat([cpuArm64, cpuX86], magic: 0xCAFE_BABF)) == [.x86_64, .arm64])
    }

    @Test func arm64eCountsAsArm64Family() {
        // The cputype is the same; only the subtype differs.
        #expect(MachO.architectures(in: thin(cpuArm64)) == [.arm64])
    }

    @Test func notMachO() {
        #expect(MachO.architectures(in: Data("#!/bin/sh\necho hi\n".utf8)) == nil)
        #expect(MachO.architectures(in: Data()) == nil)
        #expect(MachO.architectures(in: Data([0xFE, 0xED])) == nil)
        // A Java class file starts like a fat file; its "slice count" is a version number.
        #expect(MachO.architectures(in: Data(be32(0xCAFE_BABE) + be32(65) + [UInt8](repeating: 0, count: 64))) == nil)
    }

    @Test func truncatedFatHeader() {
        #expect(MachO.architectures(in: Data(be32(0xCAFE_BABE) + be32(3) + be32(cpuArm64))) == nil)
    }

    @Test func realSystemBinary() {
        // /bin/ls is universal on a Mac that has Rosetta-era slices, thin arm64 on a newer one.
        let archs = MachO.architectures(ofFileAt: URL(fileURLWithPath: "/bin/ls"))
        #expect(archs?.contains(.arm64) == true || archs?.contains(.x86_64) == true)
        #expect(MachO.architectures(ofFileAt: URL(fileURLWithPath: "/no/such/file")) == nil)
        #expect(MachO.architectures(ofFileAt: URL(fileURLWithPath: "/etc/hosts")) == nil)
    }
}

// MARK: Can it run

@Suite("Can this app run here")
struct CanRunTests {
    let appleNoRosetta = HostCPU(appleSilicon: true, rosettaInstalled: false)
    let appleRosetta = HostCPU(appleSilicon: true, rosettaInstalled: true)
    let intel = HostCPU(appleSilicon: false, rosettaInstalled: false)

    @Test func appleSiliconHost() {
        #expect(appleNoRosetta.canRun([.arm64]))
        #expect(appleNoRosetta.canRun([.x86_64, .arm64]))
        #expect(!appleNoRosetta.canRun([.x86_64]))  // the Intel-only VLC
        #expect(appleRosetta.canRun([.x86_64]))
        #expect(!appleRosetta.canRun([.other]))
    }

    @Test func intelHost() {
        #expect(intel.canRun([.x86_64]))
        #expect(intel.canRun([.x86_64, .arm64]))
        #expect(!intel.canRun([.arm64]))
    }

    @Test func unknownIsNotHeldAgainstTheApp() {
        #expect(appleNoRosetta.canRun(nil))
        #expect(appleNoRosetta.canRun([]))
    }
}

// MARK: Detection and order

private func app(_ name: String) -> URL { URL(fileURLWithPath: "/Applications/\(name).app") }

private func detect(_ present: [PlayerSpec: Set<MachOArch>], host: HostCPU = HostCPU(appleSilicon: true, rosettaInstalled: false)) -> [InstalledPlayer] {
    let byURL = Dictionary(uniqueKeysWithValues: present.map { (app($0.key.name), $0.value) })
    return PlayerCatalog.detect(host: host,
                                locate: { present[$0] == nil ? nil : app($0.name) },
                                readArchs: { byURL[$0] })
}

@Suite("Players: detection and order")
struct PlayerOrderTests {
    @Test func intelOnlyVLCIsReportedAndLeftOut() throws {
        let installed = detect([KnownPlayers.vlc: [.x86_64]])
        let vlc = try #require(installed.first)
        #expect(vlc.needsUpdate)
        #expect(vlc.menuTitle == "VLC (needs update)")
        #expect(PlayerCatalog.capable(installed, for: .other).isEmpty)
        #expect(PlayerCatalog.capable(installed, for: .mp4Family).isEmpty)
        let choice = PlayerCatalog.resolve(preference: "auto", installed: installed, container: .other)
        #expect(choice == .needsUpdate(vlc))
        #expect(choice.message?.contains("can't run on this Mac") == true)
        #expect(choice.message?.contains("Update") == true)
    }

    @Test func universalVLCRuns() throws {
        let installed = detect([KnownPlayers.vlc: [.x86_64, .arm64]])
        #expect(installed.first?.runs == true)
        #expect(installed.first?.menuTitle == "VLC")
    }

    @Test func rosettaMakesIntelVLCUsable() {
        let installed = detect([KnownPlayers.vlc: [.x86_64]], host: HostCPU(appleSilicon: true, rosettaInstalled: true))
        #expect(installed.first?.runs == true)
    }

    @Test func automaticOrderIsIINAVLCInfuseQuickTime() {
        let all: [PlayerSpec: Set<MachOArch>] = [
            KnownPlayers.quickTime: [.arm64], KnownPlayers.infuse: [.arm64], KnownPlayers.vlc: [.arm64], KnownPlayers.iina: [.arm64],
        ]
        let installed = detect(all)
        #expect(installed.map(\.id) == [KnownPlayers.iina, KnownPlayers.vlc, KnownPlayers.infuse, KnownPlayers.quickTime].map(\.id))
        #expect(PlayerCatalog.resolve(preference: "auto", installed: installed, container: .mp4Family) == .play(installed[0]))
        // Without IINA, VLC; without VLC, Infuse; then QuickTime.
        for (drop, expected) in [(0, "VLC"), (1, "Infuse"), (2, "QuickTime Player")] {
            let rest = Array(installed.dropFirst(drop + 1))
            #expect(PlayerCatalog.resolve(preference: "auto", installed: rest, container: .mp4Family) == .play(rest[0]), "\(expected)")
            #expect(rest[0].name == expected)
        }
    }

    @Test func quickTimeOnlyForMP4Family() {
        let installed = detect([KnownPlayers.quickTime: [.arm64]])
        #expect(PlayerCatalog.resolve(preference: "auto", installed: installed, container: .mp4Family) == .play(installed[0]))
        #expect(PlayerCatalog.resolve(preference: "auto", installed: installed, container: .other) == .nothingInstalled(.other))
        #expect(PlayerCatalog.capable(installed, for: .other).isEmpty)
        #expect(PlayerChoice.nothingInstalled(.other).message?.contains("QuickTime can't play it") == true)
    }

    @Test func aBrokenFirstChoiceDoesNotHideAWorkingOne() {
        // Intel-only VLC and a working Infuse: Automatic uses Infuse, Play With lists only Infuse.
        let installed = detect([KnownPlayers.vlc: [.x86_64], KnownPlayers.infuse: [.arm64]])
        let infuse = installed.first { $0.name == "Infuse" }!
        #expect(PlayerCatalog.resolve(preference: "auto", installed: installed, container: .other) == .play(infuse))
        #expect(PlayerCatalog.capable(installed, for: .other) == [infuse])
    }

    @Test func aChosenPlayer() {
        let installed = detect([KnownPlayers.iina: [.arm64], KnownPlayers.vlc: [.arm64], KnownPlayers.quickTime: [.arm64]])
        let vlc = installed.first { $0.name == "VLC" }!
        #expect(PlayerCatalog.resolve(preference: KnownPlayers.vlc.id, installed: installed, container: .other) == .play(vlc))
        // QuickTime cannot play an MKV: fall back to Automatic instead of failing.
        #expect(PlayerCatalog.resolve(preference: KnownPlayers.quickTime.id, installed: installed, container: .other) == .play(installed[0]))
        // The chosen app was uninstalled: Automatic.
        #expect(PlayerCatalog.resolve(preference: KnownPlayers.infuse.id, installed: installed, container: .other) == .play(installed[0]))
        // An unknown id is Automatic too.
        #expect(PlayerCatalog.resolve(preference: "com.example.unknown", installed: installed, container: .other) == .play(installed[0]))
    }

    @Test func aChosenPlayerThatCannotRunIsSaidSo() {
        let installed = detect([KnownPlayers.iina: [.arm64], KnownPlayers.vlc: [.x86_64]])
        let vlc = installed.first { $0.name == "VLC" }!
        #expect(PlayerCatalog.resolve(preference: KnownPlayers.vlc.id, installed: installed, container: .other) == .needsUpdate(vlc))
    }

    @Test func nothingInstalled() {
        #expect(PlayerCatalog.resolve(preference: "auto", installed: [], container: .other) == .nothingInstalled(.other))
        #expect(PlayerChoice.nothingInstalled(.mp4Family).message?.contains("Install IINA or VLC") == true)
    }

    @Test func otherKnownPlayersFollowAsALastResort() {
        let installed = detect([KnownPlayers.mpv: [.arm64], KnownPlayers.movist: [.arm64], KnownPlayers.elmedia: [.arm64], KnownPlayers.quickTime: [.arm64]])
        #expect(installed.map(\.name) == ["QuickTime Player", "mpv", "Movist Pro", "Elmedia Player"])
        #expect(PlayerCatalog.resolve(preference: "auto", installed: installed, container: .other) == .play(installed[1]))
    }

    @Test func knownSpecLookupByAnyBundleID() {
        #expect(KnownPlayers.spec(forID: "com.firecore.infuse.mac") == KnownPlayers.infuse)
        #expect(KnownPlayers.spec(forID: "nonsense") == nil)
    }

    @Test func containerFromFileName() {
        #expect(VideoContainer(fileName: "a.MP4") == .mp4Family)
        #expect(VideoContainer(fileName: "a.mov") == .mp4Family)
        #expect(VideoContainer(fileName: "a.mkv") == .other)
        #expect(VideoContainer(fileName: "noext") == .other)
    }
}

// MARK: Launch plans

@Suite("Players: launching")
struct LaunchTests {
    func player(_ s: PlayerSpec) -> InstalledPlayer { InstalledPlayer(spec: s, appURL: app(s.name), archs: [.arm64], runs: true) }
    let stream = URL(string: "http://127.0.0.1:51234/abcdef0123456789/My%20Movie%20(2021).mkv")!

    @Test func iinaWeblinkIsPercentEncoded() throws {
        let plan = PlayerLauncher.plan(url: stream, player: player(KnownPlayers.iina))
        guard case let .openScheme(url, _) = plan else { Issue.record("not a scheme: \(plan)"); return }
        #expect(url.absoluteString == "iina://weblink?url=http%3A%2F%2F127.0.0.1%3A51234%2Fabcdef0123456789%2FMy%2520Movie%2520%282021%29.mkv")
        // Round trip: the query value decodes to the stream URL.
        let value = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems?.first { $0.name == "url" }?.value
        #expect(value == stream.absoluteString)
    }

    @Test func infuseCallback() {
        let plan = PlayerLauncher.plan(url: stream, player: player(KnownPlayers.infuse))
        guard case let .openScheme(url, _) = plan else { Issue.record("not a scheme"); return }
        #expect(url.absoluteString.hasPrefix("infuse://x-callback-url/play?url=http%3A%2F%2F127.0.0.1%3A51234%2F"))
    }

    @Test func vlcAndQuickTimeOpenTheURLWithTheApp() {
        for spec in [KnownPlayers.vlc, KnownPlayers.quickTime, KnownPlayers.movist, KnownPlayers.elmedia] {
            #expect(PlayerLauncher.plan(url: stream, player: player(spec)) == .openWith(app: app(spec.name), urls: [stream], arguments: []), "\(spec.name)")
        }
    }

    @Test func mpvTakesTheURLAsArgument() {
        #expect(PlayerLauncher.plan(url: stream, player: player(KnownPlayers.mpv)) == .openWith(app: app("mpv"), urls: [], arguments: [stream.absoluteString]))
    }

    @Test func aFinishedFileIsOpenedWithTheAppWhateverTheStyle() {
        let file = URL(fileURLWithPath: "/Users/me/Movies/A Film.mkv")
        for spec in [KnownPlayers.iina, KnownPlayers.infuse, KnownPlayers.vlc, KnownPlayers.mpv] {
            #expect(PlayerLauncher.plan(url: file, player: player(spec)) == .openWith(app: app(spec.name), urls: [file], arguments: []), "\(spec.name)")
        }
    }
}

// MARK: Files

// The episode labels of the Play menus are the core's (`play_episode_label`, tested in trs-ffi's row_commands).
@Suite("Torrent files")
struct LabelTests {
    func file(_ name: String) -> TorrentFile {
        TorrentFile(index: 0, path: "Show/\(name)", diskPath: "", name: name, length: 10, done: 0, wanted: true, priority: .normal, isVideo: true, isMp4Family: false)
    }

    @Test func finishedMeansEveryByte() {
        var f = file("a.mkv")
        #expect(!f.isFinished)
        f.done = f.length
        #expect(f.isFinished)
        #expect(!TorrentFile(index: 0, path: "e", diskPath: "", name: "e", length: 0, done: 0, wanted: true, priority: .normal, isVideo: true, isMp4Family: false).isFinished)
    }
}
