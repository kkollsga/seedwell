import SwiftUI

/// Shared scaffolding for the three list views: custom selection (click, ⌘-click, ⇧-click),
/// per-row context menu and group headers. Plain stacks instead of `List` for exact styling.
struct RowGroup: Identifiable {
    let id: String
    let title: String?
    let trailing: String?
    /// The longer explanation of the group, as a tooltip on its header instead of a line of text.
    var hint: String?
    let style: RowStyle
    let rows: [TransferRow]
    var action: (title: String, run: () -> Void)?
    /// A top-level block of a tracker tab ("Downloads", "Sharing"): a stronger header, no rows of its own.
    var isBlock = false
    /// The block this group sits in; folding the block hides it.
    var parent: String?
    /// Rows not fetched yet ("Show 80 more…").
    var more: (count: Int, run: () -> Void)?
}

/// Rows that scroll under the toolbar must never show through the title and buttons. The list runs
/// under the transparent toolbar (about 52 pt); an opaque strip of the window colour covers that band
/// and fades out below it. A row at rest starts under the fade, so the top of the list is untouched.
private struct TopScrim: ViewModifier {
    func body(content: Content) -> some View {
        content.overlay(alignment: .top) {
            LinearGradient(stops: [.init(color: Color(nsColor: .windowBackgroundColor), location: 0),
                                   .init(color: Color(nsColor: .windowBackgroundColor), location: 0.78),
                                   .init(color: Color(nsColor: .windowBackgroundColor).opacity(0), location: 1)],
                           startPoint: .top, endPoint: .bottom)
                .frame(height: 56)
                .ignoresSafeArea(.container, edges: .top)
                .allowsHitTesting(false)
                .accessibilityHidden(true)
        }
    }
}

struct RowList<Header: View>: View {
    @Bindable var model: LibraryModel
    let commands: Commands
    let groups: [RowGroup]
    @ViewBuilder var header: Header
    private let files = FilesStore.shared

    var body: some View {
        ScrollViewReader { proxy in
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0, pinnedViews: []) {
                if let c = model.categoryFilter {
                    CategoryFilterChip(category: c) { model.categoryFilter = nil }
                        .padding(.horizontal, 10).padding(.top, 10)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                header
                ForEach(groups.filter { !hidden($0) }) { g in
                    if let title = g.title { groupHeader(g, title) }
                    let folded = g.title != nil && model.isCollapsed(g.id)
                    ForEach(Array((folded ? [] : g.rows).enumerated()), id: \.element.id) { n, row in
                        // Divider snaps to whole device pixels; a 0.5 pt Rectangle vanishes at fractional offsets.
                        if n > 0 { Divider().opacity(0.6).padding(.leading, 52 + FileLayout.iconX).padding(.trailing, 10) }
                        rowView(row, style: g.style)
                            .onTapGesture { click(row, group: g.id) }
                            .contextMenu { MenuEntriesView(entries: commands.entries(for: target(row))) }
                        if let p = commands.removal.pending, !p.plural, row.id == removalAnchor {
                            RemoveConfirmStrip(pending: p, confirm: commands.confirmedRemove, cancel: commands.removal.cancel)
                                .id(RemoveConfirmStrip.id)
                        }
                        if row.awaitingChoice, files.isExpanded(row.id), let m = ChoiceStore.shared.models[row.id] {
                            HeldFilesBlock(model: m, id: row.id)
                        } else if files.isExpanded(row.id), row.fileCount > 1 {
                            FileTreeBlock(row: row, commands: commands, store: files, selection: $model.selection)
                        }
                    }
                    if !folded, let more = g.more {
                        Button("Show \(more.count) more…", action: more.run)
                            .buttonStyle(.plain)
                            .font(.system(size: 12.5)).foregroundStyle(Color.accentColor)
                            .padding(.horizontal, 10).padding(.vertical, 8)
                            .help("Show the other \(more.count) in this list")
                    }
                }
            }
            .padding(.horizontal, 22)
            .padding(.bottom, 20)
        }
        // The question sits under its row: bring that row into view when it is off screen.
        .onChange(of: commands.removal.pending) { _, p in
            guard let p, !p.plural, let id = removalAnchor else { return }
            // The row first (it may be far off screen and not built yet), then the strip just below it.
            proxy.scrollTo(id)
            DispatchQueue.main.async { withAnimation(.snappy(duration: 0.25)) { proxy.scrollTo(RemoveConfirmStrip.id) } }
        }
        // A new download: its row comes into view (also when the section just switched and the list is new).
        .task(id: model.scrollRequest) {
            guard let r = model.scrollRequest else { return }
            try? await Task.sleep(for: .milliseconds(200))
            withAnimation(.snappy(duration: 0.25)) { proxy.scrollTo(r.id, anchor: .center) }
            model.scrollDone(r)
        }
        }
        .scrollContentBackground(.hidden)
        .modifier(TopScrim())
        .id(model.section)  // a section starts at its top
        .focusable()
        .focusEffectDisabled()
        .onKeyPress(.space) {
            if files.selectedTorrent != nil, !files.selectedNodes.isEmpty { files.toggleSelected(); return .handled }
            guard model.selection.count == 1, let id = model.selection.first else { return .ignored }
            model.infoID = id
            return .handled
        }
        .onKeyPress(.rightArrow) {
            if files.openSelected() { return .handled }
            guard let row = singleSelected, row.fileCount > 1, !files.isExpanded(row.id) else { return .ignored }
            files.expand(row.id)
            return .handled
        }
        .onKeyPress(.leftArrow) {
            if files.selectedTorrent != nil {
                let r = files.closeSelected()
                if let id = r.selectTorrent { model.selection = [id] }
                return r.handled ? .handled : .ignored
            }
            guard let row = singleSelected, files.isExpanded(row.id) else { return .ignored }
            files.collapse(row.id)
            return .handled
        }
        .onChange(of: model.visibleRows.map(\.id)) { _, ids in commands.removal.visibleRowsChanged(Set(ids)) }
        .onKeyPress(.escape) {
            model.selection = []
            files.clearSelection()  // Escape drops a file selection too, not only the rows
            return .handled
        }
    }

    private func rowView(_ row: TransferRow, style: RowStyle) -> TransferRowView {
        let selected = model.selection.contains(row.id)
        let list: (expanded: Bool, toggle: () -> Void)? = hasList(row) ? (files.isExpanded(row.id), { files.toggle(row.id) }) : nil
        let filtered = model.categoryFilter == row.category
        return TransferRowView(row: row, style: style, selected: selected, commands: commands, infoID: $model.infoID,
                               showTracker: model.trackerID == nil, disclosure: list,
                               categoryAction: { model.toggleCategory($0) }, categoryActive: filtered)
    }

    /// A click on the title folds the group (its count stays); the chevron says which way.
    private func groupHeader(_ g: RowGroup, _ title: String) -> some View {
        let folded = model.isCollapsed(g.id)
        return HStack {
            Button {
                // Folded rows leave the selection, so Delete never acts on what cannot be seen.
                if !folded { model.drop(g.rows.map(\.id) + groups.filter { $0.parent == g.id }.flatMap { $0.rows.map(\.id) }) }
                withAnimation(.snappy(duration: 0.2)) { model.toggleCollapsed(g.id) }
            } label: {
                HStack(spacing: 5) {
                    Text(title).fontWeight(.semibold)
                        .font(.system(size: g.isBlock ? 13 : 12))
                        .foregroundStyle(g.isBlock ? Color.primary : g.style == .dead ? Color.red : .secondary)
                    Image(systemName: "chevron.down")
                        .font(.system(size: 9, weight: .semibold))
                        .foregroundStyle(.tertiary)
                        .rotationEffect(.degrees(folded ? -90 : 0))
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(folded ? "Show \(title)" : "Hide \(title)")
            .accessibilityLabel(folded ? "Show \(title)" : "Hide \(title)")
            Spacer()
            if let trailing = g.trailing { Text(trailing).fontWeight(.regular).monospacedDigit() }
            if let action = g.action { Button(action.title, action: action.run).buttonStyle(.plain).fontWeight(.regular).help("\(action.title) for this whole group") }
        }
        .font(.system(size: 12))
        .foregroundStyle(.secondary)
        .padding(.horizontal, 10)
        .padding(.top, g.isBlock ? 24 : g.parent != nil ? 12 : 20)
        .padding(.bottom, folded ? 0 : 6)
        .help(g.hint ?? "")
    }

    /// The row a single-torrent Remove… asks under (several ask in the title bar).
    private var removalAnchor: String? {
        guard let ids = commands.removal.pending.map({ Set($0.ids) }) else { return nil }
        return groups.flatMap(\.rows).last { ids.contains($0.id) }?.id
    }

    private var singleSelected: TransferRow? {
        model.selection.count == 1 ? model.selection.first.flatMap { id in model.visibleRows.first { $0.id == id } } : nil
    }

    /// Groups whose block is folded are not on screen.
    private func hidden(_ g: RowGroup) -> Bool { g.parent.map { model.isCollapsed($0) } ?? false }

    /// The triangle: a torrent with several files, or one that waits for a choice (selection mode, SPEC 8.3).
    private func hasList(_ row: TransferRow) -> Bool {
        if row.awaitingChoice { return (ChoiceStore.shared.models[row.id]?.fileFlags().count ?? 0) > 1 }
        return row.fileCount > 1 && row.kind == .torrent && !row.isHeld
    }

    private func click(_ row: TransferRow, group: String) {
        files.clearSelection()
        let flags = NSEvent.clickModifiers
        // Rows in list order across every group on screen; folded groups are skipped.
        let order = groups.filter { !hidden($0) && ($0.title == nil || !model.isCollapsed($0.id)) }
            .flatMap { g in g.rows.map { (group: g.id, id: $0.id) } }
        model.click(group: group, id: row.id, command: flags.contains(.command), shift: flags.contains(.shift), order: order)
    }

    /// Right-clicking inside a multi-selection acts on all of it; otherwise on that row alone.
    private func target(_ row: TransferRow) -> [TransferRow] {
        model.selection.contains(row.id) && model.selection.count > 1 ? model.rows(for: model.selection) : [row]
    }
}

extension RowList where Header == EmptyView {
    init(model: LibraryModel, commands: Commands, groups: [RowGroup]) {
        self.init(model: model, commands: commands, groups: groups) { EmptyView() }
    }
}

/// Builds the groups for each sidebar section.
@MainActor enum Lists {
    static func downloads(_ model: LibraryModel, commands: Commands) -> [RowGroup] {
        let s = model.snapshot
        let f = { (rows: [TransferRow]) in rows.filter(model.matches) }
        return [
            .init(id: "active", title: nil, trailing: nil, style: .download, rows: f(s.downloading)),
            .init(id: "next", title: "Up Next", trailing: nil, style: .queued, rows: f(s.queued)),
            .init(id: "recent", title: "Recently Finished", trailing: nil, style: .recent, rows: f(s.recent),
                  action: ("Clear", { commands.core.clearRecent() })),
        ].filter { !$0.rows.isEmpty }
    }

    /// The five groups of SPEC §8.3, each only when it has rows (plus Stopped for paused seeds).
    static func sharing(_ model: LibraryModel, commands: Commands) -> [RowGroup] {
        SeedGroup.allCases.compactMap { g in
            guard let rows = model.groups[g], !rows.isEmpty else { return nil }
            let n = model.counts.count(g)
            let hint: String? = switch g {
            case .dormant: "Resting in low power: still announcing on schedule, wakes instantly when a peer asks."
            case .error: "Retrying automatically."
            case .dead: "Nothing is removed automatically."
            default: nil
            }
            let more = model.remaining(g)
            return RowGroup(id: "\(g)", title: g.title, trailing: "\(n)", hint: hint, style: RowStyle(g), rows: rows,
                            more: more > 0 ? (more, { model.showMore(g) }) : nil)
        }
    }
}

extension Lists {
    /// A tracker tab (SPEC §8.3): a Downloads block (Downloading, Queued, and Recently Finished for downloads that
    /// are not torrents) and a Sharing block (the sharing groups), each subgroup only when it has rows and each
    /// block only when a subgroup does. A torrent that finished downloading leaves the Downloads block at once and
    /// is listed in its Sharing group, with its upload rate like any other seed.
    static func tracker(_ model: LibraryModel, commands: Commands) -> [RowGroup] {
        let s = model.snapshot
        return trackerGroups(downloading: s.allDownloading.filter(model.matches), queued: s.allQueued.filter(model.matches),
                             recent: s.allRecent.filter(model.matches), sharing: model.groups, counts: model.counts,
                             remaining: { model.remaining($0) }, showMore: { model.showMore($0) })
    }

    static func trackerGroups(downloading: [TransferRow], queued: [TransferRow], recent: [TransferRow],
                              sharing: [SeedGroup: [TransferRow]], counts: SharingCounts,
                              remaining: (SeedGroup) -> Int, showMore: @escaping (SeedGroup) -> Void) -> [RowGroup] {
        func sub(_ id: String, _ title: String, _ style: RowStyle, _ rows: [TransferRow], parent: String) -> RowGroup {
            RowGroup(id: id, title: title, trailing: "\(rows.count)", style: style, rows: rows, parent: parent)
        }
        // A finished torrent is a seed: it is in the Sharing groups, not here. Only what Sharing cannot list
        // (a finished direct download) stays under Recently Finished.
        let seeding = Set(sharing.values.flatMap { $0.map(\.id) })
        let finished = recent.filter { $0.kind != .torrent && !seeding.contains($0.id) }
        let downloads = [
            sub("downloading", "Downloading", .download, downloading, parent: "downloads"),
            sub("queued", "Queued", .queued, queued, parent: "downloads"),
            sub("recent", "Recently Finished", .recent, finished, parent: "downloads"),
        ].filter { !$0.rows.isEmpty }
        let seeds: [RowGroup] = SeedGroup.allCases.compactMap { g in
            let rows = sharing[g] ?? []
            guard !rows.isEmpty else { return nil }
            let n = max(rows.count, counts.count(g))
            let hint: String? = switch g {
            case .dormant: "Resting in low power: still announcing on schedule, wakes instantly when a peer asks."
            case .error: "Retrying automatically."
            case .dead: "Nothing is removed automatically."
            default: nil
            }
            let more = remaining(g)
            return RowGroup(id: "\(g)", title: g.title, trailing: "\(n)", hint: hint, style: RowStyle(g), rows: rows,
                            parent: "sharing", more: more > 0 ? (more, { showMore(g) }) : nil)
        }
        func block(_ id: String, _ title: String, _ kids: [RowGroup]) -> [RowGroup] {
            guard !kids.isEmpty else { return [] }
            let n = kids.reduce(0) { $0 + (Int($1.trailing ?? "") ?? $1.rows.count) }
            return [RowGroup(id: id, title: title, trailing: "\(n)", style: .download, rows: [], isBlock: true)] + kids
        }
        return block("downloads", "Downloads", downloads) + block("sharing", "Sharing", seeds)
    }
}
