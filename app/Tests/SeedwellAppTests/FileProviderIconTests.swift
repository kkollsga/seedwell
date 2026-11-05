import AppKit
import Testing

/// The Finder sidebar's icon for an on-demand folder (SPEC 7G.11), read from the built bundle (`app/build.sh` runs
/// before these tests, locally in `tools/check.sh` and on CI). Finder has no API that answers which icon it drew:
/// `NSWorkspace.icon(forFile:)` is a plain folder for every File Provider domain's root, OneDrive's too. These tests
/// look the symbol up as the sidebar does: the name in the extension's Info.plist, in the extension's bundle.
/// The extension in the app `app/build.sh` built last.
private let builtFileProviderExtension = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    .appendingPathComponent("build/Seedwell.app/Contents/PlugIns/SeedwellFileProvider.appex").path

@Suite(.timeLimit(.minutes(1)), .enabled(if: FileManager.default.fileExists(atPath: builtFileProviderExtension)))
struct FileProviderIconTests {
    static let extensionPath = builtFileProviderExtension

    /// Fails when the sidebar would show a plain folder: the extension's Info.plist names no symbol, or one its bundle
    /// does not have (the catalog not compiled, built without Xcode's actool, or compiled under another name), or the
    /// symbol is there but draws nothing.
    @Test func theSidebarSymbolIsSeedwellsAndDraws() throws {
        let bundle = try #require(Bundle(path: Self.extensionPath))
        let icons = bundle.object(forInfoDictionaryKey: "CFBundleIcons") as? [String: Any]
        let name = (icons?["CFBundlePrimaryIcon"] as? [String: Any])?["CFBundleSymbolName"] as? String
        #expect(name == "seedwell", "CFBundleIcons › CFBundlePrimaryIcon › CFBundleSymbolName (no actool when the app was built?)")
        let symbol = try #require(NSImage(symbolName: name ?? "", bundle: bundle, variableValue: 1),
                                  "the extension's Assets.car has no symbol \(name ?? "")")
        #expect(symbol.isTemplate, "the sidebar tints its icons: the symbol must be a template")

        // Drawn black at the sidebar's size, the two arrows cover some of the square, not none of it and not all.
        let px = 64
        let rep = try #require(NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
                                                samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                                                bytesPerRow: 0, bitsPerPixel: 0))
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        let sized = symbol.withSymbolConfiguration(.init(pointSize: 40, weight: .regular)) ?? symbol
        sized.draw(in: NSRect(x: 0, y: 0, width: px, height: px))
        NSGraphicsContext.restoreGraphicsState()
        var ink = 0
        for y in 0..<px { for x in 0..<px where (rep.colorAt(x: x, y: y)?.alphaComponent ?? 0) > 0.5 { ink += 1 } }
        let share = Double(ink) / Double(px * px)
        #expect(share > 0.05 && share < 0.6, "ink covers \(share) of the symbol's square")
    }

    /// Fails when the extension names an icon file its bundle lacks (the system then shows a generic extension icon
    /// where it shows the extension's own).
    @Test func theIconFileIsInTheExtension() throws {
        let bundle = try #require(Bundle(path: Self.extensionPath))
        let file = try #require(bundle.object(forInfoDictionaryKey: "CFBundleIconFile") as? String)
        let url = try #require(bundle.url(forResource: file, withExtension: "icns"), "no \(file).icns in the extension")
        #expect(NSImage(contentsOf: url)?.isValid == true)
    }
}
