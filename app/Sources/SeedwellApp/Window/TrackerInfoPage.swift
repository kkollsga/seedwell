import SwiftUI

/// What the Info page says about a tracker's health, from a few of its torrents' announce lines
/// (Get Info already has them; nothing new is tracked).
struct TrackerFacts: Equatable {
    /// Unix seconds of the newest successful announce.
    var lastAnnounce: UInt64?
    /// Mean gap between the last and the next announce of the lines that have both, seconds.
    var intervalSecs: UInt64?
    /// The first failure text ("failed: …"), if any line failed.
    var problem: String?
    /// Lines looked at.
    var sampled = 0

    /// `lines` are the tracker lines of the sampled torrents that belong to this tracker.
    static func summarize(_ lines: [TrackerLine]) -> TrackerFacts {
        var f = TrackerFacts()
        f.sampled = lines.count
        var gaps: [UInt64] = []
        for l in lines {
            if l.status.lowercased().hasPrefix("failed") || l.status.lowercased().contains("error") {
                if f.problem == nil { f.problem = l.status }
            } else if let last = l.lastAnnounce {
                f.lastAnnounce = max(f.lastAnnounce ?? 0, last)
            }
            if let last = l.lastAnnounce, let next = l.nextAnnounce, next > last { gaps.append(next - last) }
        }
        if !gaps.isEmpty { f.intervalSecs = gaps.reduce(0, +) / UInt64(gaps.count) }
        return f
    }

    /// The host of a (masked) announce URL, lower case.
    static func host(of url: String) -> String {
        var s = Substring(url)
        if let r = s.range(of: "://") { s = s[r.upperBound...] }
        let end = s.firstIndex { "/?#".contains($0) } ?? s.endIndex
        var h = String(s[..<end]).lowercased()
        if h.hasPrefix("["), let close = h.firstIndex(of: "]") { return String(h[h.index(after: h.startIndex)..<close]) }
        if let colon = h.lastIndex(of: ":"), h.filter({ $0 == ":" }).count == 1 { h = String(h[..<colon]) }
        return h
    }

    /// Whether an announce URL belongs to the tracker: its host is one of `hosts` (a `*.` pattern
    /// matches subdomains), or lies under the automatic tracker's `domain`.
    static func belongs(_ url: String, hosts: [String], domain: String?) -> Bool {
        let h = host(of: url)
        if let d = domain, h == d || h.hasSuffix("." + d) { return true }
        return hosts.contains { p in
            let ph = host(of: p.contains("://") ? p : "x://" + p)
            if ph.hasPrefix("*.") { return h.hasSuffix(String(ph.dropFirst(1))) }
            return ph == h
        }
    }
}

/// The Info page of a tracker (a view in the main window): interesting numbers, then the tracker's
/// addresses. A favorite's addresses are edited here with a pen, an x and a plus; an automatic
/// tracker's are read-only.
struct TrackerInfoPage: View {
    @Bindable var model: LibraryModel
    let tracker: TrackerSummary
    let commands: Commands
    @State private var facts = TrackerFacts()
    @State private var hosts: [String] = []
    @State private var seeding = 0
    @State private var downloading = 0
    @State private var busiest: TransferRow?
    /// The tracker's torrents moving now, bytes/s, against its shared caps.
    @State private var nowDown: UInt64 = 0
    @State private var nowUp: UInt64 = 0
    /// The folders being edited, until saved or reverted (a path typed half way must not move anything).
    @State private var folderDraft: ProfileRecord?

    var body: some View {
        let e = commands.favorites
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                Button { model.trackerInfo = false } label: {
                    Label("Torrents", systemImage: "chevron.left").font(.system(size: 12))
                }
                .buttonStyle(.plain).foregroundStyle(Color.accentColor)
                .help("Back to this tracker's torrents")

                HStack(spacing: 8) {
                    if tracker.favorite { Image(systemName: "star.fill").foregroundStyle(Color.accentColor.opacity(0.85)) }
                    Text(tracker.name).font(.system(size: 20, weight: .semibold)).lineLimit(1)
                    Text("Info").font(.system(size: 13)).foregroundStyle(.tertiary)
                }

                if let q = e.removal, q.id == tracker.id {
                    FavoriteRemoveStrip(question: q, confirm: e.confirmRemoval, cancel: e.cancelRemoval)
                }

                if let stats = model.trackerStats {
                    TrackerHeader(tracker: tracker, stats: stats, daily: model.daily, down: nowDown, up: nowUp).padding(.horizontal, -10)
                }
                factsBlock
                speedLimits(e)
                downloadLocation(e)
                addresses(e)
            }
            .padding(.horizontal, 22).padding(.vertical, 16)
            .frame(maxWidth: 640, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .task(id: tracker.id) {
            while !Task.isCancelled {
                load()
                try? await Task.sleep(for: .seconds(10))
            }
        }
        .task(id: tracker.id) {
            while !Task.isCancelled {
                rates()
                try? await Task.sleep(for: .seconds(2))
            }
        }
        .onChange(of: model.snapshot.trackers.map(\.count)) { _, _ in tally() }
        .onChange(of: tracker.id) { _, _ in folderDraft = nil }
    }

    // MARK: Facts

    private var factsBlock: some View {
        VStack(alignment: .leading, spacing: 7) {
            section("Now")
            fact("Torrents", "\(seeding) sharing · \(downloading) downloading")
            if let b = busiest, b.upRate > 0 { fact("Busiest now", "\(b.displayName) · ↑ \(Fmt.rate(b.upRate))") }
            fact("Tracker", stateText, error: facts.problem != nil)
            if let t = facts.lastAnnounce { fact("Last announce", Self.ago(t)) }
            if let i = facts.intervalSecs { fact("Announce interval", "about \(Self.span(i))") }
        }
    }

    private var stateText: String {
        if let p = facts.problem { return p }
        return facts.sampled == 0 ? "Not announced yet" : "Working"
    }

    private func fact(_ k: String, _ v: String, error: Bool = false) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Text(k).font(.system(size: 12)).foregroundStyle(.secondary).frame(width: 120, alignment: .leading)
            Text(v).font(.system(size: 13)).foregroundStyle(error ? Color.red : Color.primary).lineLimit(2)
        }
    }

    private func section(_ t: String) -> some View {
        Text(t).font(.system(size: 11, weight: .semibold)).foregroundStyle(.tertiary).textCase(.uppercase)
    }

    // MARK: Speed limits

    /// One cap for the tracker's torrents together (SPEC 7E.4), for a favorite; an automatic tracker
    /// says how to get one.
    @ViewBuilder private func speedLimits(_ e: FavoriteEditing) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            section("Speed limits")
            if tracker.favorite {
                let down = Binding<UInt64>(get: { e.record(tracker.id)?.downLimit ?? 0 }, set: { e.setLimits(tracker.id, down: $0, up: e.record(tracker.id)?.upLimit ?? 0) })
                let up = Binding<UInt64>(get: { e.record(tracker.id)?.upLimit ?? 0 }, set: { e.setLimits(tracker.id, down: e.record(tracker.id)?.downLimit ?? 0, up: $0) })
                limitRow("Download", down, now: nowDown)
                limitRow("Upload", up, now: nowUp)
                Text("One cap for all torrents on \(tracker.name) together. The global limits still apply; the lower one wins.")
                    .font(.system(size: 11)).foregroundStyle(.tertiary).fixedSize(horizontal: false, vertical: true)
            } else {
                Text("Save as Favorite to set limits").font(.system(size: 11)).foregroundStyle(.tertiary)
            }
        }
    }

    private func limitRow(_ k: String, _ limit: Binding<UInt64>, now: UInt64) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Text(k).font(.system(size: 12)).foregroundStyle(.secondary).frame(width: 120, alignment: .leading)
            RateField(bytes: limit)
            Text(RateText.line(rate: now, limit: limit.wrappedValue))
                .font(.system(size: 12)).monospacedDigit().foregroundStyle(.secondary)
        }
    }

    // MARK: Download location

    /// The favorite's own folders (SPEC 7A.4); an automatic tracker says how to get them.
    @ViewBuilder private func downloadLocation(_ e: FavoriteEditing) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            section("Download location")
            if tracker.favorite, let saved = e.record(tracker.id) {
                let draft = Binding<ProfileRecord>(get: { folderDraft ?? saved }, set: { folderDraft = $0 })
                TrackerLocationEditor(record: draft, warnings: e.volumeWarnings(for: draft.wrappedValue))
                if let d = folderDraft, !d.sameFolders(as: saved) {
                    HStack(spacing: 8) {
                        Spacer()
                        Button("Revert") { folderDraft = nil }.help("Discard the changed folders")
                        Button("Save") { e.saveFolders(tracker.id, from: d); folderDraft = nil }
                            .buttonStyle(.borderedProminent).help("Use these folders for this tracker")
                    }
                }
                if let h = e.hint { Text(h).font(.system(size: 11)).foregroundStyle(.red) }
            } else {
                Text("Save as Favorite to choose folders").font(.system(size: 11)).foregroundStyle(.tertiary)
            }
        }
        .id(tracker.id)
    }

    // MARK: Addresses

    @ViewBuilder private func addresses(_ e: FavoriteEditing) -> some View {
        let urls = tracker.favorite ? e.urls(tracker.id) : hosts
        VStack(alignment: .leading, spacing: 5) {
            section("Tracker URLs")
            ForEach(Array(urls.enumerated()), id: \.offset) { i, url in
                if tracker.favorite, e.target == .url(tracker.id, i) {
                    field(e, .url(tracker.id, i))
                } else {
                    HStack(spacing: 6) {
                        Text(url).font(.system(size: 12, design: .monospaced)).foregroundStyle(.secondary)
                            .lineLimit(1).truncationMode(.middle)
                            .textSelection(.enabled)
                        Spacer(minLength: 8)
                        if tracker.favorite {
                            RowIconButton(symbol: "pencil", help: "Edit this address") { e.beginEditURL(tracker.id, index: i) }
                            RowIconButton(symbol: "xmark", help: urls.count == 1 ? "Remove this address (it is the only one: removes the favorite)" : "Remove this address") {
                                e.removeURL(tracker.id, index: i)
                            }
                        }
                    }
                    .frame(minHeight: 22)
                }
            }
            if tracker.favorite, e.target == .url(tracker.id, nil) { field(e, .url(tracker.id, nil)) }
            if tracker.favorite {
                RowIconButton(symbol: "plus", help: "Add a tracker address") { e.beginAddURL(tracker.id) }
                if let note = e.note { Text(note).font(.system(size: 11)).foregroundStyle(.tertiary) }
            } else {
                Text("Save as Favorite to edit").font(.system(size: 11)).foregroundStyle(.tertiary)
            }
        }
    }

    private func field(_ e: FavoriteEditing, _ t: FavoriteEditing.Target) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            InlineField(editing: e, target: t, prompt: "tracker.example.org or a full announce URL", font: .system(size: 12, design: .monospaced))
            if let h = e.hint { Text(h).font(.system(size: 11)).foregroundStyle(.red).padding(.leading, 4) }
        }
    }

    // MARK: Loading

    private func load() {
        commands.favorites.reload()
        if !tracker.favorite { hosts = commands.core.trackerHosts(groupId: tracker.id) }
        tally()
        let rows = model.snapshot.allDownloading + model.snapshot.allQueued
            + commands.core.sharingRows(query: RowQuery(group: nil, trackerId: tracker.id, text: "", offset: 0, limit: 200))
        let mine = rows.filter { $0.trackerId == tracker.id && $0.kind == .torrent }
        busiest = mine.max { $0.upRate < $1.upRate }
        // A few torrents' announce lines tell the tracker's health; Get Info already has them.
        let urls = tracker.favorite ? commands.favorites.urls(tracker.id) : hosts
        var lines: [TrackerLine] = []
        for r in mine.prefix(4) {
            guard let info = try? commands.core.info(id: r.id) else { continue }
            lines += info.trackers.filter { TrackerFacts.belongs($0.url, hosts: urls, domain: tracker.favorite ? nil : tracker.id) }
        }
        facts = TrackerFacts.summarize(lines)
    }

    private func rates() {
        let s = model.snapshot
        let rows = s.allDownloading + s.allQueued + commands.core.sharingRows(query: RowQuery(group: nil, trackerId: tracker.id, text: "", offset: 0, limit: 200))
        let mine = rows.filter { $0.trackerId == tracker.id && $0.kind == .torrent }
        nowDown = mine.reduce(0) { $0 + $1.downRate }
        nowUp = mine.reduce(0) { $0 + $1.upRate }
    }

    private func tally() {
        let s = model.snapshot
        downloading = (s.allDownloading + s.allQueued).filter { $0.trackerId == tracker.id }.count
        let c = commands.core.sharingCounts(trackerId: tracker.id, category: nil, text: "")
        seeding = SeedGroup.allCases.reduce(0) { $0 + c.count($1) }
    }

    static func ago(_ unix: UInt64) -> String {
        let now = UInt64(Date().timeIntervalSince1970)
        return now <= unix ? "just now" : "\(span(now - unix)) ago"
    }

    static func span(_ s: UInt64) -> String {
        switch s {
        case ..<90: return "\(s) s"
        case ..<5400: return "\((s + 30) / 60) min"
        case ..<172_800: return "\((s + 1800) / 3600) h"
        default: return "\((s + 43_200) / 86_400) days"
        }
    }
}

/// A small borderless symbol button with hover text and a soft highlight.
struct RowIconButton: View {
    let symbol: String
    let help: String
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(hovering ? Color.primary : Color.secondary)
                .frame(width: 22, height: 20)
                .background(RoundedRectangle(cornerRadius: 5, style: .continuous).fill(Color.primary.opacity(hovering ? 0.10 : 0)))
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .help(help)
        .accessibilityLabel(help)
    }
}
