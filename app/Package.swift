// swift-tools-version: 6.0
import Foundation
import PackageDescription

// build.sh builds the Rust static library into build/rust and generates the UniFFI bindings
// (Sources/trs_ffiFFI/include and Sources/SeedwellApp/Generated); this manifest only links them.
let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().path

let package = Package(
    name: "SeedwellApp",
    platforms: [.macOS(.v15)],
    targets: [
        // Sparkle 2.10.1 (SPEC 12 #19), fetched and checksum-pinned by fetch-sparkle.sh; build.sh embeds it in Contents/Frameworks.
        .binaryTarget(name: "Sparkle", path: ".sparkle/Sparkle.xcframework"),
        .target(name: "trs_ffiFFI", path: "Sources/trs_ffiFFI", publicHeadersPath: "include"),
        // The status file and badge rules shared by the app and its Finder extension (SPEC 7G.5).
        .target(name: "SeedwellFinderShared", path: "Sources/SeedwellFinderShared",
                swiftSettings: [.unsafeFlags(["-application-extension"])]),
        // The Finder extension; build.sh wraps it into Seedwell.app/Contents/PlugIns/SeedwellFinderSync.appex.
        .executableTarget(
            name: "SeedwellFinderSync",
            dependencies: ["SeedwellFinderShared"],
            path: "Sources/SeedwellFinderSync",
            swiftSettings: [.unsafeFlags(["-application-extension"])],
            linkerSettings: [.linkedFramework("FinderSync"), .unsafeFlags(["-Xlinker", "-application_extension"])]
        ),
        // On-demand folders in the Finder (SPEC 7G.11): what the File Provider extension shares with the app and the
        // tests (the socket client, the wire, the mapping onto File Provider's types).
        .target(name: "SeedwellFileProviderShared", path: "Sources/SeedwellFileProviderShared",
                swiftSettings: [.unsafeFlags(["-application-extension"])],
                linkerSettings: [.linkedFramework("FileProvider")]),
        // The File Provider extension; build.sh wraps it into Seedwell.app/Contents/PlugIns/SeedwellFileProvider.appex.
        .executableTarget(
            name: "SeedwellFileProvider",
            dependencies: ["SeedwellFileProviderShared"],
            path: "Sources/SeedwellFileProvider",
            swiftSettings: [.unsafeFlags(["-application-extension"])],
            linkerSettings: [.linkedFramework("FileProvider"), .unsafeFlags(["-Xlinker", "-application_extension"])]
        ),
        // Seedwell in the system Share menu; build.sh wraps it into Seedwell.app/Contents/PlugIns/SeedwellShare.appex.
        .executableTarget(
            name: "SeedwellShare",
            path: "Sources/SeedwellShare",
            swiftSettings: [.unsafeFlags(["-application-extension"])],
            linkerSettings: [.unsafeFlags(["-Xlinker", "-application_extension"])]
        ),
        .executableTarget(
            name: "SeedwellApp",
            dependencies: ["trs_ffiFFI", "Sparkle", "SeedwellFinderShared", "SeedwellFileProviderShared"],
            path: "Sources/SeedwellApp",
            linkerSettings: [.unsafeFlags([
                "-L\(root)/build/rust", "-ltrs_ffi",
                // Sparkle.framework lives in Seedwell.app/Contents/Frameworks.
                "-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks",
            ])]
        ),
        // Swift Testing: `swift test` (detection, ordering and launch plans of the video players).
        .testTarget(name: "SeedwellAppTests", dependencies: ["SeedwellApp", "SeedwellFinderShared", "SeedwellFileProviderShared"], path: "Tests/SeedwellAppTests"),
    ]
)
