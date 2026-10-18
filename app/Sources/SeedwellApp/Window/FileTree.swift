import Foundation

/// The folder tree of one torrent's files (the expandable list under a row in Downloads and Sharing).
/// A plain value type with no UI: built once from the engine's file list, then only its numbers change
/// (progress every second, priorities when the user edits).
struct FileTree: Sendable {
    enum Check { case on, off, mixed }

    struct Node: Sendable {
        var name: String
        /// Path inside the torrent, `/`-separated (a folder's own path for a folder).
        var path: String
        /// Where the file is (or will be) on disk.
        var disk = ""
        var parent: Int
        var children: [Int] = []
        /// Index in the torrent's file list; nil for a folder.
        var file: UInt32?
        var priority: FilePriority = .normal
        var isVideo = false
        /// Lower-case extension for the icon ("" for a folder).
        var ext = ""
        // Aggregates (a file's own numbers for a file).
        var length: UInt64 = 0
        var done: UInt64 = 0
        var fileCount = 0
        var wantedCount = 0
        var wantedLength: UInt64 = 0
        var wantedDone: UInt64 = 0
        /// Bit per priority among the wanted files below (1 low, 2 normal, 4 high).
        var prioMask: UInt8 = 0
        /// Bytes served from the file since the torrent went live (the engine's running count).
        var uploaded: UInt64 = 0
        /// Smoothed bytes per second, worked out from the change between two refreshes (a folder: its files' sum).
        var downRate: Double = 0
        var upRate: Double = 0

        var isFolder: Bool { file == nil }
    }

    /// When the numbers were last read (the rates are changes per second between two reads).
    private var sampled: Date?

    /// Node 0 is the invisible root; a parent always has a smaller id than its children.
    private(set) var nodes: [Node] = []
    /// File list index to node id (-1 for an index the tree does not hold: padding).
    private var nodeOfFile: [Int] = []
    var top: [Int] { nodes.isEmpty ? [] : nodes[0].children }

    init(files: [TorrentFile]) {
        nodes = [Node(name: "", path: "", parent: -1)]
        var folders: [String: Int] = [:]
        let maxIndex = Int(files.map(\.index).max() ?? 0)
        nodeOfFile = Array(repeating: -1, count: files.isEmpty ? 0 : maxIndex + 1)
        for f in files {
            let comps = f.path.split(separator: "/").map(String.init)
            guard let leaf = comps.last else { continue }
            var parent = 0
            var prefix = ""
            for c in comps.dropLast() {
                prefix = prefix.isEmpty ? c : prefix + "/" + c
                if let id = folders[prefix] { parent = id; continue }
                let id = nodes.count
                nodes.append(Node(name: c, path: prefix, parent: parent))
                nodes[parent].children.append(id)
                folders[prefix] = id
                parent = id
            }
            let id = nodes.count
            nodes.append(Node(name: leaf, path: f.path, disk: f.diskPath, parent: parent, file: f.index, priority: f.priority, isVideo: f.isVideo,
                              ext: (leaf as NSString).pathExtension.lowercased(), length: f.length, done: min(f.done, f.length)))
            nodes[id].uploaded = f.uploaded
            nodes[parent].children.append(id)
            nodeOfFile[Int(f.index)] = id
        }
        sampled = Date()
        sortChildren()
        recompute()
    }

    /// Folders first, then files, each in Finder's "standard" order (2 before 10).
    private mutating func sortChildren() {
        for id in nodes.indices where nodes[id].children.count > 1 {
            let kids = nodes[id].children
            nodes[id].children = kids.sorted { a, b in
                let (x, y) = (nodes[a], nodes[b])
                if x.isFolder != y.isFolder { return x.isFolder }
                return x.name.localizedStandardCompare(y.name) == .orderedAscending
            }
        }
    }

    /// Folder aggregates from the files, bottom-up (ids run parent before child, so backwards works).
    private mutating func recompute() {
        for i in nodes.indices where nodes[i].isFolder {
            nodes[i].length = 0; nodes[i].done = 0; nodes[i].fileCount = 0; nodes[i].wantedCount = 0
            nodes[i].wantedLength = 0; nodes[i].wantedDone = 0; nodes[i].prioMask = 0
            nodes[i].uploaded = 0; nodes[i].downRate = 0; nodes[i].upRate = 0
        }
        for i in nodes.indices where !nodes[i].isFolder {
            let wanted = nodes[i].priority != .skip
            nodes[i].fileCount = 1
            nodes[i].wantedCount = wanted ? 1 : 0
            nodes[i].wantedLength = wanted ? nodes[i].length : 0
            nodes[i].wantedDone = wanted ? nodes[i].done : 0
            nodes[i].prioMask = Self.bit(nodes[i].priority)
        }
        for i in stride(from: nodes.count - 1, to: 0, by: -1) {
            let n = nodes[i]
            let p = n.parent
            nodes[p].length += n.length
            nodes[p].done += n.done
            nodes[p].fileCount += n.fileCount
            nodes[p].wantedCount += n.wantedCount
            nodes[p].wantedLength += n.wantedLength
            nodes[p].wantedDone += n.wantedDone
            nodes[p].prioMask |= n.prioMask
            nodes[p].uploaded += n.uploaded
            nodes[p].downRate += n.downRate
            nodes[p].upRate += n.upRate
        }
    }

    private static func bit(_ p: FilePriority) -> UInt8 {
        switch p {
        case .skip: 0
        case .low: 1
        case .normal: 2
        case .high: 4
        }
    }

    // MARK: Totals

    var fileCount: Int { nodes.first?.fileCount ?? 0 }
    var wantedCount: Int { nodes.first?.wantedCount ?? 0 }
    var totalLength: UInt64 { nodes.first?.length ?? 0 }
    var wantedLength: UInt64 { nodes.first?.wantedLength ?? 0 }

    // MARK: Per node

    func check(_ id: Int) -> Check {
        let n = nodes[id]
        if n.wantedCount == 0 { return .off }
        return n.wantedCount == n.fileCount ? .on : .mixed
    }

    /// Progress of what is wanted below (a file: its own), 0...1. A folder with nothing wanted shows nothing.
    func progress(_ id: Int) -> Double {
        let n = nodes[id]
        if n.isFolder {
            if n.wantedLength == 0 { return n.wantedCount > 0 ? 1 : 0 }
            return Double(n.wantedDone) / Double(n.wantedLength)
        }
        if n.length == 0 { return 1 }
        return Double(n.done) / Double(n.length)
    }

    /// How much of the size capsule is filled, 0...1: a file's downloaded part (nothing for a skipped file
    /// with nothing on disk), a folder's share of all its files' bytes.
    func fillFraction(_ id: Int) -> Double {
        let n = nodes[id]
        if n.isFolder {
            return n.length == 0 ? 0 : min(1, Double(n.done) / Double(n.length))
        }
        if n.length == 0 { return n.priority == .skip ? 0 : 1 }
        return min(1, Double(n.done) / Double(n.length))
    }

    /// "↓ 1.2 MB/s" or "↑ 340 KB/s" (download first); nil below 1 KB/s so a quiet file shows nothing.
    func rateLabel(_ id: Int) -> String? {
        Self.rateLabel(down: nodes[id].downRate, up: nodes[id].upRate)
    }

    static func rateLabel(down: Double, up: Double) -> String? {
        if down >= 1_000 { return "↓ " + Fmt.rate(UInt64(down)) }
        if up >= 1_000 { return "↑ " + Fmt.rate(UInt64(up)) }
        return nil
    }

    /// The next smoothed rate: half of the old one, half of what the last `dt` seconds moved.
    static func smoothed(_ old: Double, moved: UInt64, dt: Double) -> Double {
        guard dt > 0 else { return old }
        let r = 0.5 * old + 0.5 * Double(moved) / dt
        return r < 200 ? 0 : r
    }

    func isComplete(_ id: Int) -> Bool {
        let n = nodes[id]
        return n.isFolder ? (n.wantedCount > 0 && n.wantedDone >= n.wantedLength) : (n.done >= n.length)
    }

    /// The priority a node shows: a file's own; a folder's when all its wanted files agree, else nil (mixed).
    /// Skip when nothing below is wanted.
    func shownPriority(_ id: Int) -> FilePriority? {
        let n = nodes[id]
        if !n.isFolder { return n.priority }
        switch n.prioMask {
        case 0: return .skip
        case 1: return .low
        case 2: return .normal
        case 4: return .high
        default: return nil
        }
    }

    /// File nodes at or below `ids`.
    func fileNodes(under ids: [Int]) -> [Int] {
        var out: [Int] = []
        var stack = ids
        while let i = stack.popLast() {
            if nodes[i].isFolder { stack.append(contentsOf: nodes[i].children) } else { out.append(i) }
        }
        return out
    }

    // MARK: Edits (return what the engine has to be told)

    typealias Change = (index: UInt32, priority: FilePriority)

    /// What a click on the checkbox of `ids` does: an unchecked or mixed selection becomes checked
    /// (skipped files get Normal, others keep their priority); a fully checked one is skipped.
    func toggleChanges(_ ids: [Int]) -> [Change] {
        let files = fileNodes(under: ids)
        let allOn = !files.isEmpty && files.allSatisfy { nodes[$0].priority != .skip }
        return files.compactMap { i in
            guard let idx = nodes[i].file else { return nil }
            if allOn { return (idx, .skip) }
            return nodes[i].priority == .skip ? (idx, .normal) : nil
        }
    }

    /// The Priority menu on `ids`: wanted files below change; when none is wanted, all do (choosing a
    /// priority on a skipped folder checks it).
    func priorityChanges(_ ids: [Int], _ p: FilePriority) -> [Change] {
        let files = fileNodes(under: ids)
        let wanted = files.filter { nodes[$0].priority != .skip }
        return (wanted.isEmpty ? files : wanted).compactMap { i in
            guard let idx = nodes[i].file, nodes[i].priority != p else { return nil }
            return (idx, p)
        }
    }

    /// Applies changes locally (before the engine confirms).
    mutating func apply(_ changes: [Change]) {
        guard !changes.isEmpty else { return }
        for c in changes where Int(c.index) < nodeOfFile.count && nodeOfFile[Int(c.index)] >= 0 {
            nodes[nodeOfFile[Int(c.index)]].priority = c.priority
        }
        recompute()
    }

    /// New numbers from the engine. `keepPriorities` leaves the user's pending edits alone. Returns
    /// false when the list does not fit this tree (the caller rebuilds).
    @discardableResult
    mutating func refresh(from files: [TorrentFile], keepPriorities: Bool, at now: Date = Date()) -> Bool {
        let dt = sampled.map { now.timeIntervalSince($0) }
        var seen = 0
        for f in files {
            guard Int(f.index) < nodeOfFile.count, nodeOfFile[Int(f.index)] >= 0 else { return false }
            let i = nodeOfFile[Int(f.index)]
            guard nodes[i].length == f.length else { return false }
            let done = min(f.done, f.length)
            if let dt, dt > 0 {
                nodes[i].downRate = Self.smoothed(nodes[i].downRate, moved: done > nodes[i].done ? done - nodes[i].done : 0, dt: dt)
                nodes[i].upRate = Self.smoothed(nodes[i].upRate, moved: f.uploaded > nodes[i].uploaded ? f.uploaded - nodes[i].uploaded : 0, dt: dt)
            }
            nodes[i].done = done
            nodes[i].uploaded = f.uploaded
            if !keepPriorities { nodes[i].priority = f.priority }
            seen += 1
        }
        guard seen == fileCount else { return false }
        sampled = now
        recompute()
        return true
    }

    // MARK: Visible rows

    struct Visible: Equatable, Sendable, Identifiable { let id: Int; let depth: Int }

    /// The rows to draw, in order. A filter shows the files whose path contains the text, with the
    /// folders above them (open); without one, only the folders in `expanded` are open.
    func visible(expanded: Set<Int>, filter: String) -> [Visible] {
        var out: [Visible] = []
        let needle = filter.trimmingCharacters(in: .whitespaces)
        if needle.isEmpty {
            var stack: [Visible] = top.reversed().map { Visible(id: $0, depth: 0) }
            while let v = stack.popLast() {
                out.append(v)
                if nodes[v.id].isFolder, expanded.contains(v.id) {
                    for c in nodes[v.id].children.reversed() { stack.append(Visible(id: c, depth: v.depth + 1)) }
                }
            }
            return out
        }
        var hit = Array(repeating: false, count: nodes.count)
        for i in nodes.indices where !nodes[i].isFolder && nodes[i].path.localizedCaseInsensitiveContains(needle) {
            var p = i
            while p > 0, !hit[p] { hit[p] = true; p = nodes[p].parent }
        }
        var stack: [Visible] = top.reversed().filter { hit[$0] }.map { Visible(id: $0, depth: 0) }
        while let v = stack.popLast() {
            out.append(v)
            for c in nodes[v.id].children.reversed() where hit[c] { stack.append(Visible(id: c, depth: v.depth + 1)) }
        }
        return out
    }

    /// Folders that start open: a single top folder (Season 1/...).
    var initiallyExpanded: Set<Int> {
        let t = top
        if t.count == 1, nodes[t[0]].isFolder { return [t[0]] }
        return []
    }

    /// The file as the engine listed it (for Play).
    func torrentFile(_ id: Int) -> TorrentFile? {
        let n = nodes[id]
        guard let idx = n.file else { return nil }
        return TorrentFile(index: idx, path: n.path, diskPath: n.disk, name: n.name, length: n.length, done: n.done, wanted: n.priority != .skip,
                           priority: n.priority, isVideo: n.isVideo, isMp4Family: n.isVideo && ["mp4", "m4v", "mov"].contains(n.ext))
    }

    var folderIDs: [Int] { nodes.indices.filter { $0 > 0 && nodes[$0].isFolder } }
}
