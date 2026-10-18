import Foundation
import Observation

/// What the choice store needs from the core: the chooser data of a held torrent, its answer, its removal.
struct ChoiceEngine {
    var load: (_ id: String, _ reopen: Bool) throws -> ChoiceInfo
    var confirm: (_ id: String, _ files: [Bool]) throws -> Void
    var cancel: (_ id: String) throws -> Void
    /// The `.torrent` file a held torrent came from, checked by the core right now (a regular file with the torrent's own
    /// info-hash); nil when there is none (magnet link, web address, gone, changed).
    var sourceFile: (_ id: String) -> String? = { _ in nil }
    /// Whether `sourceFile` has something to offer (cheap, for the button's Shift variant).
    var hasSource: (_ id: String) -> Bool = { _ in false }
    /// Removes a torrent that is not waiting for a choice (an auto-start torrent in its countdown): Cancel on it.
    var remove: ((_ id: String) -> Void)?
    /// Changes the category of a torrent: the engine re-plans destination, rule, title and structure.
    var setCategory: ((_ id: String, _ category: Category) -> Void)?
}

/// The one `ChooserModel` per torrent that the notifier popover and the main-window chooser share
/// (SPEC 7A.6): both edit the same ticks, so a change in one shows in the other, and either one's
/// OK or Cancel resolves the torrent for both.
@MainActor @Observable final class ChoiceStore {
    static let shared = ChoiceStore()

    @ObservationIgnored var engine: ChoiceEngine?
    /// Told when OK or Cancel resolved a torrent here, so a chooser window open for it can close.
    @ObservationIgnored var didResolve: (String) -> Void = { _ in }
    private(set) var models: [String: ChooserModel] = [:]
    /// Moves a cancelled torrent's `.torrent` file to the Trash; injected so that tests never touch the real one.
    @ObservationIgnored var trash: SourceTrash
    /// A quiet note (a notification, never an alert).
    @ObservationIgnored var note: (String) -> Void

    init(engine: ChoiceEngine? = nil, trash: SourceTrash = .standard(),
         note: @escaping (String) -> Void = { Notifier.post(title: $0, body: "The torrent was removed.") }) {
        self.engine = engine
        self.trash = trash
        self.note = note
    }

    /// Cancel would move a `.torrent` file to the Trash (so ⇧ has something to keep).
    func hasSource(_ id: String) -> Bool { engine?.hasSource(id) ?? false }

    /// The shared model of `id`, loaded from the core the first time. `reopen` is for a running
    /// torrent (Get Info ▸ Files); a held one is asked as it is. Nil when the core cannot say (yet).
    func model(for id: String, reopen: Bool = false) -> ChooserModel? {
        if let m = models[id] { return m }
        guard let engine, let info = try? engine.load(id, reopen) else { return nil }
        let m = ChooserModel(info: info, origin: info.awaiting ? "Choose what to download" : "Choose files")
        // Start on a torrent that waits takes everything except the Extras unless the user ticks otherwise (SPEC 7A.6):
        // the list shows exactly what Start does.
        if info.awaiting { m.applyHeldDefault() }
        models[id] = m
        return m
    }

    /// OK: apply the ticks and let the torrent start through the normal path (no countdown).
    func confirm(_ id: String) throws {
        guard let engine, let m = models[id] else { return }
        try engine.confirm(id, m.fileFlags())
        models[id] = nil
        didResolve(id)
    }

    /// Cancel: remove the held torrent. Nothing was downloaded; the engine never deletes files here. The `.torrent`
    /// file it came from goes to the Trash (SPEC 7A.6) unless `keepFile` (⇧-Cancel). A file that cannot be moved
    /// never stops the removal: a quiet note says so.
    func cancel(_ id: String, keepFile: Bool = false) throws {
        guard let engine else { return }
        // Asked before the removal, while the engine still knows the file; the core re-reads it and checks its hash.
        let source = keepFile ? nil : engine.sourceFile(id)
        do { try engine.cancel(id) } catch {
            guard let remove = engine.remove else { throw error }
            remove(id)
        }
        models[id] = nil
        didResolve(id)
        if let source, case .failed(let name) = trash.trash(source) {
            note("Couldn't move \(name) to the Trash")
        }
    }

    /// The pill's menu, and the chooser's Change: the engine re-plans the torrent and the shared model is rebuilt from
    /// it (new destination, rule, structure, quick picks). A torrent that waits keeps the user's ticks where the same
    /// files are still there; one they never touched gets the default again (everything but Extras). A countdown
    /// keeps counting; nothing starts or stops.
    func changeCategory(_ id: String, to c: Category) {
        guard let engine, let set = engine.setCategory else { return }
        guard let m = models[id] else { return set(id, c) }
        guard m.category != c else { return }
        let before = m.fileFlags()
        let edited = m.edited
        set(id, c)
        guard let info = try? engine.load(id, !m.awaiting) else { return }
        let fresh = ChooserModel(info: info, origin: m.origin)
        if info.awaiting {
            if edited {
                fresh.checked = Set(fresh.leafItems.filter { leaf in
                    !leaf.files.isEmpty && leaf.files.allSatisfy { before.indices.contains($0) && before[$0] }
                }.map(\.id))
                fresh.edited = true
            } else {
                fresh.applyHeldDefault()
            }
        }
        m.adopt(fresh)
    }

    func drop(_ id: String) { models[id] = nil }

    /// Forgets the models of torrents that do not wait for a choice any more.
    func prune(keeping ids: Set<String>) {
        for id in models.keys where !ids.contains(id) { models[id] = nil }
    }
}

extension ChooserModel {
    /// Every file-level tick of the chooser, in order.
    private var allLeafIDs: [String] { items.flatMap(\.leafIDs) }

    var totalBytes: UInt64 {
        items.reduce(0) { $0 + $1.size }
    }

    /// All ticked, none ticked, or in between: what the "Select all / none" toggle shows.
    var allState: TriState {
        let leaves = allLeafIDs
        let n = leaves.filter(checked.contains).count
        return n == 0 ? .off : n == leaves.count ? .on : .mixed
    }

    func selectAll() { checked = Set(allLeafIDs); pick = nil }
    func selectNone() { checked = []; pick = nil }

    /// Everything ticked turns into nothing ticked; anything else turns into everything.
    func toggleAll() { if allState == .on { selectNone() } else { selectAll() } }

    /// The rows the compact checklist shows, in order: each group, then its children when open.
    var visibleRows: [(item: ChoiceItem, level: Int)] {
        items.flatMap { top -> [(ChoiceItem, Int)] in
            [(top, 0)] + (expanded.contains(top.id) ? top.children.map { ($0, 1) } : [])
        }
    }
}
