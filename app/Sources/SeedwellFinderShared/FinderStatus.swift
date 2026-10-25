import AppKit

/// What the app tells its Finder extension (SPEC 7G.5): one small JSON file the app rewrites when something
/// changes, and the extension reads (it runs sandboxed and can read only that folder). Shared by both, so the
/// format and the badge rules are one piece of code.
public struct FinderStatus: Codable, Equatable, Sendable {
    public static let fileName = "status.json"
    /// Bumped when the format changes incompatibly; an extension that does not know it shows no badges.
    public static let currentVersion = 1

    public var version = FinderStatus.currentVersion
    /// The app is running. When it quits it says so, and the folders' own badges turn grey.
    public var running: Bool
    public var folders: [Folder]

    public init(running: Bool, folders: [Folder]) {
        self.running = running
        self.folders = folders
    }

    /// One synced folder. Paths below the root are relative, `/`-separated, as the core names them.
    public struct Folder: Codable, Equatable, Sendable {
        public var id: String
        public var name: String
        /// Absolute, symlinks resolved (what Finder hands the extension).
        public var path: String
        /// The folder's own badge.
        public var badge: FinderBadge
        /// Paths still to fetch or delete (the first ones); `pendingMore` when there are more.
        public var pending: [String]
        public var pendingMore: Bool
        public var conflicts: [String]
        /// Paths that cannot be synced.
        public var issues: [String]
        /// Everything not listed above is in sync: nothing is pending beyond the list, and the folder has
        /// no ignore patterns of its own (an ignored file would otherwise get a tick).
        public var complete: Bool
        /// This Mac may share the folder (its owner or an admin): Finder's menu offers Share….
        public var canShare: Bool

        public init(id: String, name: String, path: String, badge: FinderBadge, pending: [String] = [], pendingMore: Bool = false,
                    conflicts: [String] = [], issues: [String] = [], complete: Bool = true, canShare: Bool = false) {
            self.id = id
            self.name = name
            self.path = path
            self.badge = badge
            self.pending = pending
            self.pendingMore = pendingMore
            self.conflicts = conflicts
            self.issues = issues
            self.complete = complete
            self.canShare = canShare
        }
    }

    /// Writes the status atomically into `dir` (created when missing).
    public func write(to dir: URL) throws {
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let enc = JSONEncoder()
        enc.outputFormatting = [.sortedKeys]
        try enc.encode(self).write(to: dir.appendingPathComponent(Self.fileName), options: .atomic)
    }

    /// The status in `dir`; `nil` when there is none or it cannot be read (no badges then).
    public static func read(from dir: URL) -> FinderStatus? {
        guard let data = try? Data(contentsOf: dir.appendingPathComponent(fileName)),
              let s = try? JSONDecoder().decode(FinderStatus.self, from: data), s.version == currentVersion else { return nil }
        return s
    }
}

/// The badges Finder shows. The raw value is the badge identifier the extension registers.
public enum FinderBadge: String, Codable, CaseIterable, Sendable {
    case synced, syncing, downloading, uploading, paused, offline, conflict, error

    /// What Finder may show next to the badge (and VoiceOver reads).
    public var label: String {
        switch self {
        case .synced: "Up to date"
        case .syncing: "Syncing"
        case .downloading: "Downloading"
        case .uploading: "Uploading"
        case .paused: "Paused"
        case .offline: "No devices online"
        case .conflict: "Conflict"
        case .error: "Needs attention"
        }
    }
}

/// Answers "which badge for this path" quickly: built once per status, asked for every item Finder shows.
public struct FinderBadges: Sendable {
    private struct Entry: Sendable {
        let folder: FinderStatus.Folder
        let root: String
        let pending: Set<String>, pendingDirs: Set<String>
        let conflicts: Set<String>, conflictDirs: Set<String>
        let issues: Set<String>, issueDirs: Set<String>
    }

    private let entries: [Entry]
    private let running: Bool

    public init(_ status: FinderStatus?) {
        running = status?.running ?? false
        entries = (status?.folders ?? []).map { f in
            func split(_ paths: [String]) -> (Set<String>, Set<String>) {
                let set = Set(paths.map(FinderBadges.normalized))
                var dirs = Set<String>()
                for p in set {
                    var parts = p.split(separator: "/").dropLast()
                    while !parts.isEmpty {
                        dirs.insert(parts.joined(separator: "/"))
                        parts = parts.dropLast()
                    }
                }
                return (set, dirs)
            }
            let (p, pd) = split(f.pending), (c, cd) = split(f.conflicts), (i, id) = split(f.issues)
            return Entry(folder: f, root: FinderBadges.normalized(f.path), pending: p, pendingDirs: pd,
                         conflicts: c, conflictDirs: cd, issues: i, issueDirs: id)
        }
        // Longest root first: a folder synced inside another one answers for its own items.
        .sorted { $0.root.count > $1.root.count }
    }

    /// The synced folders' roots, for `directoryURLs`.
    public var roots: [String] { entries.map(\.root) }

    /// The synced folder an absolute path is in (the innermost one), compared as the badges are.
    public func folder(containing absolutePath: String) -> FinderStatus.Folder? {
        let path = Self.normalized(absolutePath)
        return entries.first { path == $0.root || path.hasPrefix($0.root + "/") }?.folder
    }

    /// The badge for an absolute path, `nil` for none (outside every folder, ignored, or unknown).
    public func badge(for absolutePath: String) -> FinderBadge? {
        let path = Self.normalized(absolutePath)
        for e in entries {
            if path == e.root { return running ? e.folder.badge : .offline }
            guard path.hasPrefix(e.root + "/") else { continue }
            let rel = String(path.dropFirst(e.root.count + 1))
            if rel.isEmpty || Self.ignoredByDefault(rel) { return nil }
            if e.issues.contains(rel) || e.issueDirs.contains(rel) { return .error }
            if e.conflicts.contains(rel) || e.conflictDirs.contains(rel) { return .conflict }
            // The folder's own state decides the rest. Paused, failed or not running: the folder's badge says
            // it all. With no device online, what is pending waits and the rest is as synced as it was.
            let root = e.folder.badge
            guard running, ![.paused, .error].contains(root) else {
                return running && root == .paused && (e.pending.contains(rel) || e.pendingDirs.contains(rel)) ? .paused : nil
            }
            if e.pending.contains(rel) || e.pendingDirs.contains(rel) { return root == .offline ? .offline : .downloading }
            return e.folder.complete && !e.folder.pendingMore ? .synced : nil
        }
        return nil
    }

    /// Finder's paths and the core's can differ in Unicode form (NFD from old HFS+ names, NFC on the wire), and
    /// Finder may name a folder on the data volume by its firmlinked path (`/System/Volumes/Data/private/tmp`).
    static func normalized(_ path: String) -> String {
        var p = path.precomposedStringWithCanonicalMapping
        let data = "/System/Volumes/Data/"
        if p.hasPrefix(data) { p.removeFirst(data.count - 1) }
        while p.count > 1, p.hasSuffix("/") { p.removeLast() }
        return p
    }

    /// The core's default ignores (`crates/sync/src/ignore.rs`, `DEFAULTS`), which never sync: no badge.
    static func ignoredByDefault(_ rel: String) -> Bool {
        let parts = rel.split(separator: "/").map { $0.lowercased() }
        if parts.first == ".seedwell" { return true }
        return parts.contains { n in
            [".ds_store", "icon\r", ".spotlight-v100", ".trashes", ".fseventsd", ".temporaryitems", ".documentrevisions-v100"].contains(n)
                || n.hasPrefix("._") || n.hasPrefix("~$") || (n.hasPrefix(".~lock.") && n.hasSuffix("#"))
                || (n.hasPrefix(".seedwell.") && n.hasSuffix(".tmp"))
        }
    }
}

public extension FinderBadge {
    /// A bold glyph in the badge's colour with a soft shadow on a clear background, as a ready bitmap (128 px; Finder scales it to the
    /// icon size, and the app's setup window shows the same ones). No disc behind it: at list-view size a white
    /// glyph on a disc read as a plain dot, and a disc looks like a system icon. The glyphs are plain paths drawn
    /// here once, not SF Symbols drawn lazily (which in the sandboxed extension could come out empty).
    var image: NSImage {
        let px = 128
        guard let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8, samplesPerPixel: 4,
                                         hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0),
              let ctx = NSGraphicsContext(bitmapImageRep: rep) else { return NSImage(size: NSSize(width: 64, height: 64)) }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = ctx
        let s = CGFloat(px)
        // A soft shadow keeps the glyph visible on light, dark and selected rows; the glyph is a little smaller
        // than the square so the shadow is not cut off.
        let shadow = NSShadow()
        shadow.shadowColor = NSColor.black.withAlphaComponent(0.5)
        shadow.shadowBlurRadius = s * 0.05
        shadow.shadowOffset = NSSize(width: 0, height: -s * 0.02)
        shadow.set()
        var fit = AffineTransform(translationByX: s / 2, byY: s / 2)
        fit.scale(0.86)
        fit.translate(x: -s / 2, y: -s / 2)
        let paths = glyph(in: s)
        for p in paths {
            p.transform(using: fit)
            p.lineCapStyle = .round
            p.lineJoinStyle = .round
        }
        // A thin light rim under the glyph, so a blue glyph still shows on a selected (blue) row.
        NSColor.white.set()
        for p in paths {
            p.lineWidth = s * 0.24
            p.stroke()
        }
        NSShadow().set()
        color.set()
        for p in paths {
            p.lineWidth = s * 0.14
            p.stroke()
        }
        NSGraphicsContext.restoreGraphicsState()
        let image = NSImage(size: NSSize(width: 64, height: 64))
        image.addRepresentation(rep)
        image.accessibilityDescription = label
        return image
    }

    private var color: NSColor {
        switch self {
        case .synced: .systemGreen
        case .syncing, .downloading, .uploading: .systemBlue
        case .paused, .offline: .systemGray
        case .conflict: .systemOrange
        case .error: .systemRed
        }
    }

    /// The glyph as strokes on an `s` × `s` square (origin bottom left), filling most of it.
    private func glyph(in s: CGFloat) -> [NSBezierPath] {
        func line(_ pts: [(CGFloat, CGFloat)]) -> NSBezierPath {
            let p = NSBezierPath()
            for (i, (x, y)) in pts.enumerated() {
                let pt = NSPoint(x: x * s, y: y * s)
                if i == 0 { p.move(to: pt) } else { p.line(to: pt) }
            }
            return p
        }
        switch self {
        case .synced:
            return [line([(0.14, 0.52), (0.4, 0.26), (0.86, 0.74)])]
        case .downloading:
            return [line([(0.5, 0.86), (0.5, 0.16)]), line([(0.22, 0.44), (0.5, 0.15), (0.78, 0.44)])]
        case .uploading:
            return [line([(0.5, 0.14), (0.5, 0.84)]), line([(0.22, 0.56), (0.5, 0.85), (0.78, 0.56)])]
        case .syncing:
            // Most of a circle, and an arrowhead at its end pointing the way it turns.
            let (r, end): (CGFloat, CGFloat) = (0.32, 60 * .pi / 180)
            let arc = NSBezierPath()
            arc.appendArc(withCenter: NSPoint(x: s / 2, y: s / 2), radius: s * r, startAngle: 120, endAngle: 420)
            let tip = (0.5 + r * cos(end), 0.5 + r * sin(end))
            // The arc turns anticlockwise, so at its end it heads along `end + 90°`; the wings point back from there.
            let back = { (turn: CGFloat) -> (CGFloat, CGFloat) in
                (tip.0 - 0.18 * cos(end + .pi / 2 + turn), tip.1 - 0.18 * sin(end + .pi / 2 + turn))
            }
            return [arc, line([back(0.7), tip, back(-0.7)])]
        case .paused:
            return [line([(0.36, 0.2), (0.36, 0.8)]), line([(0.64, 0.2), (0.64, 0.8)])]
        case .offline:
            let ring = NSBezierPath(ovalIn: NSRect(x: s * 0.2, y: s * 0.2, width: s * 0.6, height: s * 0.6))
            return [ring]
        case .conflict:
            return [line([(0.5, 0.86), (0.5, 0.42)]), line([(0.5, 0.16), (0.5, 0.15)])]
        case .error:
            return [line([(0.22, 0.22), (0.78, 0.78)]), line([(0.22, 0.78), (0.78, 0.22)])]
        }
    }
}
