import SwiftUI

/// Where the file rows sit relative to the torrent row above them: the triangle column of the row,
/// then the rows of the tree. The header and the rows are separate children of the list, so a torrent
/// with thousands of files only builds the rows on screen.
enum FileLayout {
    /// Leading space of a torrent row before its icon: padding and the disclosure triangle.
    static let lead: CGFloat = 6
    static let triangle: CGFloat = 12
    static let gap: CGFloat = 5
    /// x of the torrent row's icon.
    static var iconX: CGFloat { lead + triangle + gap }
    static let indent: CGFloat = 16
    static let rowHeight: CGFloat = 21
    /// An open list is at most this tall (the core's `max_rows` rows); beyond it the group scrolls inside.
    static let maxListHeight: CGFloat = 280
    /// UserDefaults key of "Show All": the lists grow to their full height instead of scrolling inside (one choice
    /// for every torrent, the held-files list included).
    static let showAllKey = "fileListShowAll"
    static let sizeWidth: CGFloat = 62
    /// "↓ 12.3 MB/s" next to the size; fixed so the names do not move as the numbers change.
    static let rateWidth: CGFloat = 74
    static let progressWidth: CGFloat = 66
    static let priorityWidth: CGFloat = 62
    static let playWidth: CGFloat = 26
}

/// The disclosure triangle of a torrent row (Finder's list view), in a fixed column so every row aligns.
struct DisclosureTriangle: View {
    let expanded: Bool
    let toggle: () -> Void

    var body: some View {
        let words = fileListToggle(expanded: expanded)
        Button(action: toggle) {
            Image(systemName: "chevron.right")
                .font(Theme.micro.weight(.bold))
                .foregroundStyle(.secondary)
                .rotationEffect(.degrees(expanded ? 90 : 0))
                .frame(width: FileLayout.triangle, height: 22)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(words.help)
        .accessibilityLabel(words.label)
    }
}

/// Everything under an expanded torrent row: the header line, then the tree, as the core lays it out.
struct FileTreeBlock: View {
    let row: TransferRow
    let commands: Commands
    let store: FilesStore
    /// A click on a file row deselects the torrent rows.
    let clearRows: () -> Void

    var body: some View {
        if let v = store.views[row.id] {
            if let status = v.status {
                Text(status)
                    .font(Theme.secondary).foregroundStyle(.secondary)
                    .padding(.leading, FileLayout.iconX + 4).padding(.vertical, 8)
            } else {
                if let header = v.header { FilesHeader(id: row.id, header: header, store: store) }
                if let empty = v.empty {
                    Text(empty)
                        .font(Theme.secondary).foregroundStyle(.secondary)
                        .padding(.leading, FileLayout.iconX + 4).padding(.vertical, 6)
                }
                if !v.rows.isEmpty, v.maxRows == nil {
                    // No inner scroll: the rows are children of the outer list (still lazy there).
                    ForEach(v.rows, id: \.file.id) { f in
                        FileRowView(row: row, commands: commands, store: store, item: f, priorityHelp: v.priorityHelp,
                                    completeLabel: v.completeLabel, clearRows: clearRows)
                    }
                } else if !v.rows.isEmpty {
                    // Its own height, known from the fixed row height, so the inner scroll view takes no more room than
                    // the rows need; the rows stay lazy inside it.
                    ScrollView {
                        LazyVStack(spacing: 0) {
                            ForEach(v.rows, id: \.file.id) { f in
                                FileRowView(row: row, commands: commands, store: store, item: f, priorityHelp: v.priorityHelp,
                                            completeLabel: v.completeLabel, clearRows: clearRows)
                            }
                        }
                    }
                    .scrollBounceBehavior(.basedOnSize, axes: .vertical)
                    .scrollIndicators(.automatic)
                    .frame(height: CGFloat(min(Int(v.maxRows ?? 0), v.rows.count)) * FileLayout.rowHeight)
                }
                Color.clear.frame(height: 6)
            }
        }
    }
}

/// "12 of 13 files · 14.2 GB of 15.1 GB selected", Select All / None and (for long lists) a filter.
struct FilesHeader: View {
    let id: String
    let header: FileListHeader
    let store: FilesStore

    var body: some View {
        HStack(alignment: .center, spacing: 10) {
            Text(header.summary)
                .font(Theme.caption).monospacedDigit().foregroundStyle(Color.secondary)
                .lineLimit(1)
                .help(header.summaryHelp)
            Spacer(minLength: 8)
            if let filter = header.filter { filterField(filter) }
            if let showAll = header.showAll { ShowAllButton(button: showAll) }
            Button(header.selectAll.title) { store.selectAll(id, true) }.disabled(!header.selectAll.enabled)
                .help(header.selectAll.help)
            Button(header.selectNone.title) { store.selectAll(id, false) }.disabled(!header.selectNone.enabled)
                .help(header.selectNone.help)
        }
        .buttonStyle(.plain)
        .font(Theme.caption)
        .foregroundStyle(Color.accentColor)
        .padding(.leading, FileLayout.iconX).padding(.trailing, 10)
        .padding(.top, 0).padding(.bottom, 3)
    }

    private func filterField(_ filter: FileFilterField) -> some View {
        HStack(spacing: 4) {
            Image(systemName: "magnifyingglass").font(.system(size: Theme.Icon.small)).foregroundStyle(Color.secondary)
            TextField(filter.placeholder, text: Binding(get: { filter.text }, set: { store.setFilter(id, $0) }))
                .textFieldStyle(.plain).font(Theme.caption).foregroundStyle(Color.primary)
                .frame(width: 110)
            if !filter.text.isEmpty {
                Button { store.setFilter(id, "") } label: { Image(systemName: "xmark.circle.fill").font(.system(size: Theme.Icon.small)).foregroundStyle(Color.secondary) }
                    .help(filter.clearHelp)
                    .accessibilityLabel(filter.clearLabel)
            }
        }
        .padding(.horizontal, 6).padding(.vertical, 1.5)
        .background(RoundedRectangle(cornerRadius: 5).fill(Color.primary.opacity(0.07)))
    }
}

/// "Show All" / "Show Less" (SPEC 8.3), as the core words it: lifts the height cap of the file lists, for every list.
struct ShowAllButton: View {
    let button: FileListButton

    var body: some View {
        Button(button.title) { FilesStore.shared.showAll.toggle() }
            .help(button.help)
    }
}

struct FileRowView: View {
    let row: TransferRow
    let commands: Commands
    let store: FilesStore
    /// The row as the core lays it out.
    let item: FileListRow
    let priorityHelp: String
    let completeLabel: String
    let clearRows: () -> Void

    var body: some View {
        let f = item.file
        let skipped = f.check == .off
        HStack(spacing: 6) {
            if f.isFolder {
                Button { store.toggleFolder(row.id, f.id) } label: {
                    Image(systemName: "chevron.right")
                        .font(Theme.micro.weight(.bold)).foregroundStyle(.secondary)
                        .rotationEffect(.degrees(f.isOpen ? 90 : 0))
                        .frame(width: 12, height: 18).contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help(item.foldHelp)
                .accessibilityLabel(item.foldLabel)
            } else {
                Color.clear.frame(width: 12, height: 1)
            }
            checkbox
            Image(nsImage: FileIcons.icon(ext: f.isFolder ? "" : f.ext)).resizable().frame(width: 14, height: 14)
                .opacity(skipped ? 0.5 : 1)
            Text(f.name)
                .font(Theme.caption)
                .foregroundStyle(skipped ? .tertiary : .secondary)
                .lineLimit(1).truncationMode(.middle)
                .frame(maxWidth: .infinity, alignment: .leading)
            Text(f.rate)
                .font(Theme.micro).monospacedDigit().foregroundStyle(.tertiary)
                .lineLimit(1)
                .frame(width: FileLayout.rateWidth, alignment: .trailing)
            SizeCapsule(text: f.size, fraction: f.fill, label: item.sizeLabel)
            progress
            priorityMenu
            playSlot
        }
        .padding(.leading, FileLayout.iconX + CGFloat(f.depth) * FileLayout.indent)
        .padding(.trailing, 10)
        .frame(height: FileLayout.rowHeight)
        .background(RoundedRectangle(cornerRadius: 6).fill(item.selected ? Color.accentColor.opacity(0.22) : .clear))
        .contentShape(Rectangle())
        .onTapGesture {
            let m = NSEvent.clickModifiers
            Self.click(torrent: row.id, node: f.id, store: store, command: m.contains(.command), shift: m.contains(.shift), clearRows: clearRows)
        }
        .contextMenu { MenuEntriesView(entries: Self.menuEntries(row: row, commands: commands, store: store, node: f.id)) }
        .accessibilityElement(children: .combine)
    }

    /// A click on a file or folder row (also QA's `files select`): the torrent rows give up the selection, the file takes it.
    static func click(torrent: String, node: UInt32, store: FilesStore, command: Bool, shift: Bool, clearRows: () -> Void) {
        clearRows()
        store.select(torrent: torrent, node: node, command: command, shift: shift)
    }

    private var checkbox: some View {
        let c = item.file.check
        return Button { store.toggleCheck(row.id, item.file.id) } label: {
            Image(systemName: c == .on ? "checkmark.square.fill" : c == .mixed ? "minus.square.fill" : "square")
                .font(.system(size: Theme.Icon.regular))
                .foregroundStyle(c == .off ? Color.secondary : Color.accentColor)
                .frame(width: 18, height: 18).contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(item.checkHelp)
        .accessibilityLabel(item.file.name)
        .accessibilityValue(item.checkValue)
    }

    /// A check mark when everything wanted is here, else a thin bar and the percent.
    @ViewBuilder private var progress: some View {
        let file = item.file
        HStack(spacing: 5) {
            if file.check == .off {
                Text("").frame(width: FileLayout.progressWidth)
            } else if file.complete {
                Image(systemName: "checkmark").font(Theme.micro.weight(.semibold)).foregroundStyle(.secondary)
                    .frame(width: FileLayout.progressWidth, alignment: .trailing)
                    .accessibilityLabel(completeLabel)
            } else {
                ThinProgress(value: file.progress).frame(width: 30)
                Text("\(file.percent)%").font(Theme.micro).monospacedDigit().foregroundStyle(.secondary)
                    .frame(width: 28, alignment: .trailing)
            }
        }
        .frame(width: FileLayout.progressWidth, alignment: .trailing)
    }

    private var priorityMenu: some View {
        Menu {
            ForEach(item.priorities, id: \.title) { p in
                Button { store.setPriority(row.id, item.file.id, p.priority) } label: {
                    if p.checked { Label(p.title, systemImage: "checkmark") } else { Text(p.title) }
                }
            }
        } label: {
            Text(item.file.priorityLabel).font(Theme.micro).foregroundStyle(item.file.priority == .skip ? .tertiary : .secondary)
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .frame(width: FileLayout.priorityWidth, alignment: .trailing)
        .help(priorityHelp)
    }

    /// A Play button on every video, visible always; it streams a file that is not complete (selecting a
    /// skipped one) and opens a finished one.
    @ViewBuilder private var playSlot: some View {
        ZStack {
            if let play = item.play {
                Button { Self.play(row, node: item.file.id, player: nil, commands: commands, store: store) } label: {
                    PlayGlyph(size: Theme.Icon.small)
                        .frame(width: FileLayout.playWidth, height: 19).contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help(play.help)
                .accessibilityLabel(play.help)
            }
        }
        .frame(width: FileLayout.playWidth)
    }

    static func play(_ row: TransferRow, node: UInt32, player id: String?, commands: Commands, store: FilesStore) {
        guard let file = store.lists.file(torrent: row.id, node: node) else { return }
        commands.play(row, file: file, with: id)
    }

    /// The context menu of a file or folder row, as the core makes it (also what QA shows as a native menu).
    static func menuEntries(row: TransferRow, commands: Commands, store: FilesStore, node: UInt32) -> [MenuEntry] {
        let entries = store.lists.menu(torrent: row.id, node: node, row: row, players: commands.core.playPlayers(),
                                       host: hostWindowsAll())
        return draw(entries, row: row, commands: commands, store: store, node: node)
    }

    private static func draw(_ entries: [FileMenuEntry], row: TransferRow, commands: Commands, store: FilesStore, node: UInt32) -> [MenuEntry] {
        entries.map { e in
            switch e {
            case let .item(title, command):
                .item(title) { run(command, row: row, commands: commands, store: store, node: node) }
            case let .note(title): .item(title, enabled: false) {}
            case let .submenu(title, entries): .submenu(title, draw(entries, row: row, commands: commands, store: store, node: node))
            case .separator: .separator
            }
        }
    }

    private static func run(_ command: FileCommand, row: TransferRow, commands: Commands, store: FilesStore, node: UInt32) {
        switch command {
        case let .reveal(path):
            // The file or folder where the core found it; the torrent's own folder when it is not on disk (yet).
            if let path { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)]) } else { commands.reveal([row]) }
        case let .setFilePriority(priority): store.setPriority(row.id, node, priority)
        case let .playFile(node, playerId): play(row, node: node, player: playerId, commands: commands, store: store)
        }
    }
}

extension NSEvent {
    /// The modifier keys of the click being handled: the event itself when there is one (so a posted
    /// click carries its own flags), else the keys held right now.
    @MainActor static var clickModifiers: ModifierFlags {
        if let e = NSApp.currentEvent, e.type == .leftMouseUp || e.type == .leftMouseDown { return e.modifierFlags }
        return modifierFlags
    }
}

/// The size of a file or folder in a subtle capsule that fills from the left with how much is downloaded
/// (all of it: the whole capsule). Nothing downloaded: plain text, no capsule.
struct SizeCapsule: View {
    let text: String
    /// 0...1
    let fraction: Double
    /// What a screen reader says ("1.2 GB, 42 percent downloaded", the core's words).
    let label: String

    var body: some View {
        let shown = fraction > 0
        Text(text)
            .font(Theme.micro).monospacedDigit()
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .frame(width: FileLayout.sizeWidth, height: 16)
            .background {
                if shown {
                    GeometryReader { geo in
                        ZStack(alignment: .leading) {
                            Color.primary.opacity(0.04)
                            Color.primary.opacity(0.12).frame(width: geo.size.width * min(1, fraction))
                        }
                        .clipShape(Capsule())
                    }
                }
            }
            .accessibilityLabel(label)
    }
}
