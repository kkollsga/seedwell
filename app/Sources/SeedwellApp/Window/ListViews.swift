import SwiftUI

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

/// The list of a list page as the core decides it (groups, rows, folds, "Show more", the selection and the Remove…
/// question): clicks, keys and the context menu go to the core. Plain stacks instead of `List` for exact styling.
struct RowList<Header: View>: View {
    @Bindable var model: LibraryModel
    let commands: Commands
    let list: ListModel
    @ViewBuilder var header: Header
    private let files = FilesStore.shared

    var body: some View {
        ScrollViewReader { proxy in
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 0, pinnedViews: []) {
                if let chip = list.filterChip {
                    CategoryFilterChip(chip: chip) { model.clearCategory() }
                        .padding(.horizontal, 10).padding(.top, 10)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                header
                ForEach(list.groups, id: \.key) { g in
                    if let title = g.title { groupHeader(g, title) }
                    ForEach(Array(g.rows.enumerated()), id: \.element.id) { n, row in
                        // Divider snaps to whole device pixels; a 0.5 pt Rectangle vanishes at fractional offsets.
                        if n > 0 { Divider().opacity(0.6).padding(.leading, 52 + FileLayout.iconX).padding(.trailing, 10) }
                        rowView(row)
                            .onTapGesture { click(row, group: g.key) }
                            .contextMenu { MenuEntriesView(entries: commands.entries(for: target(row))) }
                        if let p = model.view.removal, p.groupKey == g.key, p.rowId == row.id {
                            RemoveConfirmStrip(pending: p.question, confirm: model.confirmRemoval, cancel: model.cancelRemoval)
                                .id(RemoveConfirmStrip.id)
                        }
                        if row.transfer.awaitingChoice, files.isExpanded(row.id), let m = ChoiceStore.shared.models[row.id] {
                            HeldFilesBlock(model: m, id: row.id)
                        } else if files.isExpanded(row.id), row.transfer.fileCount > 1 {
                            FileTreeBlock(row: row.transfer, commands: commands, store: files) { model.setSelection([]) }
                        }
                    }
                    if let more = g.more {
                        Button(more.title) { model.showMore(more.group) }
                            .buttonStyle(.plain)
                            .font(.system(size: 12.5)).foregroundStyle(Color.accentColor)
                            .padding(.horizontal, 10).padding(.vertical, 8)
                            .help(more.help)
                    }
                }
            }
            .padding(.horizontal, 22)
            .padding(.bottom, 20)
        }
        // The question sits under its row: bring that row into view when it is off screen.
        .onChange(of: model.view.removal) { _, p in
            guard let id = p?.rowId else { return }
            // The row first (it may be far off screen and not built yet), then the strip just below it.
            proxy.scrollTo(id)
            DispatchQueue.main.async { withAnimation(.snappy(duration: 0.25)) { proxy.scrollTo(RemoveConfirmStrip.id) } }
        }
        // A row the core asks for comes into view: a new download centred once the list has settled (the section may
        // just have switched and the list be new), the row an arrow key moved to at once and only as far as it takes.
        .task(id: model.view.scrollTo) {
            guard let r = model.view.scrollTo else { return }
            if r.center {
                try? await Task.sleep(for: .milliseconds(200))
                withAnimation(.snappy(duration: 0.25)) { proxy.scrollTo(r.rowId, anchor: .center) }
            } else {
                proxy.scrollTo(r.rowId)
            }
            model.scrollDone(r.request)
        }
        }
        .scrollContentBackground(.hidden)
        .modifier(TopScrim())
        .id(model.section)  // a section starts at its top
        .focusable()
        .focusEffectDisabled()
        .onKeyPress(.space) { key(.space) }
        .onKeyPress(.rightArrow) { key(.right) }
        .onKeyPress(.leftArrow) { key(.left) }
        .onKeyPress(.escape) { key(.escape) }
        .onKeyPress(keys: [.upArrow, .downArrow]) { press in
            key(press.key == .upArrow ? .up : .down, KeyModifiers(press.modifiers))
        }
    }

    private func key(_ k: LibraryKey, _ modifiers: KeyModifiers = KeyModifiers(shift: false, command: false, option: false, control: false)) -> KeyPress.Result {
        model.key(k, modifiers, scope: .list) ? .handled : .ignored
    }

    private func rowView(_ row: ListRow) -> TransferRowView {
        let list: (expanded: Bool, toggle: () -> Void)? = hasList(row) ? (files.isExpanded(row.id), { files.toggle(row.id) }) : nil
        return TransferRowView(row: row, commands: commands, openInfo: { model.openInfo(row.id) }, infoClosed: { model.infoClosed(row.id) },
                               disclosure: list, categoryAction: { model.toggleCategory($0) })
    }

    /// A click on the title folds the group (its count stays); the chevron says which way.
    private func groupHeader(_ g: ListGroup, _ title: String) -> some View {
        HStack {
            Button {
                withAnimation(.snappy(duration: 0.2)) { model.toggleCollapsed(g.key) }
            } label: {
                HStack(spacing: 5) {
                    Text(title).fontWeight(.semibold)
                        .font(.system(size: g.block ? 13 : 12))
                        .foregroundStyle(g.block ? Color.primary : g.tone == .problem ? Color.red : .secondary)
                    Image(systemName: "chevron.down")
                        .font(.system(size: 9, weight: .semibold))
                        .foregroundStyle(.tertiary)
                        .rotationEffect(.degrees(g.folded ? -90 : 0))
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(g.foldHelp ?? "")
            .accessibilityLabel(g.foldHelp ?? title)
            Spacer()
            if let count = g.count { Text(count).fontWeight(.regular).monospacedDigit() }
            if let action = g.action {
                Button(action.title) { run(action.command) }.buttonStyle(.plain).fontWeight(.regular).help(action.help)
            }
        }
        .font(.system(size: 12))
        .foregroundStyle(.secondary)
        .padding(.horizontal, 10)
        .padding(.top, g.block ? 24 : g.parent != nil ? 12 : 20)
        .padding(.bottom, g.folded ? 0 : 6)
        .help(g.hint ?? "")
    }

    private func run(_ command: ListCommand) {
        switch command {
        case .clearRecent: commands.core.clearRecent()
        }
    }

    /// The triangle: a torrent with several files, or one that waits for a choice while its chooser has several.
    private func hasList(_ row: ListRow) -> Bool {
        switch row.disclosure {
        case .files: true
        case .choice: (ChoiceStore.shared.models[row.id]?.fileFlags().count ?? 0) > 1
        case nil: false
        }
    }

    private func click(_ row: ListRow, group: String) {
        let flags = NSEvent.clickModifiers
        model.click(group: group, id: row.id, command: flags.contains(.command), shift: flags.contains(.shift))
    }

    /// What a context menu on the row acts on (the whole selection when the row is in it).
    private func target(_ row: ListRow) -> [TransferRow] { model.menuRows(row.id) }
}

extension RowList where Header == EmptyView {
    init(model: LibraryModel, commands: Commands, list: ListModel) {
        self.init(model: model, commands: commands, list: list) { EmptyView() }
    }
}

extension KeyModifiers {
    /// The modifiers of a SwiftUI key press.
    init(_ m: EventModifiers) {
        self.init(shift: m.contains(.shift), command: m.contains(.command), option: m.contains(.option), control: m.contains(.control))
    }
}
