import AppKit
import Darwin

// Play while downloading (SPEC §7F.3): which video players are on this Mac, whether each can run on
// this CPU, which one "Play" uses and how each one is handed a URL. Everything except the actual
// launch is pure, so the detection and the ordering are unit-tested (Tests/SeedwellAppTests).

// MARK: Mach-O

/// The CPU families that matter for "can this app run here".
enum MachOArch: Hashable, Sendable { case arm64, x86_64, other }

enum MachO {
    private static let cpuArm64: UInt32 = 0x0100_000C
    private static let cpuX86_64: UInt32 = 0x0100_0007

    /// The architectures in the header of a Mach-O file, thin or fat. `nil` when `data` (the first
    /// few KiB of the file are enough) is not a Mach-O file at all.
    static func architectures(in data: Data) -> Set<MachOArch>? {
        let b = [UInt8](data.prefix(4096))
        guard b.count >= 8 else { return nil }
        func be32(_ o: Int) -> UInt32? {
            guard o + 4 <= b.count else { return nil }
            return UInt32(b[o]) << 24 | UInt32(b[o + 1]) << 16 | UInt32(b[o + 2]) << 8 | UInt32(b[o + 3])
        }
        func le32(_ o: Int) -> UInt32? {
            guard o + 4 <= b.count else { return nil }
            return UInt32(b[o + 3]) << 24 | UInt32(b[o + 2]) << 16 | UInt32(b[o + 1]) << 8 | UInt32(b[o])
        }
        func family(_ cpu: UInt32) -> MachOArch {
            cpu == cpuArm64 ? .arm64 : cpu == cpuX86_64 ? .x86_64 : .other
        }
        guard let magic = be32(0) else { return nil }
        switch magic {
        case 0xCAFE_BABE, 0xCAFE_BABF:
            // Fat header, always big-endian: magic, nfat_arch, then one entry per slice
            // (20 bytes; 32 for the 64-bit variant). A Java class file shares 0xCAFEBABE; its next
            // word is a version number (45 and up), a real fat file has a handful of slices.
            guard let n = be32(4), n >= 1, n <= 16 else { return nil }
            let entry = magic == 0xCAFE_BABE ? 20 : 32
            var out = Set<MachOArch>()
            for i in 0..<Int(n) {
                guard let cpu = be32(8 + i * entry) else { return nil }
                out.insert(family(cpu))
            }
            return out
        default:
            // Thin files are in the byte order of their CPU; every Mac we care about is little-endian.
            if let m = le32(0), m == 0xFEED_FACF || m == 0xFEED_FACE, let cpu = le32(4) { return [family(cpu)] }
            if magic == 0xFEED_FACE || magic == 0xFEED_FACF, let cpu = be32(4) { return [family(cpu)] }
            return nil
        }
    }

    /// Reads the header of the file at `url`; `nil` when it cannot be read or is not Mach-O.
    static func architectures(ofFileAt url: URL) -> Set<MachOArch>? {
        guard let h = try? FileHandle(forReadingFrom: url) else { return nil }
        defer { try? h.close() }
        guard let data = try? h.read(upToCount: 4096) else { return nil }
        return architectures(in: data)
    }

    /// The main executable of an app bundle.
    static func executable(ofApp app: URL) -> URL? { Bundle(url: app)?.executableURL }
}

/// What the machine can run.
struct HostCPU: Equatable, Sendable {
    var appleSilicon: Bool
    var rosettaInstalled: Bool

    static let rosettaRuntime = "/Library/Apple/usr/libexec/oah/libRosettaRuntime"

    static var current: HostCPU {
        var v: Int32 = 0
        var size = MemoryLayout<Int32>.size
        // `hw.optional.arm64` is 1 on Apple silicon even for a process that runs translated.
        let arm = sysctlbyname("hw.optional.arm64", &v, &size, nil, 0) == 0 && v == 1
        return HostCPU(appleSilicon: arm, rosettaInstalled: FileManager.default.fileExists(atPath: rosettaRuntime))
    }

    /// An app whose executable has these slices starts on this Mac. Unknown (an unreadable or
    /// script-based executable) is not held against the app.
    func canRun(_ archs: Set<MachOArch>?) -> Bool {
        guard let archs, !archs.isEmpty else { return true }
        if appleSilicon { return archs.contains(.arm64) || (archs.contains(.x86_64) && rosettaInstalled) }
        return archs.contains(.x86_64)
    }
}

// MARK: Players

/// A video container, as far as the choice of player goes.
enum VideoContainer: Sendable {
    /// MP4, M4V, MOV: AVFoundation (QuickTime Player) plays these.
    case mp4Family
    case other

    init(fileName: String) {
        switch (fileName as NSString).pathExtension.lowercased() {
        case "mp4", "m4v", "mov": self = .mp4Family
        default: self = .other
        }
    }

    init(isMp4Family: Bool) { self = isMp4Family ? .mp4Family : .other }
}

/// How a player is handed a stream URL.
enum LaunchStyle: Hashable, Sendable {
    /// A URL scheme of the app; `{url}` is replaced by the percent-encoded stream URL.
    case scheme(String)
    /// The URL is opened with the app (Apple events), like `open -a App URL`.
    case openURL
    /// The URL is the command-line argument of the app (`open -a App --args URL`).
    case argument
}

struct PlayerSpec: Hashable, Identifiable, Sendable {
    /// The primary bundle identifier; this is what the setting stores.
    let id: String
    let name: String
    /// Every bundle identifier the app has been seen with (the first is the primary one).
    let bundleIDs: [String]
    /// File names to look for in /Applications and ~/Applications when LaunchServices has no hit.
    let appNames: [String]
    let launch: LaunchStyle
    /// Only plays MP4 / M4V / MOV.
    let mp4Only: Bool

    func supports(_ c: VideoContainer) -> Bool { !mp4Only || c == .mp4Family }
}

enum KnownPlayers {
    static let iina = PlayerSpec(id: "com.colliderli.iina", name: "IINA", bundleIDs: ["com.colliderli.iina"], appNames: ["IINA.app"],
                                 launch: .scheme("iina://weblink?url={url}"), mp4Only: false)
    static let vlc = PlayerSpec(id: "org.videolan.vlc", name: "VLC", bundleIDs: ["org.videolan.vlc"], appNames: ["VLC.app"],
                                launch: .openURL, mp4Only: false)
    static let infuse = PlayerSpec(id: "com.firecore.infuse", name: "Infuse", bundleIDs: ["com.firecore.infuse", "com.firecore.infuse.mac"],
                                   appNames: ["Infuse.app"], launch: .scheme("infuse://x-callback-url/play?url={url}"), mp4Only: false)
    static let quickTime = PlayerSpec(id: "com.apple.QuickTimePlayerX", name: "QuickTime Player", bundleIDs: ["com.apple.QuickTimePlayerX"],
                                      appNames: ["QuickTime Player.app"], launch: .openURL, mp4Only: true)
    static let mpv = PlayerSpec(id: "io.mpv", name: "mpv", bundleIDs: ["io.mpv", "org.mpv.mpv"], appNames: ["mpv.app"],
                                launch: .argument, mp4Only: false)
    static let movist = PlayerSpec(id: "com.movist.MovistPro", name: "Movist Pro", bundleIDs: ["com.movist.MovistPro", "com.cocoable.MovistPro"],
                                   appNames: ["Movist Pro.app"], launch: .openURL, mp4Only: false)
    static let elmedia = PlayerSpec(id: "com.electronic.ElmediaPlayer", name: "Elmedia Player",
                                    bundleIDs: ["com.electronic.ElmediaPlayer", "com.eltima.elmedia", "com.eltima.ElmediaPlayer"],
                                    appNames: ["Elmedia Player.app", "Elmedia Video Player.app"], launch: .openURL, mp4Only: false)

    /// The order of "Automatic": IINA, VLC, Infuse, then QuickTime (MP4 family only). The rest follow
    /// as a last resort when none of the four is usable.
    static let all: [PlayerSpec] = [iina, vlc, infuse, quickTime, mpv, movist, elmedia]

    static func spec(forID id: String) -> PlayerSpec? {
        all.first { $0.id == id || $0.bundleIDs.contains(id) }
    }
}

/// A known player found on this Mac.
struct InstalledPlayer: Equatable, Identifiable, Sendable {
    let spec: PlayerSpec
    let appURL: URL
    /// The slices of its executable (`nil`: could not be read).
    let archs: Set<MachOArch>?
    /// The app starts on this Mac (an arm64 slice, or x86_64 with Rosetta installed).
    let runs: Bool

    var id: String { spec.id }
    var name: String { spec.name }
    var needsUpdate: Bool { !runs }
    /// "VLC" or "VLC (needs update)": the Settings popup and the Play With submenu.
    var menuTitle: String { runs ? spec.name : "\(spec.name) (needs update)" }
    /// What the core's Play entries know of it.
    var option: PlayerOption { PlayerOption(id: spec.id, name: spec.name, runs: runs, mp4Only: spec.mp4Only) }
}

/// What "Play" does with the setting and the installed players.
enum PlayerChoice: Equatable {
    case play(InstalledPlayer)
    /// Everything installed that could play this file cannot run on this Mac.
    case needsUpdate(InstalledPlayer)
    case nothingInstalled(VideoContainer)

    /// Plain sentence for the calm note, or nil when there is something to launch.
    var message: String? {
        switch self {
        case .play: nil
        case let .needsUpdate(p): "\(p.name) can't run on this Mac. Update it, or install IINA."
        case let .nothingInstalled(c):
            c == .mp4Family ? "No video player found. Install IINA or VLC." : "No video player found for this file. Install IINA or VLC (QuickTime can't play it)."
        }
    }
}

enum PlayerCatalog {
    /// `locate` finds an app by bundle identifier (LaunchServices), `readArchs` the slices of an app's
    /// executable. Both are injected so tests need no installed app.
    static func detect(host: HostCPU, locate: (PlayerSpec) -> URL?, readArchs: (URL) -> Set<MachOArch>?) -> [InstalledPlayer] {
        KnownPlayers.all.compactMap { spec in
            guard let url = locate(spec) else { return nil }
            let archs = readArchs(url)
            return InstalledPlayer(spec: spec, appURL: url, archs: archs, runs: host.canRun(archs))
        }
    }

    /// The machine's players: LaunchServices, then the Applications folders.
    @MainActor static func detectOnThisMac() -> [InstalledPlayer] {
        detect(host: .current, locate: locateWithLaunchServices) { MachO.executable(ofApp: $0).flatMap(MachO.architectures(ofFileAt:)) }
    }

    @MainActor private static func locateWithLaunchServices(_ spec: PlayerSpec) -> URL? {
        for id in spec.bundleIDs {
            if let url = NSWorkspace.shared.urlsForApplications(withBundleIdentifier: id).first { return url }
        }
        let folders = ["/Applications", NSHomeDirectory() + "/Applications"]
        for folder in folders {
            for name in spec.appNames {
                let url = URL(fileURLWithPath: folder).appendingPathComponent(name)
                if FileManager.default.fileExists(atPath: url.path) { return url }
            }
        }
        return nil
    }

    /// Players that can run here and play this container, in the order of "Automatic".
    static func capable(_ installed: [InstalledPlayer], for c: VideoContainer) -> [InstalledPlayer] {
        let ids = Set(capablePlayers(players: installed.map(\.option), mp4Family: c == .mp4Family).map(\.id))
        return installed.filter { ids.contains($0.id) }
    }

    /// `preference` is the setting: `"auto"` or a bundle identifier.
    static func resolve(preference: String, installed: [InstalledPlayer], container c: VideoContainer) -> PlayerChoice {
        let usable = capable(installed, for: c)
        if preference != "auto", let spec = KnownPlayers.spec(forID: preference),
           let chosen = installed.first(where: { $0.spec == spec }), chosen.spec.supports(c) {
            // A chosen player that cannot run is not silently replaced: say so.
            if chosen.runs { return .play(chosen) }
            return .needsUpdate(chosen)
        }
        // Automatic, or the chosen app is gone / cannot play this container: the first usable in order.
        if let first = usable.first { return .play(first) }
        if let broken = installed.first(where: { !$0.runs && $0.spec.supports(c) }) { return .needsUpdate(broken) }
        return .nothingInstalled(c)
    }
}

// MARK: Launching

enum LaunchPlan: Equatable {
    /// Open this URL (the player's scheme) with the app.
    case openScheme(URL, app: URL)
    /// Open these URLs with the app, or hand them over as arguments.
    case openWith(app: URL, urls: [URL], arguments: [String])
}

enum PlayerLauncher {
    /// Characters left alone when a URL is embedded as a query value: the unreserved set of RFC 3986.
    private static let unreserved = CharacterSet(charactersIn: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~")

    static func encode(_ s: String) -> String { s.addingPercentEncoding(withAllowedCharacters: unreserved) ?? s }

    /// How `player` is handed `url`. A file on disk (a finished download) is simply opened with the
    /// app; only streams (loopback http) go through URL schemes and arguments.
    static func plan(url: URL, player: InstalledPlayer) -> LaunchPlan {
        if url.isFileURL { return .openWith(app: player.appURL, urls: [url], arguments: []) }
        switch player.spec.launch {
        case let .scheme(template):
            let target = template.replacingOccurrences(of: "{url}", with: encode(url.absoluteString))
            return URL(string: target).map { .openScheme($0, app: player.appURL) }
                ?? .openWith(app: player.appURL, urls: [url], arguments: [])
        case .openURL:
            return .openWith(app: player.appURL, urls: [url], arguments: [])
        case .argument:
            return .openWith(app: player.appURL, urls: [], arguments: [url.absoluteString])
        }
    }

    /// QA runs set this: no Play launches anything (except the one explicit `play-app` test).
    @MainActor static var blocked = false
    /// QA: told the URL a blocked launch would have handed to the player, so a script can fetch it.
    @MainActor static var onBlockedLaunch: ((URL) -> Void)?

    /// Starts the player. Calls `done` with an error text when LaunchServices refused.
    @MainActor static func play(url: URL, with player: InstalledPlayer, done: @escaping @MainActor (String?) -> Void = { _ in }) {
        if blocked {
            onBlockedLaunch?(url)
            return done("QA: launching players is blocked")
        }
        let config = NSWorkspace.OpenConfiguration()
        config.activates = true
        let finished: @Sendable (NSRunningApplication?, Error?) -> Void = { _, error in
            let text = error.map { "\(player.name) did not open: \($0.localizedDescription)" }
            DispatchQueue.main.async { MainActor.assumeIsolated { done(text) } }
        }
        switch plan(url: url, player: player) {
        case let .openScheme(scheme, app):
            NSWorkspace.shared.open([scheme], withApplicationAt: app, configuration: config, completionHandler: finished)
        case let .openWith(app, urls, arguments):
            if !arguments.isEmpty {
                config.arguments = arguments
                NSWorkspace.shared.openApplication(at: app, configuration: config, completionHandler: finished)
            } else {
                NSWorkspace.shared.open(urls, withApplicationAt: app, configuration: config, completionHandler: finished)
            }
        }
    }
}
