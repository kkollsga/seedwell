import Foundation
import Observation

/// One selectable thing in the chooser (SPEC §7A.6): a season, an album, a movie, a build for
/// one platform, or the Extras group. Groups have children (episodes, tracks); leaves carry files.
struct ChoiceItem: Identifiable, Hashable {
    let id: String
    var title: String
    /// "10 episodes", "2021 · 1080p", ...
    var detail: String = ""
    var size: UInt64
    /// The library already has this (same size, or the same episode at another quality).
    var inLibrary = false
    var isExtras = false
    /// Files that belong to no item (kept whenever something is chosen): never counted as a title.
    var isOther = false
    /// "In library" or "Other quality".
    var badge = "In library"
    var children: [ChoiceItem] = []
    /// File indices of the torrent this item stands for (all descendants for a group).
    var files: [Int] = []

    var leafIDs: [String] { children.isEmpty ? [id] : children.flatMap(\.leafIDs) }
}

enum TriState { case on, off, mixed }

/// What the chooser shows and what it will answer. Built from the engine's structure; until that
/// API exists the app builds it from `ChooserModel.sample()` for design work.
@MainActor @Observable final class ChooserModel {
    enum QuickPick: String, CaseIterable, Identifiable {
        case all = "All", missing = "What I don't have", latest = "Latest season"
        var id: String { rawValue }
    }

    var origin: String
    var name: String
    var summary: String
    var categoryTitle: String
    var categoryDetail: String
    var categorySymbol: String
    var notice: String?
    var items: [ChoiceItem]
    var destination: String
    /// "Y – seed to 1.0" or "Default": the rule the torrent gets in this category (concept 6 card).
    var ruleName = ""
    /// "seeds until ratio 1.0", from the engine.
    var thenLine = ""
    var freeSpace: UInt64
    /// "Season" / "album" / "movie": the unit the primary button counts.
    var unit: (singular: String, plural: String)
    /// "Episode" / "track": what a partly ticked item counts instead.
    var childUnit: (singular: String, plural: String) = ("File", "Files")
    var quickPicks: [QuickPick]
    var checked: Set<String> = []
    var expanded: Set<String> = []
    var pick: QuickPick?
    /// The user ticked or unticked something (or took a quick pick): a category change keeps their ticks then.
    var edited = false
    /// The torrent being decided and how many files it has (for the answer).
    var torrentID = ""
    var fileCount = 0
    var awaiting = true
    var unsure = false
    var category: Category = .other
    /// The raw release name is shown in the header; this is the clean title the card shows.
    var displayName = ""

    init(origin: String, name: String, summary: String, categoryTitle: String, categoryDetail: String, categorySymbol: String,
         notice: String? = nil, items: [ChoiceItem], destination: String, freeSpace: UInt64,
         unit: (String, String), quickPicks: [QuickPick]) {
        self.origin = origin
        self.name = name
        self.summary = summary
        self.categoryTitle = categoryTitle
        self.categoryDetail = categoryDetail
        self.categorySymbol = categorySymbol
        self.notice = notice
        self.items = items
        self.destination = destination
        self.freeSpace = freeSpace
        self.unit = (unit.0, unit.1)
        self.quickPicks = quickPicks
        apply(quickPicks.contains(.missing) && items.contains { $0.inLibrary } ? .missing : .all)
        fileCount = 0
        edited = false
    }

    // MARK: Selection

    func state(_ item: ChoiceItem) -> TriState {
        let leaves = item.leafIDs
        let n = leaves.filter(checked.contains).count
        return n == 0 ? .off : n == leaves.count ? .on : .mixed
    }

    func toggle(_ item: ChoiceItem) {
        let leaves = item.leafIDs
        if state(item) == .on { leaves.forEach { checked.remove($0) } } else { leaves.forEach { checked.insert($0) } }
        pick = nil
        edited = true
    }

    /// Folds or unfolds a group. Only the `expanded` set changes, never the ticks.
    func toggleExpanded(_ item: ChoiceItem) {
        guard !item.children.isEmpty else { return }
        if expanded.contains(item.id) { expanded.remove(item.id) } else { expanded.insert(item.id) }
    }

    func apply(_ p: QuickPick) {
        pick = p
        edited = true
        checked = []
        let regular = items.filter { !$0.isExtras }
        switch p {
        case .all: regular.forEach { checked.formUnion($0.leafIDs) }
        case .missing: regular.filter { !$0.inLibrary }.forEach { checked.formUnion($0.leafIDs) }
        case .latest: regular.last.map { checked.formUnion($0.leafIDs) }
        }
    }

    /// The start of a torrent that waits (SPEC 7A.6), as the core decides it (`choiceDefaultFiles`: everything but the
    /// Extras group): each leaf whose files it all wants is ticked. It is not an edit: a category change may replace it.
    func applyHeldDefault(_ files: [Bool]) {
        checked = Set(leafItems.filter { leaf in
            !leaf.files.isEmpty && leaf.files.allSatisfy { files.indices.contains($0) && files[$0] }
        }.map(\.id))
        pick = nil
    }

    /// Every leaf item (the things that carry files).
    var leafItems: [ChoiceItem] { items.flatMap { $0.children.isEmpty ? [$0] : $0.children } }

    // MARK: Totals

    private var allItems: [ChoiceItem] { items.flatMap { [$0] + $0.children } }

    var selectedTop: [ChoiceItem] { items.filter { state($0) != .off } }

    var selectedBytes: UInt64 {
        var total: UInt64 = 0
        for item in items {
            if item.children.isEmpty { if checked.contains(item.id) { total += item.size } }
            else { total += item.children.filter { checked.contains($0.id) }.reduce(0) { $0 + $1.size } }
        }
        return total
    }

    var selectedCount: Int { selectedTop.filter { !$0.isExtras && !$0.isOther }.count }

    /// "1 season", "3 episodes" or "1 season + 2 episodes": whole items count as the unit, the ticked
    /// children of a partly ticked item as theirs. Empty when nothing is ticked.
    var selectionPhrase: String {
        let main = selectedTop.filter { !$0.isExtras && !$0.isOther }
        let whole = main.filter { state($0) == .on || $0.children.isEmpty }.count
        let parts = main.filter { state($0) == .mixed }.reduce(0) { $0 + $1.leafIDs.filter(checked.contains).count }
        var out: [String] = []
        if whole > 0 { out.append("\(whole) \(whole == 1 ? unit.singular : unit.plural)") }
        if parts > 0 { out.append("\(parts) \(parts == 1 ? childUnit.singular : childUnit.plural)") }
        return out.joined(separator: " + ")
    }

    var totalsLine: String {
        let phrase = selectionPhrase.isEmpty ? "Nothing selected" : selectionPhrase.lowercased()
        return "\(phrase) · \(Fmt.bytes(selectedBytes)) · \(Fmt.bytes(freeSpace)) free"
    }

    /// "Start" for a torrent that waits (the same word everywhere it is held or new); "Download" when more files of a
    /// running torrent are ticked. The count is in the totals line, never in the button.
    var primaryTitle: String { awaiting ? "Start" : "Download" }

    /// One flag per torrent file: the files of every ticked item. Unticked leaves are skipped.
    func fileFlags() -> [Bool] {
        var flags = [Bool](repeating: false, count: fileCount)
        func mark(_ item: ChoiceItem) {
            if item.children.isEmpty {
                if checked.contains(item.id) { for f in item.files where f < flags.count { flags[f] = true } }
            } else {
                item.children.forEach(mark)
            }
        }
        items.forEach(mark)
        return flags
    }

    var notEnoughSpace: Bool { selectedBytes > freeSpace }
    var canConfirm: Bool { selectedBytes > 0 }
}

extension ChooserModel {
    /// Builds the chooser from what the engine found (SPEC §7A.6).
    convenience init(info: ChoiceInfo, origin: String) {
        var items: [ChoiceItem] = []
        // A torrent classified Other has no albums or seasons: its items are listed plainly.
        let generic = info.category == .other
        func leaf(_ gi: Int, _ ii: Int, _ i: ChoiceItemInfo) -> ChoiceItem {
            var detail = ""
            if let q = i.quality { detail = q }
            if i.thisMac { detail = detail.isEmpty ? "for this Mac" : detail + " · for this Mac" }
            var it = ChoiceItem(id: "g\(gi)i\(ii)", title: i.label, detail: detail, size: i.bytes,
                                inLibrary: i.inLibrary || i.otherQuality, files: i.files.map(Int.init))
            it.badge = i.otherQuality && !i.inLibrary ? "Other quality" : "In library"
            return it
        }
        var preselected = Set<String>()
        for (gi, g) in info.groups.enumerated() {
            let leaves = g.items.enumerated().map { leaf(gi, $0.offset, $0.element) }
            for (l, src) in zip(leaves, g.items) where src.selected { preselected.insert(l.id) }
            switch g.kind {
            case _ where generic && g.kind != .extras && g.kind != .other, .collection, .other:
                items += leaves.map { var l = $0; l.isOther = g.kind == .other; return l }
            default:
                var top = ChoiceItem(id: "g\(gi)", title: g.label,
                                     detail: g.kind == .extras ? "samples, featurettes, .nfo" : "\(leaves.count) \(leaves.count == 1 ? "item" : "items")",
                                     size: leaves.reduce(0) { $0 + $1.size }, isExtras: g.kind == .extras, children: leaves)
                top.inLibrary = !leaves.isEmpty && leaves.allSatisfy(\.inLibrary)
                top.files = leaves.flatMap(\.files)
                if g.kind == .season || g.kind == .specials { top.detail = "\(leaves.count) episode\(leaves.count == 1 ? "" : "s")" }
                items.append(top)
            }
        }
        let unit: (String, String) = if generic { ("Item", "Items") } else { switch info.structure {
        case "tv": ("Season", "Seasons")
        case "movies": ("Movie", "Movies")
        case "music": ("Album", "Albums")
        case "books", "audiobooks": ("Title", "Titles")
        case "software": ("Build", "Builds")
        default: ("Item", "Items")
        } }
        let cat = info.category
        let title = info.displayName.isEmpty ? info.name : info.displayName
        var picks: [QuickPick] = [.all]
        if items.contains(where: \.inLibrary) || info.libraryChecked { picks.append(.missing) }
        if info.structure == "tv" { picks.append(.latest) }
        self.init(origin: origin, name: info.name, summary: Self.summaryLine(info),
                  categoryTitle: "\(cat.label) · \(title)",
                  categoryDetail: "\(Int((info.confidence * 100).rounded()))% confident",
                  categorySymbol: cat.symbol, notice: info.reasons.isEmpty ? nil : info.reasons.joined(separator: " "),
                  items: items, destination: (info.destination ?? "").isEmpty ? "the download folder" : Notifier.abbreviate(info.destination ?? ""),
                  freeSpace: info.freeBytes ?? UInt64.max / 2, unit: unit, quickPicks: picks)
        torrentID = info.id
        childUnit = switch info.structure {
        case "tv": ("Episode", "Episodes")
        case "music": ("Track", "Tracks")
        default: ("File", "Files")
        }
        displayName = title
        ruleName = info.ruleName
        thenLine = info.thenText
        fileCount = Int(info.fileCount)
        awaiting = info.awaiting
        unsure = info.unsure
        category = cat
        checked = preselected
        pick = nil
        if picks.contains(.missing), !items.filter({ !$0.isExtras }).allSatisfy({ state($0) == .on }) { pick = .missing }
        else if items.filter({ !$0.isExtras && !$0.isOther }).allSatisfy({ state($0) == .on }) { pick = .all }
        if let first = items.first(where: { $0.children.count > 0 && state($0) != .off }) { expanded = [first.id] }
    }

    /// Replaces everything with what `other` shows (the category changed: new structure, new place).
    func adopt(_ other: ChooserModel) {
        origin = other.origin; name = other.name; summary = other.summary
        categoryTitle = other.categoryTitle; categoryDetail = other.categoryDetail; categorySymbol = other.categorySymbol
        notice = other.notice; items = other.items; destination = other.destination
        ruleName = other.ruleName; thenLine = other.thenLine; freeSpace = other.freeSpace
        unit = other.unit; childUnit = other.childUnit; quickPicks = other.quickPicks; checked = other.checked; expanded = other.expanded
        edited = other.edited; pick = other.pick; torrentID = other.torrentID; fileCount = other.fileCount; awaiting = other.awaiting
        unsure = other.unsure; category = other.category; displayName = other.displayName
    }

    private static func summaryLine(_ i: ChoiceInfo) -> String {
        var parts = [Fmt.bytes(i.totalBytes), "\(i.fileCount) file\(i.fileCount == 1 ? "" : "s")"]
        if !i.tracker.isEmpty { parts.append(i.tracker) }
        return parts.joined(separator: " · ")
    }
}

extension ChooserModel {
    /// The scene of concept 10, for design work and the snapshot renderer.
    static func sample() -> ChooserModel {
        func episodes(_ season: Int, _ n: Int, gb: Double) -> [ChoiceItem] {
            (1...n).map { e in
                ChoiceItem(id: "s\(season)e\(e)", title: String(format: "S%02dE%02d", season, e),
                           size: UInt64(gb * 1e9 / Double(n)), files: [season * 100 + e])
            }
        }
        func season(_ n: Int, _ eps: Int, gb: Double, library: Bool = false) -> ChoiceItem {
            var s = ChoiceItem(id: "s\(n)", title: "Season \(n)", detail: "\(eps) episodes", size: UInt64(gb * 1e9),
                               inLibrary: library, children: episodes(n, eps, gb: gb))
            s.files = s.children.flatMap(\.files)
            return s
        }
        let extras = ChoiceItem(id: "extras", title: "Extras", detail: "samples, featurettes, .nfo", size: 2_100_000_000,
                                isExtras: true, children: [ChoiceItem(id: "x1", title: "Featurettes", size: 2_100_000_000)])
        let m = ChooserModel(
            origin: "Opened from Finder",
            name: "Fallen.Kingdom.Complete.Series.S01-S05.1080p.WEB-DL.x264",
            summary: "212 GB · 58 files · tracker-y.net",
            categoryTitle: "TV · Fallen Kingdom — Complete Series",
            categoryDetail: "98% confident · from file names and episode numbering",
            categorySymbol: "tv",
            notice: "Seasons 1 and 2 are already in ~/Media/TV/Fallen Kingdom, so they are not selected.",
            items: [season(1, 10, gb: 41.2, library: true), season(2, 10, gb: 42.0, library: true), season(3, 10, gb: 43.1),
                    season(4, 10, gb: 42.7), season(5, 8, gb: 40.5), extras],
            destination: "~/Media/TV/Fallen Kingdom/Season NN/",
            freeSpace: 412_000_000_000, unit: ("Season", "Seasons"), quickPicks: [.all, .missing, .latest])
        m.childUnit = ("Episode", "Episodes")
        m.ruleName = "Y – seed to 1.0"
        m.thenLine = "seeds until ratio 1.0"
        m.expanded = ["s3"]
        return m
    }
}
