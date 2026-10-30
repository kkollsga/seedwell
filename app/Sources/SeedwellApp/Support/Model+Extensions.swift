import AppKit
import UniformTypeIdentifiers

extension TransferRow: Identifiable {}

extension TransferRow {
    var url: URL { URL(fileURLWithPath: savePath) }
    /// Seeding rows: the Sharing view.
    var isSharing: Bool { seedGroup != nil }
}

extension SeedGroup: CaseIterable {
    public static var allCases: [SeedGroup] { [.popular, .active, .dormant, .error, .dead, .stopped] }
}

extension SharingCounts {
    var total: Int { Int(popular + active + dormant + error + dead + stopped) }
}

extension Snapshot {
    /// Every download, including those of a tracker in hidden mode (SPEC 7E.5). `downloading`, `queued`
    /// and `recent` are what the Downloads tab and the menu list; a tracker's own tab, the sleep assertion,
    /// the chooser and the notifications need all of them.
    var allDownloading: [TransferRow] { downloading + hiddenDownloading }
    var allQueued: [TransferRow] { queued + hiddenQueued }
    var allRecent: [TransferRow] { recent + hiddenRecent }
}

extension Category {
    static let all: [Category] = [.movie, .tv, .music, .audiobook, .book, .software, .game, .other]

    /// "Movie", "TV", "App" (the core's words).
    var label: String { categoryLabel(category: self) }

    /// The one symbol of a category, in the pill, the chooser card and Settings.
    var symbol: String {
        switch self {
        case .movie: "film"
        case .tv: "tv"
        case .music: "music.note"
        case .audiobook: "headphones"
        case .book: "book"
        case .software: "opticaldisc"
        case .game: "gamecontroller"
        case .other: "shippingbox"
        }
    }
}

enum Windows {
    /// Every window is pinned to sRGB. With the display's own wide-gamut profile AppKit builds one
    /// colour conversion per colour-space pair (about 140 KB per tone curve) and never frees it;
    /// sRGB saves 2 to 3 MB for the rest of the run. The UI is neutral greys plus the system accent.
    static let space = NSColorSpace.sRGB
}

/// Finder icons by file extension (better than anything we could draw), cached per extension.
@MainActor enum FileIcons {
    private static var cache: [String: NSImage] = [:]

    static func reset() { cache = [:] }

    /// The system hands out icons with every size up to 1024 px (about a megabyte each). The rows
    /// show them at 38 pt: keep one 2x bitmap and let the big one go.
    private static func rendered(_ source: NSImage, points: CGFloat = 40) -> NSImage {
        let px = Int(points * 2)
        // sRGB like the windows (see `Windows.sRGB`): no colour conversion between icon and window.
        guard let space = CGColorSpace(name: CGColorSpace.sRGB),
              let cg = CGContext(data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0, space: space,
                                 bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return source }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: cg, flipped: false)
        source.draw(in: NSRect(x: 0, y: 0, width: px, height: px), from: .zero, operation: .copy, fraction: 1)
        NSGraphicsContext.restoreGraphicsState()
        guard let image = cg.makeImage() else { return source }
        return NSImage(cgImage: image, size: NSSize(width: points, height: points))
    }

    static func icon(ext: String) -> NSImage {
        if let hit = cache[ext] { return hit }
        let type = ext.isEmpty ? UTType.folder : (UTType(filenameExtension: ext) ?? .data)
        let image = Self.rendered(NSWorkspace.shared.icon(for: type))
        cache[ext] = image
        return image
    }
}
