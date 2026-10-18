import Foundation
import Observation

// Favorite trackers (SPEC 7E.1, 8.3). Every tracker is identified automatically; a favorite is a
// stored profile the user made with "Save as Favorite". All editing starts from a right-click and
// happens inline: a sidebar label or a line in the favorite's tab turns into a text field.

/// The order of the sidebar's Trackers section: favorites (alphabetical), then the automatic trackers
/// by torrent count and name, then Other private (only when it has something), then Public.
enum TrackerOrder {
    static func sorted(_ trackers: [TrackerSummary]) -> [TrackerSummary] {
        func rank(_ t: TrackerSummary) -> Int {
            if t.favorite { return 0 }
            switch t.id {
            case "public": return 3
            case "other-private": return 2
            default: return 1
            }
        }
        return trackers.sorted { a, b in
            let (ra, rb) = (rank(a), rank(b))
            if ra != rb { return ra < rb }
            if ra == 1, a.count != b.count { return a.count > b.count }
            let c = a.name.localizedCaseInsensitiveCompare(b.name)
            return c == .orderedSame ? a.id < b.id : c == .orderedAscending
        }
    }

    /// The sidebar's trackers: only those with torrents, in `sorted` order. The open tracker stays
    /// while its tab is shown, so the place the user is looking at does not vanish under them.
    static func visible(_ trackers: [TrackerSummary], selected: String?) -> [TrackerSummary] {
        sorted(trackers).filter { $0.count > 0 || $0.id == selected }
    }

    /// More automatic trackers than this and the list scrolls (it always can; this is only the note).
    static let comfortable = 8
}

/// The list of addresses of one favorite, edited a line at a time. Pure: the checking of a line is
/// the core's (`UrlCheck`), passed in.
enum FavoriteURLs {
    enum Commit: Equatable {
        /// Nothing to save (an empty new line, or the same text).
        case unchanged
        /// Red hint under the field; the field stays.
        case invalid(String)
        /// The address is already on the list.
        case duplicate
        /// The new list; `passkeyRemoved` when the line carried a passkey that was dropped.
        case saved([String], passkeyRemoved: Bool)
    }

    /// `index == nil` adds a line. The stored form (passkey and query removed) is what lands in the list.
    static func commit(_ urls: [String], index: Int?, text: String, check: (String) -> UrlCheck) -> Commit {
        let raw = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if raw.isEmpty { return index == nil ? .unchanged : .invalid("Enter a tracker address") }
        let c = check(raw)
        guard let pattern = c.pattern else { return .invalid(c.error ?? "Not a tracker address") }
        if let i = index, urls.indices.contains(i), urls[i] == pattern { return .unchanged }
        if urls.enumerated().contains(where: { $0.offset != index && $0.element == pattern }) { return .duplicate }
        var next = urls
        if let i = index, next.indices.contains(i) { next[i] = pattern } else { next.append(pattern) }
        return .saved(next, passkeyRemoved: c.passkeyRemoved)
    }

    enum Removal: Equatable {
        case removed([String])
        /// The only address: removing it removes the favorite, so the user is asked.
        case lastOne
    }

    static func removing(_ urls: [String], at i: Int) -> Removal {
        guard urls.indices.contains(i) else { return .removed(urls) }
        if urls.count <= 1 { return .lastOne }
        var next = urls
        next.remove(at: i)
        return .removed(next)
    }
}

/// What the editing needs of the core (the real `Core`, or a stand-in in tests).
protocol FavoriteStore: AnyObject {
    func profilesLoad() -> [ProfileRecord]
    func profilesSave(profiles: [ProfileRecord]) throws -> [ProfileRecord]
    func urlCheck(raw: String) -> UrlCheck
    func favoriteAdd(groupId: String) throws -> ProfileRecord
    func favoriteRuleCount(id: String) -> UInt32
}

extension Core: FavoriteStore {}

/// The one inline edit in progress (a name or an address) and the question of removing a favorite.
@MainActor @Observable final class FavoriteEditing {
    enum Target: Hashable {
        case sidebarName(String)
        /// `nil` index: a new, empty line.
        case url(String, Int?)
    }

    struct Removal: Equatable {
        let id: String
        let name: String
        let rules: Int
        /// Asked because the last address was removed.
        let lastAddress: Bool
        var message: String {
            var s = lastAddress ? "That is the only address. Remove “\(name)” from Favorites?" : "Remove “\(name)” from Favorites?"
            if rules > 0 { s += " \(rules) rule\(rules == 1 ? " uses" : "s use") it." }
            return s
        }
    }

    static let passkeyNote = "Passkey removed — only the tracker address is kept"

    private(set) var target: Target?
    var text = ""
    /// Short red hint under the field being edited.
    private(set) var hint: String?
    /// Quiet note after a save ("Passkey removed …").
    private(set) var note: String?
    private(set) var removal: Removal?
    private(set) var records: [ProfileRecord] = []
    private let store: FavoriteStore
    /// Opens the favorite's tab (wired by Commands).
    @ObservationIgnored var openTab: (String) -> Void = { _ in }
    /// Opens the tracker's Info page (wired by Commands).
    @ObservationIgnored var openInfo: (String) -> Void = { _ in }
    /// After the profiles changed (the list refreshes).
    @ObservationIgnored var changed: () -> Void = {}
    /// A favorite was removed or saved.
    @ObservationIgnored var gone: (String) -> Void = { _ in }
    @ObservationIgnored private var noteTask: Task<Void, Never>?

    init(store: FavoriteStore) {
        self.store = store
        records = store.profilesLoad()
    }

    func reload() { records = store.profilesLoad() }
    func record(_ id: String) -> ProfileRecord? { records.first { $0.id == id } }
    func urls(_ id: String) -> [String] { record(id)?.urls ?? [] }

    // MARK: Starting an edit (right-click)

    func beginRename(_ id: String) {
        reload()
        guard let r = record(id) else { return }
        clearTransient()
        text = r.name
        target = .sidebarName(id)
    }

    /// The "+" under the list on the Info page: an empty, focused line.
    func beginAddURL(_ id: String) {
        reload()
        guard record(id) != nil else { return }
        clearTransient()
        text = ""
        target = .url(id, nil)
    }

    func beginEditURL(_ id: String, index: Int) {
        reload()
        guard let u = record(id)?.urls, u.indices.contains(index) else { return }
        clearTransient()
        text = u[index]
        target = .url(id, index)
    }

    func cancel() {
        target = nil
        hint = nil
        text = ""
    }

    // MARK: Finishing it (Return, or the field losing focus)

    func commit() {
        guard let t = target else { return }
        switch t {
        case .sidebarName(let id):
            let name = text.trimmingCharacters(in: .whitespacesAndNewlines)
            guard var r = record(id), !name.isEmpty, name != r.name else { return cancel() }
            r.name = String(name.prefix(64))
            if save(replacing: r) { cancel() }
        case .url(let id, let index):
            guard var r = record(id) else { return cancel() }
            switch FavoriteURLs.commit(r.urls, index: index, text: text, check: { store.urlCheck(raw: $0) }) {
            case .unchanged:
                cancel()
            case .invalid(let why):
                hint = why
            case .duplicate:
                hint = "Already in the list"
            case .saved(let next, let removed):
                r.urls = next
                if save(replacing: r) {
                    cancel()
                    if removed { say(Self.passkeyNote) }
                }
            }
        }
    }

    /// The favorite's shared speed caps (bytes/s; 0 or `nil` = unlimited). Applied live by the engine.
    func setLimits(_ id: String, down: UInt64, up: UInt64) {
        guard var r = record(id) else { return }
        let (d, u): (UInt64?, UInt64?) = (down == 0 ? nil : down, up == 0 ? nil : up)
        guard r.downLimit != d || r.upLimit != u else { return }
        r.downLimit = d
        r.upLimit = u
        _ = save(replacing: r)
    }

    // MARK: Removing

    /// A line of the list. The last one asks first (it would remove the favorite).
    func removeURL(_ id: String, index: Int) {
        reload()
        guard var r = record(id) else { return }
        switch FavoriteURLs.removing(r.urls, at: index) {
        case .removed(let next):
            r.urls = next
            _ = save(replacing: r)
        case .lastOne:
            askRemove(id, lastAddress: true)
        }
    }

    func askRemove(_ id: String, lastAddress: Bool = false) {
        reload()
        guard let r = record(id) else { return }
        cancel()
        if !lastAddress { openTab(id) }
        removal = Removal(id: id, name: r.name, rules: Int(store.favoriteRuleCount(id: id)), lastAddress: lastAddress)
    }

    func cancelRemoval() { removal = nil }

    func confirmRemoval() {
        guard let q = removal else { return }
        removal = nil
        let rest = store.profilesLoad().filter { $0.id != q.id }
        do {
            records = try store.profilesSave(profiles: rest)
            gone(q.id)
            changed()
        } catch {
            hint = "\(error)"
        }
    }

    /// "Save as Favorite" on an automatic tracker.
    func saveAsFavorite(_ groupID: String) {
        do {
            let r = try store.favoriteAdd(groupId: groupID)
            reload()
            changed()
            openTab(r.id)
        } catch {
            hint = "\(error)"
        }
    }

    // MARK: Helpers

    private func save(replacing r: ProfileRecord) -> Bool {
        var all = store.profilesLoad()
        guard let i = all.firstIndex(where: { $0.id == r.id }) else { return false }
        all[i] = r
        do {
            records = try store.profilesSave(profiles: all)
            hint = nil
            changed()
            return true
        } catch {
            hint = "\(error)"
            return false
        }
    }

    private func clearTransient() {
        hint = nil
        removal = nil
        note = nil
    }

    private func say(_ s: String) {
        note = s
        noteTask?.cancel()
        noteTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(8))
            if !Task.isCancelled { self?.note = nil }
        }
    }
}
