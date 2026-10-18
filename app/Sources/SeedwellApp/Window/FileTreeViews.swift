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
    /// An open list is at most this tall; beyond it the group scrolls inside (about 13 rows).
    static let maxListHeight: CGFloat = 280
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
        Button(action: toggle) {
            Image(systemName: "chevron.right")
                .font(.system(size: 9.5, weight: .bold))
                .foregroundStyle(.secondary)
                .rotationEffect(.degrees(expanded ? 90 : 0))
                .frame(width: FileLayout.triangle, height: 22)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(expanded ? "Hide the files" : "Show the files")
        .accessibilityLabel(expanded ? "Hide files" : "Show files")
    }
}

/// Everything under an expanded torrent row: the header line, then the tree.
struct FileTreeBlock: View {
    let row: TransferRow
    let commands: Commands
    let store: FilesStore
    @Binding var selection: Set<String>

    var body: some View {
        if let st = store.states[row.id] {
            FilesHeader(state: st)
            if let tree = st.tree {
                if st.visible.isEmpty {
                    Text(st.filter.isEmpty ? "No files." : "No files match.")
                        .font(.system(size: 12)).foregroundStyle(.secondary)
                        .padding(.leading, FileLayout.iconX + 4).padding(.vertical, 6)
                }
                // Its own height, known from the fixed row height, so the inner scroll view takes no more
                // room than the rows need; the rows stay lazy inside it.
                let height = min((FileLayout.maxListHeight / FileLayout.rowHeight).rounded(.down) * FileLayout.rowHeight, CGFloat(st.visible.count) * FileLayout.rowHeight)
                if st.visible.count > 0 {
                    ScrollView {
                        LazyVStack(spacing: 0) {
                            ForEach(st.visible) { v in
                                FileRowView(row: row, commands: commands, state: st, store: store, tree: tree, node: v.id, depth: v.depth, selection: $selection)
                            }
                        }
                    }
                    .scrollBounceBehavior(.basedOnSize, axes: .vertical)
                    .scrollIndicators(.automatic)
                    .frame(height: height)
                }
                Color.clear.frame(height: 6)
            } else {
                Text(st.failed ? "Couldn't read the files." : "Loading files…")
                    .font(.system(size: 12)).foregroundStyle(.secondary)
                    .padding(.leading, FileLayout.iconX + 4).padding(.vertical, 8)
            }
        }
    }
}

/// "12 of 13 files · 14.2 GB of 15.1 GB selected", Select All / None and (for long lists) a filter.
struct FilesHeader: View {
    @Bindable var state: FileTreeState

    var body: some View {
        if let t = state.tree {
            HStack(alignment: .center, spacing: 10) {
                Text(String(t.wantedCount) + " of " + String(t.fileCount) + " files · \(Fmt.bytes(t.wantedLength)) of \(Fmt.bytes(t.totalLength)) selected")
                    .font(.system(size: 11)).monospacedDigit().foregroundStyle(Color.secondary)
                    .lineLimit(1)
                    .help("Skipped files stay on disk")
                Spacer(minLength: 8)
                if t.fileCount > 20 { filterField }
                Button("Select All") { state.selectAll(true) }.disabled(t.wantedCount == t.fileCount && state.filter.isEmpty)
                    .help("Download every file")
                Button("Select None") { state.selectAll(false) }.disabled(t.wantedCount == 0 && state.filter.isEmpty)
                    .help("Skip every file; skipped files stay on disk")
            }
            .buttonStyle(.plain)
            .font(.system(size: 11))
            .foregroundStyle(Color.accentColor)
            .padding(.leading, FileLayout.iconX).padding(.trailing, 10)
            .padding(.top, 0).padding(.bottom, 3)
        }
    }

    private var filterField: some View {
        HStack(spacing: 4) {
            Image(systemName: "magnifyingglass").font(.system(size: 10.5)).foregroundStyle(Color.secondary)
            TextField("Filter files", text: $state.filter)
                .textFieldStyle(.plain).font(.system(size: 11)).foregroundStyle(Color.primary)
                .frame(width: 110)
            if !state.filter.isEmpty {
                Button { state.filter = "" } label: { Image(systemName: "xmark.circle.fill").font(.system(size: 11)).foregroundStyle(Color.secondary) }
                    .help("Clear the file filter")
                    .accessibilityLabel("Clear filter")
            }
        }
        .padding(.horizontal, 6).padding(.vertical, 1.5)
        .background(RoundedRectangle(cornerRadius: 5).fill(Color.primary.opacity(0.07)))
    }
}

struct FileRowView: View {
    let row: TransferRow
    let commands: Commands
    let state: FileTreeState
    let store: FilesStore
    let tree: FileTree
    let node: Int
    let depth: Int
    @Binding var selection: Set<String>

    var body: some View {
        let n = tree.nodes[node]
        let selected = store.isSelected(torrent: row.id, node: node)
        let open = state.expandedFolders.contains(node) || !state.filter.isEmpty
        HStack(spacing: 6) {
            if n.isFolder {
                Button { state.toggleFolder(node) } label: {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 8.5, weight: .bold)).foregroundStyle(.secondary)
                        .rotationEffect(.degrees(open ? 90 : 0))
                        .frame(width: 12, height: 18).contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help(open ? "Collapse this folder" : "Expand this folder")
                .accessibilityLabel(open ? "Collapse" : "Expand")
            } else {
                Color.clear.frame(width: 12, height: 1)
            }
            checkbox
            Image(nsImage: FileIcons.icon(ext: n.isFolder ? "" : n.ext)).resizable().frame(width: 14, height: 14)
                .opacity(tree.check(node) == .off ? 0.5 : 1)
            Text(n.name)
                .font(.system(size: 11.5))
                .foregroundStyle(tree.check(node) == .off ? .tertiary : .secondary)
                .lineLimit(1).truncationMode(.middle)
                .frame(maxWidth: .infinity, alignment: .leading)
            Text(tree.rateLabel(node) ?? "")
                .font(.system(size: 9.5)).monospacedDigit().foregroundStyle(.tertiary)
                .lineLimit(1)
                .frame(width: FileLayout.rateWidth, alignment: .trailing)
            SizeCapsule(text: Fmt.bytes(n.length), fraction: tree.fillFraction(node))
            progress
            priorityMenu
            playSlot
        }
        .padding(.leading, FileLayout.iconX + CGFloat(depth) * FileLayout.indent)
        .padding(.trailing, 10)
        .frame(height: FileLayout.rowHeight)
        .background(RoundedRectangle(cornerRadius: 6).fill(selected ? Color.accentColor.opacity(0.22) : .clear))
        .contentShape(Rectangle())
        .onTapGesture {
            let f = NSEvent.clickModifiers
            selection = []
            store.select(torrent: row.id, node: node, command: f.contains(.command), shift: f.contains(.shift))
        }
        .contextMenu { menu(n) }
        .accessibilityElement(children: .combine)
    }

    private var checkbox: some View {
        let c = tree.check(node)
        return Button { state.toggle(store.targets(torrent: row.id, node: node)) } label: {
            Image(systemName: c == .on ? "checkmark.square.fill" : c == .mixed ? "minus.square.fill" : "square")
                .font(.system(size: 12.5))
                .foregroundStyle(c == .off ? Color.secondary : Color.accentColor)
                .frame(width: 18, height: 18).contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(c == .on ? "Skip this; the file stays on disk if already downloaded" : "Download this")
        .accessibilityLabel(tree.nodes[node].name)
        .accessibilityValue(c == .on ? "Downloading" : c == .mixed ? "Some files" : "Skipped")
    }

    /// A check mark when everything wanted is here, else a thin bar and the percent.
    @ViewBuilder private var progress: some View {
        let skipped = tree.check(node) == .off
        HStack(spacing: 5) {
            if skipped {
                Text("").frame(width: FileLayout.progressWidth)
            } else if tree.isComplete(node) {
                Image(systemName: "checkmark").font(.system(size: 9.5, weight: .semibold)).foregroundStyle(.secondary)
                    .frame(width: FileLayout.progressWidth, alignment: .trailing)
                    .accessibilityLabel("Complete")
            } else {
                let p = tree.progress(node)
                ThinProgress(value: p).frame(width: 30)
                Text("\(Int((p * 100).rounded(.down)))%").font(.system(size: 10)).monospacedDigit().foregroundStyle(.secondary)
                    .frame(width: 28, alignment: .trailing)
            }
        }
        .frame(width: FileLayout.progressWidth, alignment: .trailing)
    }

    private var priorityMenu: some View {
        let shown = tree.shownPriority(node)
        return Menu {
            priorityButtons
        } label: {
            Text(Self.label(shown)).font(.system(size: 10.5)).foregroundStyle(shown == .skip ? .tertiary : .secondary)
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .frame(width: FileLayout.priorityWidth, alignment: .trailing)
        .help("Priority")
    }

    @ViewBuilder private var priorityButtons: some View {
        let shown = tree.shownPriority(node)
        ForEach([FilePriority.high, .normal, .low], id: \.self) { p in
            Button { state.setPriority(store.targets(torrent: row.id, node: node), p) } label: {
                if shown == p { Label(Self.label(p), systemImage: "checkmark") } else { Text(Self.label(p)) }
            }
        }
    }

    /// A Play button on every video, visible always; it streams a file that is not complete (selecting a
    /// skipped one) and opens a finished one.
    @ViewBuilder private var playSlot: some View {
        ZStack {
            if let f = tree.torrentFile(node), f.isVideo {
                Button { commands.play(row, file: f) } label: {
                    PlayGlyph(size: 10.5)
                        .frame(width: FileLayout.playWidth, height: 19).contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Play \(f.name)")
                .accessibilityLabel("Play \(f.name)")
            }
        }
        .frame(width: FileLayout.playWidth)
    }

    @ViewBuilder private func menu(_ n: FileTree.Node) -> some View {
        MenuEntriesView(entries: Self.menuEntries(row: row, commands: commands, state: state, store: store, tree: tree, node: node))
    }

    /// The context menu of a file or folder row (also what QA shows as a native menu).
    static func menuEntries(row: TransferRow, commands: Commands, state: FileTreeState, store: FilesStore, tree: FileTree, node: Int) -> [MenuEntry] {
        let n = tree.nodes[node]
        let targets = store.targets(torrent: row.id, node: node)
        var out: [MenuEntry] = [.item("Show in Finder") { reveal(row, n, tree: tree, node: node, commands) }]
        if let f = tree.torrentFile(node), f.isVideo { out += commands.playEntries(row: row, videos: [f]) }
        out.append(.separator)
        out.append(priorityMenu(state, targets))
        return out
    }

    static func priorityMenu(_ state: FileTreeState, _ targets: [Int]) -> MenuEntry {
        .submenu("Priority", [
            .item("High") { state.setPriority(targets, .high) },
            .item("Normal") { state.setPriority(targets, .normal) },
            .item("Low") { state.setPriority(targets, .low) },
            .separator,
            .item("Skip") { state.setPriority(targets, .skip) },
        ])
    }

    static func label(_ p: FilePriority?) -> String {
        switch p {
        case .high: "High"
        case .normal: "Normal"
        case .low: "Low"
        case .skip: "Skipped"
        case nil: "Mixed"
        }
    }

    /// The file or folder in Finder; the torrent's own folder when it is not on disk (yet).
    static func reveal(_ row: TransferRow, _ n: FileTree.Node, tree: FileTree, node: Int, _ commands: Commands) {
        // A file: its own location. A folder: the folder holding its first file.
        var target = URL(fileURLWithPath: row.savePath)
        if !n.disk.isEmpty { target = URL(fileURLWithPath: n.disk) }
        else if n.isFolder, let first = tree.fileNodes(under: [node]).first { target = URL(fileURLWithPath: tree.nodes[first].disk).deletingLastPathComponent() }
        if FileManager.default.fileExists(atPath: target.path) {
            NSWorkspace.shared.activateFileViewerSelecting([target])
        } else {
            commands.reveal([row])
        }
    }
}

extension NSEvent {
    /// The modifier keys of the click being handled: the event itself when there is one (so a posted
    /// click carries its own flags), else the keys held right now.
    static var clickModifiers: ModifierFlags {
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

    var body: some View {
        let shown = fraction > 0
        Text(text)
            .font(.system(size: 10.5)).monospacedDigit()
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
            .accessibilityLabel(text + (fraction >= 1 ? ", downloaded" : fraction > 0 ? ", \(Int((fraction * 100).rounded(.down))) percent downloaded" : ""))
    }
}
