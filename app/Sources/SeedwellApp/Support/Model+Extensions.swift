import AppKit
import UniformTypeIdentifiers

extension TransferRow: Identifiable {}

extension TransferRow {
    var isDownload: Bool { seedGroup == nil && kind == .torrent || kind == .direct && state != .completed }
    /// What a download's progress counts. A finished torrent that downloads again starts at 0 for the
    /// new files (the engine's session); rows without a session figure use the wanted bytes held.
    var progressDone: UInt64 { sessionSize > 0 ? min(sessionDone, sessionSize) : min(doneBytes, sizeBytes) }
    var progressSize: UInt64 { sessionSize > 0 ? sessionSize : sizeBytes }
    var fraction: Double { progressSize == 0 ? 0 : min(1, Double(progressDone) / Double(progressSize)) }
    var percent: Int { Int((fraction * 100).rounded(.down)) }
    var url: URL { URL(fileURLWithPath: savePath) }
    /// Seeding rows: the Sharing view.
    var isSharing: Bool { seedGroup != nil }
    var isPaused: Bool { state == .paused }
}

extension SeedGroup: CaseIterable {
    public static var allCases: [SeedGroup] { [.popular, .active, .dormant, .error, .dead, .stopped] }

    var title: String {
        switch self {
        case .popular: "Popular"
        case .active: "Active"
        case .dormant: "Dormant"
        case .error: "Error"
        case .dead: "Dead"
        case .stopped: "Stopped"
        }
    }
}

extension SharingCounts {
    func count(_ g: SeedGroup) -> Int {
        switch g {
        case .popular: Int(popular)
        case .active: Int(active)
        case .dormant: Int(dormant)
        case .error: Int(error)
        case .dead: Int(dead)
        case .stopped: Int(stopped)
        }
    }
    var total: Int { SeedGroup.allCases.reduce(0) { $0 + count($1) } }
}

extension Category {
    static let all: [Category] = [.movie, .tv, .music, .audiobook, .book, .software, .game, .other]

    var label: String {
        switch self {
        case .movie: "Movie"
        case .tv: "TV"
        case .music: "Music"
        case .audiobook: "Audiobook"
        case .book: "Book"
        case .software: "App"
        case .game: "Game"
        case .other: "Other"
        }
    }

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

    /// Folder-style name used in the "Recently Finished" line.
    var plural: String {
        switch self {
        case .movie: "Movies"
        case .tv: "TV"
        case .music: "Music"
        case .audiobook: "Audiobooks"
        case .book: "Books"
        case .software: "Apps"
        case .game: "Games"
        case .other: "Other"
        }
    }
}

extension Priority {
    static let all: [Priority] = [.high, .normal, .low]
    var label: String {
        switch self {
        case .high: "High"
        case .normal: "Normal"
        case .low: "Low"
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
