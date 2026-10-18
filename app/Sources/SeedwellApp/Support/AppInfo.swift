import Foundation

/// The user-visible product name, in one place. Every string, window title and notification that
/// names the app uses it.
enum AppInfo {
    static let name = "Seedwell"

    /// `CFBundleShortVersionString` (from the workspace Cargo.toml via build.sh), "dev" outside a bundle.
    static var version: String { Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "dev" }
    /// `CFBundleVersion`: the git commit count at build time.
    static var build: String { Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "0" }
    /// "Seedwell 0.1.0 (build 123)".
    static var versionLine: String { "\(name) \(version) (build \(build))" }
}
