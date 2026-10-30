import Foundation
import Observation
import SwiftUI

/// View model of one main window: the core's `Library` decides the sidebar, the title bar and the lists (what is
/// listed, every text, the folds and pages, where the window goes when its place disappears, how often it refreshes),
/// and the window's state machines (the selection, Get Info, the Remove… question, the keys, the new-download takeover,
/// the add sheet); this holds its latest model, asks again after `refreshMs` (only between `start()` and `stop()`) and
/// after every change, and runs what the core leaves to it: the file lists under the rows. It is dropped (with its
/// timer) when the window closes (SPEC §2, §8.3); the `Library` lives on with the app.
///
/// Still here for now: the pages' own reads (Folder Sync's folder, a conversation's messages and members, a tracker's
/// statistics).
@MainActor @Observable final class LibraryModel {
    /// A channel's page is its General (`topicId: nil`) or one of its topics; `direct` is a private conversation with a
    /// device (SPEC 7G.7).
    typealias Section = LibraryPlace

    let core: Core
    @ObservationIgnored let library: Library
    /// The window as the core decided it at the last refresh.
    private(set) var view: LibraryViewModel
    /// The snapshot the window was built from.
    private(set) var snapshot: Snapshot
    /// The texts of the add sheet, kept while it closes (the sheet animates out with them).
    private(set) var addSheet: AddLinkSheet?
    /// True while the user is in an inline edit of the app's own (a favorite tracker's rename or removal question):
    /// a new download must not take the window. Set by the window controller.
    @ObservationIgnored var editing: () -> Bool = { false }
    /// Every snapshot the model polls, for the app's new-download watcher.
    @ObservationIgnored var onSnapshot: (Snapshot) -> Void = { _ in }
    /// Folder sync (SPEC 7G.5): every folder and device, read with each refresh of its page (cheap).
    private(set) var sync: SyncSnapshot
    /// The folder whose page is shown on Folder Sync.
    var syncSelection: String? { didSet { if syncSelection != oldValue { syncAskRemove = nil; syncMembers = []; loadConflicts(); loadMembers() } } }
    /// The conflict copies of the selected folder (read when it is selected, then every 10 s, off the main
    /// thread: the core reads the folder's whole index).
    private(set) var syncConflicts: [SyncConflict] = []
    /// The selected folder's members with their roles (read every 2 s, off the main thread; the core keeps them
    /// apart from the folder's index, so this never waits for a pull).
    private(set) var syncMembers: [SyncMember] = []
    /// Channels (SPEC 7G.7), read with each refresh of a conversation or Folder Sync, and the shown one's messages (newest
    /// last) and members. Shown messages count as read.
    private(set) var chats: [SyncChannel] = []
    /// The shown channel's topics, newest activity first (its General is not one).
    private(set) var chatTopics: [String: [SyncTopic]] = [:]
    /// The devices this Mac has private conversations with (read on a private conversation's page).
    private(set) var directs: [SyncDirect] = []
    private(set) var chatMessages: [SyncMessage] = []
    private(set) var chatMembers: [SyncMember] = []
    /// The conversation `chatMessages` and `chatMembers` are of.
    private var chatLoaded: Section?
    /// The folder whose "Stop syncing" question is open under its page header, and its Trash choice.
    var syncAskRemove: String?
    var syncTrash = false
    private(set) var trackerStats: TrackerStats?
    private(set) var daily: [DailyStat] = []
    @ObservationIgnored private var timer: Timer?
    @ObservationIgnored private var polling = false
    @ObservationIgnored private var statsFor: String?
    @ObservationIgnored private var ticks = 0

    /// The group folds are remembered per Mac, a convenience only: lost defaults just mean everything opens unfolded.
    private static let foldsKey = "collapsedGroups"

    /// The app's `Library` (the window controller's), or one of its own (the snapshot renderer, tests).
    static func makeLibrary(core: Core) -> Library {
        Library(core: core, folded: UserDefaults.standard.stringArray(forKey: Self.foldsKey) ?? [], clock: MacClock())
    }

    init(core: Core, library: Library? = nil, section: Section = .downloads) {
        self.core = core
        self.library = library ?? Self.makeLibrary(core: core)
        self.library.setOpen(open: true)
        if section != .downloads { self.library.selectPlace(place: section) }
        view = self.library.model(nowUnixMs: Date().unixMs)
        snapshot = self.library.snapshot()
        sync = core.syncSnapshot()
        FilesStore.shared.core = core
        FilesStore.shared.onEdited = { [weak self] in self?.refresh() }
        refresh()
    }

    func start() {
        guard !polling else { return }
        polling = true
        library.setVisible(visible: true)
        FilesStore.shared.resume()
        refresh()
    }

    func stop() {
        FilesStore.shared.suspend()
        polling = false
        library.setVisible(visible: false)
        timer?.invalidate()
        timer = nil
    }

    /// The window closed: the library forgets what it showed (it opens again as new).
    func close() {
        stop()
        library.setOpen(open: false)
    }

    var section: Section { view.place }

    func refresh() {
        let shown = view.place
        let next = library.model(nowUnixMs: Date().unixMs)
        if next.removal != view.removal {
            // The Remove… question slides in under its row, and out.
            withAnimation(.snappy(duration: next.removal == nil ? 0.18 : 0.22)) { view = next }
        } else if next != view {
            view = next
        }
        if let sheet = view.addSheet, sheet != addSheet { addSheet = sheet }
        snapshot = library.snapshot()
        // Another place (a click, a new download, a tracker deleted, a channel left): its own state goes.
        if view.place != shown { placeChanged() }
        readPages()
        onSnapshot(snapshot)
        ticks += 1
        if view.page == .folderSync {
            if syncSelection.map({ id in !sync.folders.contains { $0.id == id } }) ?? true { syncSelection = sync.folders.first?.id }
            else {
                if ticks % 10 == 0 { loadConflicts() }
                if ticks % 2 == 0 { loadMembers() }
            }
        }
        if chatID != nil || directID != nil { loadChatMessages() }
        FilesStore.shared.tick()
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
        schedule()
    }

    /// The next refresh, as the model asks (none while the window cannot be seen).
    private func schedule() {
        timer?.invalidate()
        timer = nil
        guard polling, let ms = view.refreshMs else { return }
        let t = Timer(timeInterval: Double(ms) / 1000, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated { self?.refresh() }
        }
        t.tolerance = Double(view.refreshLeewayMs) / 1000
        RunLoop.main.add(t, forMode: .common)
        timer = t
    }

    /// What the page shown reads for itself: Folder Sync its folders (and the channels for Open Chat), a conversation
    /// its channel and topics, a private conversation the devices.
    private func readPages() {
        switch view.page {
        case .folderSync:
            sync = core.syncSnapshot()
            let channels = core.chatChannels()
            if channels != chats { chats = channels }
        case .conversation, .missing:
            let channels = core.chatChannels()
            if channels != chats { chats = channels }
            let topics = chatID.map { [$0: core.chatTopics(channelId: $0)] } ?? [:]
            if topics != chatTopics { chatTopics = topics }
            if directID != nil {
                let ds = core.chatDirects()
                if ds != directs { directs = ds }
            }
        default:
            break
        }
    }

    /// The selected folder of the Folder Sync page.
    var syncFolder: SyncFolder? { syncSelection.flatMap { id in sync.folders.first { $0.id == id } } }

    /// The channel whose page (General or a topic) is shown, and the topic.
    var chatID: String? {
        if case .chat(let id, _) = section { return id }
        return nil
    }

    var chatTopicID: String? {
        if case .chat(_, let t) = section { return t }
        return nil
    }

    var chat: SyncChannel? { chatID.flatMap { id in chats.first { $0.id == id } } }

    var chatTopic: SyncTopic? { chatTopicID.flatMap { t in chatID.flatMap { chatTopics[$0] }?.first { $0.id == t } } }

    /// The device whose private conversation is shown.
    var directID: String? {
        if case .direct(let id) = section { return id }
        return nil
    }

    var direct: SyncDirect? { directID.flatMap { id in directs.first { $0.deviceId == id } } }

    /// The name of the device of the shown private conversation (the page's title).
    var directName: String { directID == nil ? "" : view.title }

    /// Opens or closes a channel in the sidebar (its topics under it).
    func setChannelOpen(_ place: Section, _ open: Bool) {
        guard case .chat(let id, _) = place else { return }
        library.setChannelOpen(channelId: id, open: open)
        refresh()
    }

    /// A private conversation was just started with this device (no message yet): its name until the core lists it.
    func noteDirectName(_ device: String, _ name: String) { library.noteDirectName(deviceId: device, name: name) }

    /// Reads the shown conversation's latest messages, and marks them read (the page is on screen when this runs).
    func loadChatMessages() {
        let place = section
        if chatLoaded != place { chatLoaded = place; chatMessages = []; chatMembers = [] }
        let list: [SyncMessage]
        switch place {
        case .chat(let id, let topic):
            guard chat != nil else { return }
            list = core.chatMessages(id: id, topic: topic, before: nil, limit: 300)
            let unread = topic == nil ? chat.map { ChatText.generalUnread($0, chatTopics[id] ?? []) } ?? 0 : (chatTopic?.unread ?? 0)
            if unread > 0 { core.chatMarkRead(id: id, topic: topic) }
        case .direct(let id):
            list = core.chatDirectMessages(deviceId: id, before: nil, limit: 300)
            if direct?.unread ?? 0 > 0 { core.chatMarkDirectRead(deviceId: id) }
        default:
            list = []
        }
        if list != chatMessages { chatMessages = list }
    }

    func loadChatMembers() {
        guard let id = chatID else { chatMembers = []; return }
        let core = self.core
        Task {
            let list = await Task.detached(priority: .utility) { core.syncMembers(id: id) }.value
            if chatID == id { chatMembers = list }
        }
    }

    func loadMembers() {
        guard let id = syncSelection else { syncMembers = []; return }
        let core = self.core
        Task {
            let list = await Task.detached(priority: .utility) { core.syncMembers(id: id) }.value
            if syncSelection == id, list != syncMembers { syncMembers = list }
        }
    }

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

    /// "Show more" under a Sharing group: another page of its rows.
    func showMore(_ g: SeedGroup) {
        library.showMore(group: g)
        refresh()
    }

    /// A new download arrived while the user was in Settings or editing: Downloads in the sidebar carries a quiet dot.
    var downloadsFlag: Bool { view.sidebar.places.first?.flag != nil }

    /// What the app knows that the core does not: its own inline edits and the file lists under the rows.
    var context: WindowContext {
        let files = FilesStore.shared
        return WindowContext(editing: editing(), openLists: Array(files.expanded), fileTorrent: files.selectedTorrent,
                             fileRows: UInt32(files.selectedNodes.count))
    }

    /// A torrent became a download (SPEC 8.3): the core decides whether the window goes to it or only flags Downloads.
    @discardableResult func newDownload(_ id: String) -> NewDownloadOutcome {
        let outcome = library.newDownload(rowId: id, context: context)
        if outcome == .shown { FilesStore.shared.clearSelection() }
        refresh()
        return outcome
    }

    /// A click on the torrent's title in the notifier: as a new download, but Settings gives way (an open add sheet
    /// keeps the window and its text).
    @discardableResult func showTorrent(_ id: String) -> NewDownloadOutcome {
        let outcome = library.showTorrent(rowId: id, context: context)
        if outcome == .shown { FilesStore.shared.clearSelection() }
        refresh()
        return outcome
    }

    /// The window just opened (`plain`: with no place, row or Settings asked for): on the download that arrived meanwhile.
    func takeArrival(plain: Bool) {
        if library.takeArrival(plain: plain, context: context) == .shown { FilesStore.shared.clearSelection() }
        refresh()
    }

    func scrollDone(_ request: UInt32) { library.scrollDone(request: request) }

    func select(_ s: Section) {
        library.selectPlace(place: s)
        FilesStore.shared.clearSelection()  // a file picked in the list that just left is not selected any more
        refresh()
    }

    /// Another place is shown: every torrent closes (a tab opens with every torrent closed, SPEC 8.3) and the tracker's
    /// statistics go.
    private func placeChanged() {
        statsFor = nil; trackerStats = nil
        FilesStore.shared.collapseAll()
    }

    // MARK: Selection, Get Info, Remove… and keys (the core's rules)

    /// The selected rows on screen, in list order.
    var selection: [String] { view.selection }

    /// A click on a row (`group`: the group it is listed in); a file row picked before is deselected.
    func click(group: String, id: String, command: Bool, shift: Bool) {
        FilesStore.shared.clearSelection()
        library.clickRow(groupKey: group, rowId: id, command: command, shift: shift)
        refresh()
    }

    func setSelection(_ ids: [String]) {
        library.setSelection(rowIds: ids)
        refresh()
    }

    /// The transfers a context menu on this row acts on.
    func menuRows(_ id: String) -> [TransferRow] { library.menuRows(rowId: id) }

    /// The row whose Get Info is open.
    var infoRow: String? { view.list?.groups.lazy.flatMap(\.rows).first { $0.infoOpen }?.id }

    func openInfo(_ id: String?) {
        library.setInfo(rowId: id)
        refresh()
    }

    /// The popover of this row was dismissed.
    func infoClosed(_ id: String) {
        library.infoClosed(rowId: id)
        refresh()
    }

    func askRemoval(_ rows: [TransferRow], deleteFiles: Bool) {
        library.askRemoval(rows: rows, deleteFiles: deleteFiles)
        refresh()
    }

    func cancelRemoval() {
        library.cancelRemoval()
        refresh()
    }

    func confirmRemoval() {
        library.confirmRemoval()
        refresh()
    }

    /// A key press as the core decides it; the parts about the file lists are run here. Returns whether it was handled.
    func key(_ key: LibraryKey, _ modifiers: KeyModifiers = KeyModifiers(shift: false, command: false, option: false, control: false),
             scope: KeyScope) -> Bool {
        let outcome = library.key(key: key, modifiers: modifiers, scope: scope, context: context)
        let files = FilesStore.shared
        switch outcome.action {
        case .toggleFileRows: files.toggleSelected()
        case .openFileFolder: _ = files.openSelected()
        case .closeFileFolder: if let id = files.closeSelected().selectTorrent { library.setSelection(rowIds: [id]) }
        case let .openFiles(id): files.expand(id)
        case let .closeFiles(id): files.collapse(id)
        case .clearFileSelection: files.clearSelection()
        case nil: break
        }
        refresh()
        return outcome.handled
    }

    /// The search text of the lists.
    func setSearch(_ text: String) {
        library.setSearch(text: text)
        refresh()
    }

    func showAddSheet() {
        library.showAddSheet()
        refresh()
    }

    func closeAddSheet() {
        library.closeAddSheet()
        refresh()
    }

    /// The sheet's field changed.
    func setAddSheetText(_ text: String) {
        library.setAddSheetText(text: text)
        refresh()
    }

    /// The tracker's Info page is shown instead of its torrent list (the sidebar item stays selected).
    var trackerInfo: Bool {
        get { view.page == .trackerInfo }
        set { library.setTrackerInfo(on: newValue); refresh() }
    }

    /// Selects the row in the place its Get Info belongs to (usually the one already shown).
    func reveal(id: String) {
        library.revealRow(rowId: id)
        FilesStore.shared.clearSelection()
        refresh()
    }

    var tracker: TrackerSummary? {
        trackerID.flatMap { id in snapshot.trackers.first { $0.id == id } }
    }

    /// The tracker whose tab is shown, if any.
    var trackerID: String? {
        if case .tracker(let id) = section { return id }
        return nil
    }

    /// The list is filtered to this category (the pill on a row sets it, the chip above the list clears it).
    var categoryFilter: Category? { view.list?.filterChip?.category }

    /// A click on a row's category pill: filter to it, or clear when it already is the filter.
    func toggleCategory(_ c: Category) {
        library.toggleCategory(category: c)
        refresh()
    }

    func clearCategory() {
        library.clearCategory()
        refresh()
    }

    /// The rows on screen (QA).
    var visibleRows: [TransferRow] { view.list?.groups.flatMap { $0.rows.map(\.transfer) } ?? [] }

    /// Folds or unfolds a group of the list shown; the folds are remembered. Its rows leave the selection.
    func toggleCollapsed(_ group: String) {
        UserDefaults.standard.set(library.toggleFold(key: group), forKey: Self.foldsKey)
        refresh()
    }
}
