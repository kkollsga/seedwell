import Foundation
import Observation

extension PendingStart {
    /// Seconds left at `now`, never negative. A stopped countdown stands still.
    func remaining(at now: Date) -> TimeInterval {
        if paused { return Double(remainingMs) / 1000 }
        return max(0, Double(startsAtMs) / 1000 - now.timeIntervalSince1970)
    }
}

extension TransferRow {
    /// Held by the engine: waiting for a choice, or counting down to its start. Neither announces,
    /// connects or has anything to play or list.
    var isHeld: Bool { rowIsHeld(row: self) }
    /// A magnet link that has not fetched its metadata: the engine does not know its files yet.
    var isUnresolvedMagnet: Bool { sizeBytes == 0 }
}

/// "Starting in 7 s", the one phrase of the countdown, in the notifier, the rows and the menu (the core's words).
enum StartingText {
    static func line(remaining: TimeInterval, paused: Bool) -> String {
        startingLine(remainingMs: UInt64(max(0, (remaining * 1000).rounded(.up))), paused: paused)
    }

    /// "Movie · ~/Media/Movies/…": what it is and where it goes.
    /// The size sits between them once the files are known (0 = a magnet link without metadata).
    static func detail(category: String?, size: UInt64 = 0, folder: String?) -> String {
        [category, size > 0 ? Fmt.bytes(size) : nil, folder].compactMap { $0 }.joined(separator: " · ")
    }
}

/// One torrent in the notifier.
struct StartingItem: Identifiable, Equatable {
    let id: String
    var title: String
    /// "Movie", or "Magnet link" while the engine does not know the files yet.
    var kind: String
    /// Where the files go, `~`-abbreviated.
    var folder: String?
    /// The torrent's total size; 0 while the files are not known.
    var size: UInt64 = 0
    /// The length of the wait as first seen: the ring's full circle.
    var total: TimeInterval
    /// When it starts (a running countdown) ...
    var startsAt: Date
    /// ... or how long was left when it was stopped.
    var stoppedWith: TimeInterval?
    /// The torrent waits for the user's choice of files (no countdown): the checklist is shown.
    var awaiting = false
    /// A magnet link whose file list is being fetched for the checklist.
    var fetching = false
    /// The detected category, for the pill (hidden while a magnet link has no files yet).
    var category: Category = .other
    /// Cancel would move a `.torrent` file to the Trash, so Shift has something to keep (SPEC 7A.6).
    var hasSource = false

    var paused: Bool { stoppedWith != nil }
    /// Waiting for the user rather than for the clock.
    var choosing: Bool { awaiting || fetching }

    func remaining(at now: Date) -> TimeInterval {
        stoppedWith ?? max(0, startsAt.timeIntervalSince(now))
    }

    /// How much of the ring is still full, 0...1.
    func fraction(at now: Date) -> Double {
        guard total > 0 else { return 0 }
        return min(1, max(0, remaining(at: now) / total))
    }

    func line(at now: Date) -> String {
        if fetching { return "Fetching file list…" }
        if awaiting { return "Choose what to download" }
        return StartingText.line(remaining: remaining(at: now), paused: paused)
    }

    var detail: String { StartingText.detail(category: kind, size: size, folder: folder) }
    /// A category is only known once the files are.
    var showsCategory: Bool { size > 0 }
}

/// What the notifier shows: every torrent that waits to start, each with its own countdown.
///
/// One row and one ring per torrent rather than a shared clock: the engine owns each deadline (torrents
/// added together share one to the second, so their rows tick in step and read as one countdown),
/// and a torrent added three seconds later must not be hurried by an earlier one.
@MainActor @Observable final class StartingModel {
    private(set) var items: [StartingItem] = []
    /// Torrents the user closed the notifier on; it opens again only for a torrent not in this set.
    private(set) var dismissed: Set<String> = []
    /// The one block whose checklist is open (SPEC 8.6); every other block is compact. Nil: all compact.
    private(set) var expandedID: String?

    /// Replaces the items with what the core reports (`rows` are the snapshot's downloading rows).
    /// `requested` are the magnet links whose file list the user asked for and is still coming.
    func sync(_ rows: [TransferRow], now: Date = Date(), home: String? = nil, requested: Set<String> = [],
              hasSource: (String) -> Bool = { _ in false },
              destination: (String) -> String? = { _ in nil }) {
        let known = Dictionary(uniqueKeysWithValues: items.map { ($0.id, $0) })
        let held = rows.filter { r in
            r.awaitingChoice || r.pending != nil || (requested.contains(r.id) && r.isUnresolvedMagnet)
        }
        items = held.compactMap { r -> StartingItem? in
            let old = known[r.id]
            if r.awaitingChoice || r.pending == nil {
                // Waiting for the user: no countdown; a magnet link without metadata is still fetching.
                return StartingItem(
                    id: r.id, title: r.displayName,
                    kind: r.isUnresolvedMagnet ? "Magnet link" : r.category.label,
                    folder: destination(r.id) ?? Self.folder(of: r.savePath, home: home), size: r.sizeBytes,
                    total: old?.total ?? 1, startsAt: now, stoppedWith: nil,
                    awaiting: !r.isUnresolvedMagnet, fetching: r.isUnresolvedMagnet, category: r.category, hasSource: hasSource(r.id))
            }
            guard let p = r.pending else { return nil }
            let left = p.remaining(at: now)
            return StartingItem(
                id: r.id,
                title: r.displayName,
                kind: r.isUnresolvedMagnet ? "Magnet link" : r.category.label,
                folder: Self.folder(of: r.savePath, home: home), size: r.sizeBytes,
                total: max(old?.total ?? 0, left, 1),
                startsAt: now.addingTimeInterval(left),
                stoppedWith: p.paused ? left : nil, category: r.category, hasSource: hasSource(r.id))
        }
        dismissed.formIntersection(Set(items.map(\.id)))
        // An open block goes with its torrent. A torrent between "countdown" and "awaiting" (the engine's answer
        // to Select Files… arrives a moment after the click) is in `rows` but not in `items`: it stays open.
        if let e = expandedID, !rows.contains(where: { $0.id == e }) { expandedID = nil }
    }

    /// Torrents already waiting for a choice when the app starts are not announced again by a popover.
    func dismissWaiting() { dismissed.formUnion(items.filter(\.choosing).map(\.id)) }

    /// Opens this block's checklist and compacts every other one.
    func expand(_ id: String) { expandedID = id }

    func collapse(_ id: String) { if expandedID == id { expandedID = nil } }

    func toggleExpanded(_ id: String) { if expandedID == id { expandedID = nil } else { expandedID = id } }

    func isExpanded(_ id: String) -> Bool { expandedID == id }

    /// Someone has to decide: the popover stays until OK, Cancel or its close button.
    var hasChoice: Bool { items.contains { $0.choosing && !dismissed.contains($0.id) } }

    /// The block that owns Return and Escape: the open checklist.
    var activeChoiceID: String? {
        items.first { $0.awaiting && $0.id == expandedID }?.id
    }

    /// The block that owns Return (Start) and Escape (Cancel): the open checklist, else the only block there is.
    var activeBlockID: String? {
        if let id = activeChoiceID { return id }
        return items.count == 1 ? items[0].id : nil
    }

    /// The user closed the notifier: leave it closed for these torrents.
    func dismissAll() { dismissed.formUnion(items.map(\.id)) }

    /// Something to show that the user has not already put away.
    var wantsToShow: Bool { items.contains { !dismissed.contains($0.id) } }

    /// The folder the files go to: where the row's path lives, `~`-abbreviated. A magnet link's
    /// path is the download folder itself.
    nonisolated static func folder(of savePath: String, home: String? = nil) -> String? {
        guard !savePath.isEmpty else { return nil }
        let parent = (savePath as NSString).deletingLastPathComponent
        let home = home ?? ProcessInfo.processInfo.environment["HOME"] ?? ""
        let dir = parent.isEmpty ? savePath : parent
        if !home.isEmpty, home != "/" {
            if dir == home { return "~" }
            if dir.hasPrefix(home + "/") { return "~" + dir.dropFirst(home.count) }
        }
        return dir
    }
}
