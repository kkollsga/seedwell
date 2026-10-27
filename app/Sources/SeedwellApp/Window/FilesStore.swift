import Foundation
import Observation

/// The open file lists of the main window (the tree under a torrent row). Which torrents are expanded
/// is remembered while the app runs; the files themselves are fetched when a row is expanded, kept up
/// to date once a second while the window is open, and dropped when it closes.
@MainActor @Observable final class FilesStore {
    static let shared = FilesStore()

    /// Torrent ids with their file list open.
    private(set) var expanded: Set<String> = []
    private(set) var states: [String: FileTreeState] = [:]
    /// The selected rows of one open list (Space toggles them, ← → fold folders).
    private(set) var selectedTorrent: String?
    private(set) var selectedNodes: Set<UInt32> = []
    @ObservationIgnored private var anchor: UInt32?
    @ObservationIgnored var core: Core?
    /// Told after an edit reached the engine, so the list's rows pick up the new wanted sizes.
    @ObservationIgnored var onEdited: () -> Void = {}
    /// Told before files of a torrent are ticked (a finished torrent may become a download again, SPEC 8.3).
    @ObservationIgnored var onTicked: (String) -> Void = { _ in }

    func isExpanded(_ id: String) -> Bool { expanded.contains(id) }

    func toggle(_ id: String) { isExpanded(id) ? collapse(id) : expand(id) }

    func expand(_ id: String) {
        expanded.insert(id)
        guard states[id] == nil else { return }
        let st = FileTreeState(id: id, store: self)
        states[id] = st
        st.load()
    }

    func collapse(_ id: String) {
        expanded.remove(id)
        states[id] = nil
        if selectedTorrent == id { clearSelection() }
    }

    /// A tab switch: every torrent opens closed again (the group folds are a separate, remembered setting).
    func collapseAll() {
        for id in expanded { states[id] = nil }
        expanded = []
        clearSelection()
    }

    /// The menu bar's own store: lists it fetched itself (`FileTreeState.loadNow`) and frees when its submenu closes.
    func adopt(_ st: FileTreeState) { states[st.id] = st }
    func drop(_ id: String) { states[id] = nil }

    /// The window opened again: lists that were open reload.
    func resume() {
        for id in expanded where states[id] == nil {
            let st = FileTreeState(id: id, store: self)
            states[id] = st
            st.load()
        }
    }

    /// The window closed: nothing is fetched until it opens again.
    func suspend() {
        states = [:]
        clearSelection()
    }

    /// Once a second from the window's own timer; only open lists are asked.
    func tick() {
        for st in states.values { st.refresh() }
    }

    // MARK: Selection

    func clearSelection() { selectedTorrent = nil; selectedNodes = []; anchor = nil }

    func select(torrent: String, node: UInt32, command: Bool, shift: Bool) {
        guard let st = states[torrent] else { return }
        if selectedTorrent != torrent { selectedNodes = []; anchor = nil }
        selectedTorrent = torrent
        if shift, let a = anchor, let ia = st.rows.firstIndex(where: { $0.id == a }), let ib = st.rows.firstIndex(where: { $0.id == node }) {
            selectedNodes = Set(st.rows[min(ia, ib)...max(ia, ib)].map(\.id))
        } else if command {
            if selectedNodes.contains(node) { selectedNodes.remove(node) } else { selectedNodes.insert(node) }
            anchor = node
        } else {
            selectedNodes = [node]
            anchor = node
        }
    }

    func isSelected(torrent: String, node: UInt32) -> Bool { selectedTorrent == torrent && selectedNodes.contains(node) }

    /// The rows an action on `node` covers: the whole selection when `node` is in it, else just `node`.
    func targets(torrent: String, node: UInt32) -> [UInt32] {
        isSelected(torrent: torrent, node: node) ? Array(selectedNodes) : [node]
    }

    /// Space: check what is unchecked, else skip.
    func toggleSelected() {
        guard let id = selectedTorrent, let st = states[id], !selectedNodes.isEmpty else { return }
        st.toggle(Array(selectedNodes))
    }

    /// → on the selected row: opens a folder.
    func openSelected() -> Bool {
        guard let id = selectedTorrent, let st = states[id], let node = selectedNodes.first, selectedNodes.count == 1 else { return false }
        st.setFolder(node, open: true)
        return true
    }

    /// ← on the selected row: closes a folder, else selects the folder it is in (or the torrent row).
    /// Returns the torrent id to select when the selection moves to its row.
    func closeSelected() -> (handled: Bool, selectTorrent: String?) {
        guard let id = selectedTorrent, let st = states[id], let node = selectedNodes.first, selectedNodes.count == 1,
              let row = st.rows.first(where: { $0.id == node }) else { return (false, nil) }
        if row.isFolder, st.expandedFolders.contains(node) { st.setFolder(node, open: false); return (true, nil) }
        let parent = row.parent
        if parent > 0 { selectedNodes = [parent]; anchor = parent; return (true, nil) }
        clearSelection()
        return (true, id)
    }
}

/// One torrent's open list: the tree (the core's `FileTreeModel`), which folders are open, the filter, the rows the
/// core lays out from them, and the edits on their way to the engine.
@MainActor @Observable final class FileTreeState {
    let id: String
    private(set) var tree: FileTreeModel?
    private(set) var expandedFolders: Set<UInt32> = []
    var filter = "" { didSet { if filter != oldValue { reflow() } } }
    /// The rows to draw, in order, with everything each shows.
    private(set) var rows: [FileRow] = []
    private(set) var totals: FileTotals?
    private(set) var failed = false
    @ObservationIgnored private unowned let store: FilesStore
    @ObservationIgnored private var refreshing = false
    @ObservationIgnored private var lastRefresh = Date.distantPast
    @ObservationIgnored private var pending: [UInt32: FilePriority] = [:]
    @ObservationIgnored private var inflight = 0
    @ObservationIgnored private var flushTask: Task<Void, Never>?
    /// The menu bar's file submenu redraws its rows when the tree changed (edit, failed send, reload).
    @ObservationIgnored var onChange: (() -> Void)?

    init(id: String, store: FilesStore) {
        self.id = id
        self.store = store
    }

    /// Fetches the list and builds the tree (off the main thread: 5,000 files take a moment to sort).
    func load() {
        guard let core = store.core else { return }
        let id = id
        Task { [weak self] in
            let built: FileTreeModel? = await Task.detached { (try? core.files(id: id)).map { FileTreeModel(files: $0) } }.value
            guard let self else { return }
            guard let built else { failed = true; return }
            failed = false
            tree = built
            expandedFolders = Set(built.initiallyExpanded())
            reflow()
            onChange?()
        }
    }

    /// The menu bar's version of `load`: the list is in place when this returns (the submenu is about to show).
    @discardableResult func loadNow() -> Bool {
        guard let core = store.core, let files = try? core.files(id: id) else { failed = true; return false }
        failed = false
        tree = FileTreeModel(files: files)
        reflow()
        return true
    }

    var hasPending: Bool { !pending.isEmpty || inflight > 0 }

    /// Progress (and priorities someone else changed) once a second.
    func refresh() {
        guard let core = store.core, tree != nil, !refreshing, !hasPending, Date().timeIntervalSince(lastRefresh) >= 0.9 else { return }
        refreshing = true
        lastRefresh = Date()
        let id = id
        Task { [weak self] in
            let files: [TorrentFile]? = await Task.detached { try? core.files(id: id) }.value
            guard let self else { return }
            refreshing = false
            guard let files, let t = tree, !hasPending else { return }
            switch t.refresh(files: files, keepPriorities: false, nowMs: UInt64(Date().timeIntervalSince1970 * 1000)) {
            case .same: break
            case .changed: reflow()
            case .rebuild:
                // The list changed shape: build it again.
                tree = FileTreeModel(files: files)
                reflow()
            }
        }
    }

    // MARK: Folders

    func setFolder(_ node: UInt32, open: Bool) {
        guard tree != nil else { return }
        if open { expandedFolders.insert(node) } else { expandedFolders.remove(node) }
        reflow()
    }

    func toggleFolder(_ node: UInt32) { setFolder(node, open: !expandedFolders.contains(node)) }

    private func reflow() {
        guard let t = tree else { rows = []; totals = nil; return }
        rows = t.rows(expanded: Array(expandedFolders), filter: filter)
        totals = t.totals()
    }

    // MARK: Edits

    /// The checkbox of these rows.
    func toggle(_ nodes: [UInt32]) {
        guard let t = tree else { return }
        edit(t.toggleChanges(nodes: nodes))
    }

    func setPriority(_ nodes: [UInt32], _ p: FilePriority) {
        guard let t = tree else { return }
        edit(t.priorityChanges(nodes: nodes, priority: p))
    }

    /// Select All / Select None on what the filter shows (everything without one).
    func selectAll(_ on: Bool) {
        guard let t = tree else { return }
        edit(t.wantChanges(nodes: filter.isEmpty ? t.top() : rows.map(\.id), on: on))
    }

    /// Check (`on`) or skip every file under these rows (the menu bar's Select All / None for a folder).
    func setWanted(_ nodes: [UInt32], _ on: Bool) {
        guard let t = tree else { return }
        edit(t.wantChanges(nodes: nodes, on: on))
    }

    /// Applied at once on screen; sent to the engine as one call after a quiet moment, so a run of
    /// clicks is one change.
    private func edit(_ changes: [FilePriorityChange]) {
        guard !changes.isEmpty, let t = tree else { return }
        t.apply(changes: changes)
        reflow()
        for c in changes { pending[c.index] = c.priority }
        onChange?()
        flushTask?.cancel()
        flushTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(250))
            guard !Task.isCancelled else { return }
            self?.flush()
        }
    }

    /// Sends the edits now. The menu bar drives its own debounce with a timer that runs while the menu
    /// tracks (a menu's run loop mode does not serve the tasks `edit` sleeps in).
    func flushNow() { flushTask?.cancel(); flush() }

    private func flush() {
        guard let core = store.core, !pending.isEmpty else { return }
        let sending = pending.map { FilePriorityChange(index: $0.key, priority: $0.value) }
        pending = [:]
        inflight += 1
        let id = id
        if sending.contains(where: { $0.priority != .skip }) { store.onTicked(id) }
        // The engine call starts at once, off the main actor (a menu's run loop mode does not serve main-actor tasks).
        Task.detached { [weak self] in
            let ok = (try? core.setFilePriorities(id: id, changes: sending)) != nil
            await self?.sent(ok)
        }
    }

    private func sent(_ ok: Bool) {
        inflight -= 1
        store.onEdited()
        if !ok {
            // The engine refused: show what it really has.
            pending = [:]
            load()
        } else if !hasPending {
            refresh()
        }
    }
}
