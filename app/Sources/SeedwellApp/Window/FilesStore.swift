import Foundation
import Observation

/// The open file lists of a place, as the core keeps them (`FileLists`: which are open, the trees, folders, filters,
/// the selection, the edits on their way and when to read again). This class only runs what the core asks for, off
/// the main thread where it reads the engine, and keeps the drawn lists for SwiftUI: the window's (`shared`) and the
/// menu bar's own (`MenuFiles`).
@MainActor @Observable final class FilesStore {
    static let shared = FilesStore(place: .window)

    @ObservationIgnored let lists: FileLists
    @ObservationIgnored var core: Core?
    /// The window's lists as drawn, by torrent.
    private(set) var views: [String: FileListView] = [:]
    /// Torrents with their list open, and the selected file rows (both as the core has them).
    private(set) var expanded: [String] = []
    private(set) var selection: FileSelection?
    /// "Show All": the window's lists grow to their full height instead of scrolling inside (one choice for every
    /// torrent, the held torrent's list included), remembered.
    var showAll = UserDefaults.standard.bool(forKey: FileLayout.showAllKey) {
        didSet {
            UserDefaults.standard.set(showAll, forKey: FileLayout.showAllKey)
            redraw()
        }
    }
    /// Told after an edit reached the engine, so the list's rows pick up the new wanted sizes.
    @ObservationIgnored var onEdited: () -> Void = {}
    /// Told before files of a torrent are ticked (a finished torrent may become a download again, SPEC 8.3).
    @ObservationIgnored var onTicked: (String) -> Void = { _ in }
    /// The menu bar's list redraws when its tree changed (an edit, the engine's answer, a reload).
    @ObservationIgnored var onChange: (String) -> Void = { _ in }
    @ObservationIgnored private var flushTimers: [String: Timer] = [:]

    init(place: FileListPlace) {
        lists = FileLists(place: place)
    }

    var selectedTorrent: String? { selection?.torrentId }
    var selectedNodes: Set<UInt32> { Set(selection?.nodes ?? []) }

    func isExpanded(_ id: String) -> Bool { expanded.contains(id) }

    func toggle(_ id: String) {
        if lists.toggle(torrent: id) { load(id) }
        redraw()
    }

    func expand(_ id: String) {
        if lists.expand(torrent: id) { load(id) }
        redraw()
    }

    func collapse(_ id: String) {
        lists.collapse(torrent: id)
        redraw()
    }

    /// A tab switch: every torrent opens closed again (the group folds are a separate, remembered setting).
    func collapseAll() {
        lists.collapseAll()
        redraw()
    }

    /// The window opened again: lists that were open are read again.
    func resume() {
        for id in lists.resume() { load(id) }
        redraw()
    }

    /// The window closed: nothing is read until it opens again.
    func suspend() {
        lists.suspend()
        redraw()
    }

    /// From the window's own timer: the lists the core says are due are read again.
    func tick() {
        for id in lists.due(nowMs: Date().unixMs) { read(id) }
    }

    // MARK: Reading

    /// Reads the files and builds the tree off the main thread (5,000 files take a moment to sort).
    func load(_ id: String) {
        guard let core else { return }
        let lists = lists
        Task { [weak self] in
            let files = try? await core.files(id: id)
            await Task.detached { lists.loaded(torrent: id, files: files, nowMs: Date().unixMs) }.value
            self?.changed(id)
        }
    }

    /// The menu bar's version of `load`: the list is in place when this returns (the menu is about to show it), as the
    /// core has it within a few milliseconds (a busy engine: as last read, or not yet).
    func loadNow(_ id: String) {
        lists.loaded(torrent: id, files: core?.filesShown(id: id), nowMs: Date().unixMs)
        changed(id)
    }

    private func read(_ id: String) {
        guard let core else { return }
        let lists = lists
        Task { [weak self] in
            let files = try? await core.files(id: id)
            let moved = await Task.detached { lists.read(torrent: id, files: files, nowMs: Date().unixMs) }.value
            if moved { self?.changed(id) }
        }
    }

    // MARK: Selection

    func clearSelection() {
        lists.clearSelection()
        redraw()
    }

    /// A click on a file or folder row.
    func select(torrent: String, node: UInt32, command: Bool, shift: Bool) {
        lists.click(torrent: torrent, node: node, command: command, shift: shift)
        redraw()
    }

    func isSelected(torrent: String, node: UInt32) -> Bool { selection?.torrentId == torrent && selection?.nodes.contains(node) == true }

    /// Space: check what is unchecked, else skip.
    func toggleSelected() {
        if let id = lists.space() { edited(id) }
    }

    /// → on the selected row: opens a folder.
    @discardableResult func openSelected() -> Bool {
        let done = lists.right()
        redraw()
        return done
    }

    /// ← on the selected row: closes a folder, else selects the folder it is in (or the torrent row).
    func closeSelected() -> FileLeft {
        let out = lists.left()
        redraw()
        return out
    }

    // MARK: Folders and edits

    func toggleFolder(_ id: String, _ node: UInt32) {
        lists.toggleFolder(torrent: id, node: node)
        changed(id)
    }

    func setFolder(_ id: String, _ node: UInt32, open: Bool) {
        lists.setFolder(torrent: id, node: node, open: open)
        changed(id)
    }

    func openAllFolders(_ id: String) {
        lists.openAllFolders(torrent: id)
        changed(id)
    }

    func setFilter(_ id: String, _ text: String) {
        lists.setFilter(torrent: id, text: text)
        changed(id)
    }

    /// The check of a row (and of the selection it is in).
    func toggleCheck(_ id: String, _ node: UInt32) {
        if lists.toggleCheck(torrent: id, node: node) { edited(id) }
    }

    func setPriority(_ id: String, _ node: UInt32, _ p: FilePriority) {
        if lists.setPriority(torrent: id, node: node, priority: p) { edited(id) }
    }

    /// Select All / Select None on what the filter shows (everything without one).
    func selectAll(_ id: String, _ on: Bool) {
        if lists.selectAll(torrent: id, on: on) { edited(id) }
    }

    /// Shown at once; sent to the engine after the quiet moment the core says, on a timer that also runs while a menu
    /// tracks (a menu's run loop mode does not serve tasks), so a run of clicks is one change.
    private func edited(_ id: String) {
        changed(id)
        flushTimers[id]?.invalidate()
        let delay = Double(views[id]?.flushMs ?? lists.menuView(torrent: id)?.flushMs ?? 250) / 1000
        let t = Timer(timeInterval: delay, repeats: false) { [weak self] _ in MainActor.assumeIsolated { self?.flush(id) } }
        RunLoop.main.add(t, forMode: .common)
        flushTimers[id] = t
    }

    /// Sends the edits now (the engine call starts at once, off the main actor).
    func flush(_ id: String) {
        flushTimers[id]?.invalidate()
        flushTimers[id] = nil
        guard let core, let edits = lists.takeEdits(torrent: id) else { return }
        if edits.ticked { onTicked(id) }
        Task { [weak self] in
            let ok = (try? await core.setFilePriorities(id: id, changes: edits.changes)) != nil
            self?.sent(id, ok)
        }
    }

    private func sent(_ id: String, _ ok: Bool) {
        let next = lists.sent(torrent: id, ok: ok)
        onEdited()
        switch next {
        case .reload: load(id)
        case .read: read(id)
        case .nothing: changed(id)
        }
    }

    // MARK: Drawing

    private func changed(_ id: String) {
        redraw()
        onChange(id)
    }

    /// Everything drawn, again from the core.
    private func redraw() {
        let open = lists.expanded()
        if open != expanded { expanded = open }
        let sel = lists.selection()
        if sel != selection { selection = sel }
        var next: [String: FileListView] = [:]
        for id in open { next[id] = lists.view(torrent: id, showAll: showAll) }
        if next != views { views = next }
    }
}
