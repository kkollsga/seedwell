import Foundation

/// The pure parts of the GitHub-release updater (SPEC 12A): version comparison, the `latest.json`
/// asset, when to check, when to install by itself, and the install command. No UI, no network here,
/// so all of it is unit tested.

/// A plain `MAJOR.MINOR.PATCH` version (no pre-release suffix, no build metadata).
struct SemVer: Comparable, Equatable, CustomStringConvertible {
    let major: Int, minor: Int, patch: Int

    init(_ major: Int, _ minor: Int, _ patch: Int) { self.major = major; self.minor = minor; self.patch = patch }

    /// `1.2.3` only. `1.2.3-beta`, `v1.2.3`, `1.2` and `1.2.3.4` are rejected.
    init?(plain s: String) {
        let parts = s.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 3,
              parts.allSatisfy({ !$0.isEmpty && $0.count <= 9 && $0.allSatisfy { c in c.isASCII && c.isNumber } }),
              let a = Int(parts[0]), let b = Int(parts[1]), let c = Int(parts[2]) else { return nil }
        self.init(a, b, c)
    }

    /// An OS version such as `15` or `15.0` or `15.4.1`.
    init?(osVersion s: String) {
        let parts = s.split(separator: ".", omittingEmptySubsequences: false)
        guard (1...3).contains(parts.count),
              parts.allSatisfy({ !$0.isEmpty && $0.count <= 9 && $0.allSatisfy { c in c.isASCII && c.isNumber } }) else { return nil }
        let n = parts.compactMap { Int($0) }
        self.init(n[0], n.count > 1 ? n[1] : 0, n.count > 2 ? n[2] : 0)
    }

    var description: String { "\(major).\(minor).\(patch)" }

    static func < (a: SemVer, b: SemVer) -> Bool { (a.major, a.minor, a.patch) < (b.major, b.minor, b.patch) }
}

/// What `latest.json` (a release asset written by app/release-zip.sh) says about the newest release.
struct LatestRelease: Equatable {
    let version: SemVer
    let tag: String
    let notesURL: URL?
    let minimumMacOS: SemVer?
    let zip: String
    let sha256: String

    /// Nil when the document is not what release-zip.sh writes: missing or garbage fields mean "nothing to offer".
    static func parse(_ data: Data) -> LatestRelease? {
        guard data.count <= 64 * 1024,
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let v = obj["version"] as? String, let version = SemVer(plain: v),
              obj["tag"] as? String == "v\(v)",
              let zip = obj["zip"] as? String, zip == "Seedwell-\(v).zip",
              let sha = obj["sha256"] as? String, sha.count == 64, sha.allSatisfy({ $0.isHexDigit })
        else { return nil }
        var notes: URL?
        if let n = obj["notes_url"] {
            guard let s = n as? String, let u = URL(string: s), u.scheme == "https" || u.scheme == "http" else { return nil }
            notes = u
        }
        var minimum: SemVer?
        if let m = obj["minimum_macos"] {
            guard let s = m as? String, let mv = SemVer(osVersion: s) else { return nil }
            minimum = mv
        }
        return LatestRelease(version: version, tag: "v\(v)", notesURL: notes, minimumMacOS: minimum, zip: zip, sha256: sha.lowercased())
    }

    /// Newer than the running version, and this Mac is new enough for it.
    func isOffered(current: SemVer, macOS: SemVer) -> Bool {
        guard version > current else { return false }
        if let m = minimumMacOS, macOS < m { return false }
        return true
    }
}

enum UpdateSchedule {
    static let interval: TimeInterval = 24 * 3600

    /// A check is due when none has happened for a day (or the clock went backwards).
    static func isDue(lastCheck: Date?, now: Date) -> Bool {
        guard let last = lastCheck else { return true }
        let age = now.timeIntervalSince(last)
        return age >= interval || age < 0
    }
}

enum AutoInstall {
    /// Install by itself only in a release build, with the setting on, an update waiting, nothing
    /// downloading or being moved, and no install already started. Seeding does not block it.
    static func shouldInstall(releaseBuild: Bool, checking: Bool, autoInstall: Bool, available: Bool, busy: Bool, installing: Bool) -> Bool {
        releaseBuild && checking && autoInstall && available && !busy && !installing
    }
}

enum UpdateInstall {
    /// The script is constant: the installer URL, the folder, the version and the repo are arguments ($1 to $4),
    /// never part of the script text, so spaces and quotes in a path cannot change what runs.
    static let script = #"curl -fsSL "$1" | SEEDWELL_REPO_URL="$4" SEEDWELL_INSTALL_DIR="$2" bash -s -- --version "$3""#

    static func releaseBase(_ repo: String) -> String { repo.hasSuffix("/") ? String(repo.dropLast()) : repo }

    static func latestURL(repo: String) -> URL? { URL(string: releaseBase(repo) + "/releases/latest/download/latest.json") }

    static func installerURL(repo: String, version: String) -> String { releaseBase(repo) + "/releases/download/v\(version)/install.sh" }

    /// argv of the shell that runs the installer; `argv[0]` is the shell.
    static func arguments(repo: String, version: String, bundleURL: URL) -> [String] {
        ["/bin/bash", "-c", script, "seedwell-update",
         installerURL(repo: repo, version: version),
         bundleURL.deletingLastPathComponent().path,
         version,
         releaseBase(repo)]
    }
}
