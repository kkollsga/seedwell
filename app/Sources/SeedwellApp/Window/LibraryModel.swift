import Foundation
import Observation

/// View model of one main window. It polls the core once a second, only between `start()` and
/// `stop()`, and is dropped (with its timer) when the window closes (SPEC §2, §8.3).
///
/// Sidebar: Downloads, Sharing and each tracker are places (sections). A tracker tab shows every
/// torrent of that tracker, downloads and seeds, as collapsible groups (SPEC §8.3).
@MainActor @Observable final class LibraryModel {
    enum Section: Hashable { case downloads, sharing, folderSync, settings, tracker(String) }

    let core: Core
    var section: Section
    /// The row a ⇧-click range starts from: the last plainly or ⌘-clicked row.
    var selectionAnchor: String?
    var snapshot: Snapshot
    var selection: Set<String> = []
    var search = "" { didSet { if search != oldValue { refresh() } } }
    var infoID: String?
    /// Only torrents of this category are listed (the pill on a row sets it, the chip above the list clears it).
    /// Combines with the search text and the tracker; a tab switch clears it.
    var categoryFilter: Category? { didSet { if categoryFilter != oldValue { refresh() } } }
    /// The tracker's Info page is shown instead of its torrent list (the sidebar item stays selected).
    var trackerInfo = false
    /// Group headers the user folded ("sharing.dormant", "downloads.recent"). Remembered per Mac, a
    /// convenience only: lost defaults just mean everything opens unfolded.
    var collapsed: Set<String> = Set(UserDefaults.standard.stringArray(forKey: "collapsedGroups") ?? []) {
        didSet { UserDefaults.standard.set(Array(collapsed).sorted(), forKey: "collapsedGroups") }
    }
    /// The "Add a Download" sheet (File › Add Magnet Link or URL…).
    var adding = false
    /// A new download arrived while the user was busy (Settings, an inline edit): Downloads in the sidebar
    /// carries a quiet dot until the user goes there.
    var downloadsFlag = false
    /// Asks the list to scroll a row into view (a new download); the number makes every request distinct.
    struct ScrollRequest: Equatable { let id: String; let n: Int }
    private(set) var scrollRequest: ScrollRequest?
    @ObservationIgnored private var scrollCount = 0
    /// True while the user is in the middle of something the window must not be taken from (an inline
    /// edit, a remove question). Set by the window controller; Settings counts on its own.
    @ObservationIgnored var isBusy: () -> Bool = { false }
    /// Every snapshot the model polls, for the app's new-download watcher.
    @ObservationIgnored var onSnapshot: (Snapshot) -> Void = { _ in }
    /// Folder sync (SPEC 7G.5): every folder and device, read with each refresh (cheap).
    var sync: SyncSnapshot
    /// The folder whose page is shown on Folder Sync.
    var syncSelection: String? { didSet { if syncSelection != oldValue { syncAskRemove = nil; loadConflicts() } } }
    /// The conflict copies of the selected folder (read when it is selected, then every 10 s, off the main
    /// thread: the core reads the folder's whole index).
    private(set) var syncConflicts: [SyncConflict] = []
    /// The folder whose "Stop syncing" question is open under its page header, and its Trash choice.
    var syncAskRemove: String?
    var syncTrash = false
    /// Sharing rows per group, newest page only.
    private(set) var groups: [SeedGroup: [TransferRow]] = [:]
    private(set) var counts = SharingCounts(popular: 0, active: 0, dormant: 0, error: 0, dead: 0, stopped: 0)
    private(set) var trackerStats: TrackerStats?
    private(set) var daily: [DailyStat] = []
    /// How many rows of a group are fetched; "Show more" raises it by a page.
    private var shown: [SeedGroup: Int] = [:]
    static let page = 150
    @ObservationIgnored private var timer: Timer?
    @ObservationIgnored private var statsFor: String?
    @ObservationIgnored private var ticks = 0

    init(core: Core, section: Section = .downloads) {
        self.core = core
        self.section = section
        snapshot = core.snapshot()
        sync = core.syncSnapshot()
        FilesStore.shared.core = core
        FilesStore.shared.onEdited = { [weak self] in self?.refresh() }
        refresh()
    }

    func start() {
        guard timer == nil else { return }
        FilesStore.shared.resume()
        refresh()
        let t = Timer(timeInterval: 1, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.refresh() }
        }
        t.tolerance = 0.2
        RunLoop.main.add(t, forMode: .common)
        timer = t
    }

    func stop() {
        FilesStore.shared.suspend()
        timer?.invalidate()
        timer = nil
    }

    func refresh() {
        snapshot = core.snapshot()
        sync = core.syncSnapshot()
        onSnapshot(snapshot)
        ticks += 1
        if section == .folderSync {
            if syncSelection.map({ id in !sync.folders.contains { $0.id == id } }) ?? true { syncSelection = sync.folders.first?.id }
            else if ticks % 10 == 0 { loadConflicts() }
        }
        FilesStore.shared.tick()
        if let id = trackerID, !snapshot.trackers.contains(where: { $0.id == id }) {
            select(.sharing)  // the profile was deleted while its tab was open
            return
        }
        guard section == .sharing || trackerID != nil else { return }
        counts = core.sharingCounts(trackerId: trackerID, category: categoryFilter, text: search)
        var next: [SeedGroup: [TransferRow]] = [:]
        for g in SeedGroup.allCases where counts.count(g) > 0 {
            let limit = UInt32(shown[g] ?? Self.page)
            next[g] = core.sharingRows(query: RowQuery(group: g, trackerId: trackerID, text: search, category: categoryFilter, offset: 0, limit: limit))
        }
        groups = next
        if let id = trackerID {
            // Lifetime totals change slowly and reading them forces a database flush: every 5 s.
            if statsFor != id || ticks % 5 == 0 {
                trackerStats = core.trackerStats(trackerId: id)
                daily = core.trackerDaily(trackerId: id, days: 30)  // the header's last 7 days move with today
                statsFor = id
            }
        } else {
            statsFor = nil
            trackerStats = nil
        }
    }

    /// The selected folder of the Folder Sync page.
    var syncFolder: SyncFolder? { syncSelection.flatMap { id in sync.folders.first { $0.id == id } } }

    func loadConflicts() {
        guard let id = syncSelection else { syncConflicts = []; return }
        let core = self.core
        Task {
            let list = await Task.detached(priority: .utility) { core.syncConflicts(id: id) }.value
            if syncSelection == id { syncConflicts = list }
        }
    }

    /// Opens the Folder Sync page on a folder, optionally with its remove question open.
    func showSyncFolder(_ id: String?, askRemove: Bool) {
        if section != .folderSync { select(.folderSync) }
        if let id { syncSelection = id }
        syncTrash = false
        syncAskRemove = askRemove ? id : nil
    }

    func showMore(_ g: SeedGroup) {
        shown[g] = (shown[g] ?? Self.page) + Self.page
        refresh()
    }

    func remaining(_ g: SeedGroup) -> Int { max(0, counts.count(g) - (groups[g]?.count ?? 0)) }

    /// A torrent became a download (SPEC 8.3): the window goes to Downloads, selects its row and scrolls to
    /// it, with Get Info and the sheets closed. Not while the user is in Settings or in the middle of an
    /// edit: then only Downloads in the sidebar is flagged. Returns whether the window switched.
    @discardableResult func showNewDownload(_ id: String) -> Bool {
        if section == .settings || isBusy() {
            downloadsFlag = true
            return false
        }
        // The user is ticking files in this torrent's open list and the torrent stays in this tab (Downloads, or
        // its own tracker's): the window must not jump, scroll to the row or clear the file selection under them.
        if Self.staysInPlace(id, section: section, trackerInfo: trackerInfo, open: FilesStore.shared.isExpanded(id), snapshot: snapshot) { return false }
        if !search.isEmpty { search = "" }
        adding = false
        if section != .downloads || trackerInfo { select(.downloads) } else { FilesStore.shared.clearSelection(); infoID = nil }
        selection = [id]
        selectionAnchor = id
        scrollCount += 1
        scrollRequest = ScrollRequest(id: id, n: scrollCount)
        return true
    }

    /// Whether a download that arrives because its files were ticked in the window's own open list needs no
    /// takeover: it will be listed in the tab already shown.
    static func staysInPlace(_ id: String, section: Section, trackerInfo: Bool, open: Bool, snapshot s: Snapshot) -> Bool {
        guard open, !trackerInfo else { return false }
        switch section {
        case .downloads: return true
        case .tracker(let t): return (s.allDownloading + s.allQueued).contains { $0.id == id && $0.trackerId == t }
        default: return false
        }
    }

    func scrollDone(_ r: ScrollRequest) { if scrollRequest == r { scrollRequest = nil } }

    func select(_ s: Section) {
        if s == .downloads { downloadsFlag = false }
        if s != section {
            statsFor = nil; trackerStats = nil; groups = [:]
            FilesStore.shared.collapseAll()  // a tab opens with every torrent closed (SPEC 8.3)
        }
        section = s
        trackerInfo = false
        selection = []
        FilesStore.shared.clearSelection()  // a file picked in the list that just left is not selected any more
        selectionAnchor = nil
        infoID = nil
        shown = [:]
        categoryFilter = nil  // like the open file lists, a filter does not follow to another tab
        refresh()
    }

    /// The section Get Info for `id` belongs in. Only a torrent that is still downloading or queued is
    /// Downloads' own; one that just finished is listed in both sections ("Recently Finished" and Sharing),
    /// so the user stays where they are. (This used to ask "is it in downloading + queued + recent?",
    /// which sent every recently finished seed to Downloads.)
    func section(forInfo id: String) -> Section {
        let s = snapshot
        if case .tracker(let t) = section,
           (s.allDownloading + s.allQueued + s.allRecent + visibleRows).contains(where: { $0.id == id && $0.trackerId == t }) { return section }
        if (s.downloading + s.queued).contains(where: { $0.id == id }) { return .downloads }
        if s.recent.contains(where: { $0.id == id }) { if case .sharing = section { return .sharing }
            return .downloads
        }
        return .sharing
    }

    /// Selects the row and opens its Get Info in the section it belongs to (usually the one already shown).
    func reveal(id: String) {
        let target = section(forInfo: id)
        if target != section { select(target) }
        selection = [id]
        selectionAnchor = id
    }

    var tracker: TrackerSummary? {
        trackerID.flatMap { id in snapshot.trackers.first { $0.id == id } }
    }

    /// The tracker whose tab is shown, if any.
    var trackerID: String? {
        if case .tracker(let id) = section { return id }
        return nil
    }

    var title: String {
        switch section {
        case .tracker: tracker?.name ?? "Tracker"
        case .downloads: "Downloads"
        case .sharing: "Sharing"
        case .folderSync: "Folder Sync"
        case .settings: "Settings"
        }
    }

    /// The calm line under the page title: "Favorite tracker" / "Tracker" on a tracker tab (no count: its
    /// figures are in the header), else the section's own line.
    var summary: String {
        switch section {
        case .tracker(let id):
            guard let t = tracker else { return id }
            if trackerInfo { return "Info" }
            return t.favorite ? "Favorite tracker" : "Tracker"
        case .downloads:
            let s = snapshot
            let held = s.downloading.filter(\.awaitingChoice).count
            let starting = s.downloading.filter { $0.pending != nil }.count
            var parts = ["\(s.downloading.count - held - starting) active", "\(s.queued.count) up next"]
            if held > 0 { parts.append("\(held) to choose") }
            if starting > 0 { parts.append("\(starting) starting") }
            let today = s.recent.count
            if today > 0 { parts.append("\(today) finished today") }
            parts.append(Self.rates(s))
            return parts.joined(separator: " · ")
        case .sharing:
            return "\(Fmt.count(snapshot.sharingTotal, "torrent")) · \(Self.rates(snapshot))"
        case .folderSync:
            return SyncText.summary(sync) + (sync.deviceName.isEmpty ? "" : " · This Mac: \(sync.deviceName)")
        case .settings:
            return "Settings"
        }
    }

    /// The tracker's torrents moving now, bytes/s: its downloads and the sharing rows fetched for the tab (the
    /// same rows the tab lists, so the line agrees with them).
    var trackerRates: (down: UInt64, up: UInt64) {
        guard let id = trackerID else { return (0, 0) }
        let s = snapshot
        let rows = (s.allDownloading + s.allQueued + SeedGroup.allCases.flatMap { groups[$0] ?? [] })
        return Self.sumRates(rows, tracker: id)
    }

    static func sumRates(_ rows: [TransferRow], tracker: String) -> (down: UInt64, up: UInt64) {
        var seen = Set<String>()
        var down: UInt64 = 0, up: UInt64 = 0
        for r in rows where r.trackerId == tracker && r.kind == .torrent && seen.insert(r.id).inserted {
            down += r.downRate
            up += r.upRate
        }
        return (down, up)
    }

    /// "↓ 2.1 MB/s · ↑ 340 KB/s": both rates in the toolbar line of Downloads and Sharing.
    static func rates(_ s: Snapshot) -> String { "↓ \(Fmt.rate(s.downRate)) · ↑ \(Fmt.rate(s.upRate))" }

    /// A click on a row's category pill: filter to it, or clear when it already is the filter.
    func toggleCategory(_ c: Category) { categoryFilter = categoryFilter == c ? nil : c }

    func matches(_ row: TransferRow) -> Bool {
        (categoryFilter == nil || row.category == categoryFilter)
            && (search.isEmpty || row.name.localizedCaseInsensitiveContains(search) || row.displayName.localizedCaseInsensitiveContains(search))
            && (trackerID == nil || row.trackerId == trackerID)
    }

    /// All rows currently on screen, for resolving selections to rows.
    var visibleRows: [TransferRow] {
        let s = snapshot
        switch section {
        case .downloads: return (s.downloading + s.queued + s.recent).filter(matches)
        case .sharing: return SeedGroup.allCases.flatMap { groups[$0] ?? [] }
        case .tracker:
            let seeds = SeedGroup.allCases.flatMap { groups[$0] ?? [] }
            let seeding = Set(seeds.map(\.id))
            // As the tab lists them: a finished torrent is in Sharing; only a finished direct download stays under Recently Finished.
            let top = (s.allDownloading + s.allQueued + s.allRecent.filter { $0.kind != .torrent && !seeding.contains($0.id) }).filter(matches)
            return top + seeds
        case .settings, .folderSync: return []
        }
    }

    /// "sharing.dormant", "downloads.recent"; every tracker tab shares "tracker.<group>".
    func collapseKey(_ group: String) -> String {
        if case .tracker = section { return "tracker.\(group)" }
        return "\(section).\(group)"
    }
    func isCollapsed(_ group: String) -> Bool { collapsed.contains(collapseKey(group)) }
    func toggleCollapsed(_ group: String) {
        let k = collapseKey(group)
        if collapsed.contains(k) { collapsed.remove(k) } else { collapsed.insert(k) }
    }

    func rows(for ids: Set<String>) -> [TransferRow] {
        visibleRows.filter { ids.contains($0.id) }
    }
}

extension LibraryModel {
    /// ⇧-click: the ids from the anchor's row to the clicked occurrence, in list order. `order` lists the
    /// visible rows as (group, id) pairs, so a torrent shown twice resolves to the occurrence clicked.
    /// With no (visible) anchor the clicked row alone is selected.
    nonisolated static func range(order: [(group: String, id: String)], anchor: String?, clicked: (group: String, id: String)) -> Set<String> {
        guard let b = order.firstIndex(where: { $0.group == clicked.group && $0.id == clicked.id }) else { return [clicked.id] }
        // The anchor's occurrence nearest the click (a duplicate in another group must not stretch the range).
        let anchors = order.indices.filter { order[$0].id == anchor }
        guard let a = anchors.min(by: { abs($0 - b) < abs($1 - b) }) else { return [clicked.id] }
        return Set(order[min(a, b)...max(a, b)].map(\.id))
    }

    /// A click on a row: plain selects it, ⌘ toggles it, ⇧ extends from the anchor. `order` is the visible rows.
    func click(group: String, id: String, command: Bool, shift: Bool, order: [(group: String, id: String)]) {
        if command {
            if selection.contains(id) { selection.remove(id) } else { selection.insert(id) }
            selectionAnchor = id
        } else if shift, let anchor = selectionAnchor, order.contains(where: { $0.id == anchor }) {
            selection = Self.range(order: order, anchor: anchor, clicked: (group, id))
        } else {
            selection = [id]
            selectionAnchor = id
        }
    }

    /// Selection after rows left the screen (a folded group): they are dropped, and so is an anchor among them.
    func drop(_ ids: [String]) {
        selection.subtract(ids)
        if let a = selectionAnchor, ids.contains(a) { selectionAnchor = nil }
    }
}

extension Snapshot {
    /// Every download, including those of a tracker in hidden mode (SPEC 7E.5). `downloading`, `queued`
    /// and `recent` are what the Downloads tab and the menu list; a tracker's own tab, the sleep assertion,
    /// the chooser and the notifications need all of them.
    var allDownloading: [TransferRow] { downloading + hiddenDownloading }
    var allQueued: [TransferRow] { queued + hiddenQueued }
    var allRecent: [TransferRow] { recent + hiddenRecent }
}
