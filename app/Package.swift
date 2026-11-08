// swift-tools-version: 6.0
import Foundation
import PackageDescription

// build.sh builds the Rust static library into build/rust and generates the UniFFI bindings
// (Sources/seedwell_ffiFFI/include and Sources/SeedwellApp/Generated); this manifest only links them.
let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().path

// The app extensions' executables are linked as Xcode links an app extension: the process starts in Foundation's
// NSExtensionMain (`-e`), there is no main and no Swift entry point (each executable is a C target with no code,
// Sources/ExtensionEntry/SeedwellFileProvider/entry.c says why), and `-ObjC` keeps every Objective-C class of the
// Swift code it links, the principal class (NSExtensionPrincipalClass) among them, which nothing references by name.
let appExtensionLinkerFlags = ["-Xlinker", "-application_extension", "-Xlinker", "-e", "-Xlinker", "_NSExtensionMain",
                               "-Xlinker", "-ObjC"]

let package = Package(
    name: "SeedwellApp",
    platforms: [.macOS(.v15)],
    targets: [
        // Sparkle 2.10.1 (SPEC 12 #19), fetched and checksum-pinned by fetch-sparkle.sh; build.sh embeds it in Contents/Frameworks.
        .binaryTarget(name: "Sparkle", path: ".sparkle/Sparkle.xcframework"),
        .target(name: "seedwell_ffiFFI", path: "Sources/seedwell_ffiFFI", publicHeadersPath: "include"),
        // The status file and badge rules shared by the app and its Finder extension (SPEC 7G.5).
        .target(name: "SeedwellFinderShared", path: "Sources/SeedwellFinderShared",
                swiftSettings: [.unsafeFlags(["-application-extension"])]),
        // The Finder extension's code; its executable (below) is wrapped by build.sh into
        // Seedwell.app/Contents/PlugIns/SeedwellFinderSync.appex.
        .target(name: "SeedwellFinderSyncExtension", dependencies: ["SeedwellFinderShared"], path: "Sources/SeedwellFinderSync",
                swiftSettings: [.unsafeFlags(["-application-extension"])]),
        .executableTarget(
            name: "SeedwellFinderSync",
            dependencies: ["SeedwellFinderSyncExtension"],
            path: "Sources/ExtensionEntry/SeedwellFinderSync",
            linkerSettings: [.linkedFramework("FinderSync"), .unsafeFlags(appExtensionLinkerFlags)]
        ),
        // On-demand folders in the Finder (SPEC 7G.11): what the File Provider extension shares with the app and the
        // tests (the socket client, the wire, the mapping onto File Provider's types).
        .target(name: "SeedwellFileProviderShared", path: "Sources/SeedwellFileProviderShared",
                swiftSettings: [.unsafeFlags(["-application-extension"])],
                linkerSettings: [.linkedFramework("FileProvider")]),
        // The File Provider extension's code; its executable (below) is wrapped by build.sh into
        // Seedwell.app/Contents/PlugIns/SeedwellFileProvider.appex.
        .target(name: "SeedwellFileProviderExtension", dependencies: ["SeedwellFileProviderShared"],
                path: "Sources/SeedwellFileProvider", swiftSettings: [.unsafeFlags(["-application-extension"])]),
        .executableTarget(
            name: "SeedwellFileProvider",
            dependencies: ["SeedwellFileProviderExtension"],
            path: "Sources/ExtensionEntry/SeedwellFileProvider",
            linkerSettings: [.linkedFramework("FileProvider"), .unsafeFlags(appExtensionLinkerFlags)]
        ),
        // Seedwell in the system Share menu: its code, and its executable, which build.sh wraps into
        // Seedwell.app/Contents/PlugIns/SeedwellShare.appex.
        .target(name: "SeedwellShareExtension", path: "Sources/SeedwellShare",
                swiftSettings: [.unsafeFlags(["-application-extension"])]),
        .executableTarget(
            name: "SeedwellShare",
            dependencies: ["SeedwellShareExtension"],
            path: "Sources/ExtensionEntry/SeedwellShare",
            linkerSettings: [.unsafeFlags(appExtensionLinkerFlags)]
        ),
        .executableTarget(
            name: "SeedwellApp",
            dependencies: ["seedwell_ffiFFI", "Sparkle", "SeedwellFinderShared", "SeedwellFileProviderShared"],
            path: "Sources/SeedwellApp",
            linkerSettings: [.unsafeFlags([
                "-L\(root)/build/rust", "-lseedwell_ffi",
                // Sparkle.framework lives in Seedwell.app/Contents/Frameworks.
                "-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks",
            ])]
        ),
        // Swift Testing: `swift test` (detection, ordering and launch plans of the video players).
        .testTarget(name: "SeedwellAppTests", dependencies: ["SeedwellApp", "SeedwellFinderShared", "SeedwellFileProviderShared"], path: "Tests/SeedwellAppTests"),
    ]
)
