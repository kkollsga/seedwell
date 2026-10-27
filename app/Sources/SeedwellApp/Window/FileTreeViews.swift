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
    @AppStorage(FileLayout.showAllKey) private var showAll = false

    var body: some View {
        if let st = store.states[row.id] {
            FilesHeader(state: st)
            if st.tree != nil {
                if st.rows.isEmpty {
                    Text(st.filter.isEmpty ? "No files." : "No files match.")
                        .font(.system(size: 12)).foregroundStyle(.secondary)
                        .padding(.leading, FileLayout.iconX + 4).padding(.vertical, 6)
                }
                // Its own height, known from the fixed row height, so the inner scroll view takes no more
                // room than the rows need; the rows stay lazy inside it.
                let height = min((FileLayout.maxListHeight / FileLayout.rowHeight).rounded(.down) * FileLayout.rowHeight, CGFloat(st.rows.count) * FileLayout.rowHeight)
                if st.rows.count > 0, showAll {
                    // No inner scroll: the rows are children of the outer list (still lazy there).
                    ForEach(st.rows, id: \.id) { f in
                        FileRowView(row: row, commands: commands, state: st, store: store, file: f, selection: $selection)
                    }
                } else if st.rows.count > 0 {
                    ScrollView {
                        LazyVStack(spacing: 0) {
                            ForEach(st.rows, id: \.id) { f in
                                FileRowView(row: row, commands: commands, state: st, store: store, file: f, selection: $selection)
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
        if let t = state.totals {
            HStack(alignment: .center, spacing: 10) {
                Text(t.summary)
                    .font(.system(size: 11)).monospacedDigit().foregroundStyle(Color.secondary)
                    .lineLimit(1)
                    .help("Skipped files stay on disk")
                Spacer(minLength: 8)
                if t.fileCount > 20 { filterField }
                ShowAllButton(overflows: CGFloat(state.rows.count) * FileLayout.rowHeight > FileLayout.maxListHeight)
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

/// "Show All" / "Show Less": lifts the height cap of the file lists (SPEC 8.3). It shows only while a list is
/// taller than the cap, or while the cap is lifted (so it can be put back).
struct ShowAllButton: View {
    let overflows: Bool
    @AppStorage(FileLayout.showAllKey) private var showAll = false

    var body: some View {
        if overflows || showAll {
            Button(showAll ? "Show Less" : "Show All") { showAll.toggle() }
                .help(showAll ? "Scroll long file lists inside their row again" : "Grow long file lists to their full height instead of scrolling inside")
        }
    }
}

struct FileRowView: View {
    let row: TransferRow
    let commands: Commands
    let state: FileTreeState
    let store: FilesStore
    /// The row as the core lays it out.
    let file: FileRow
    @Binding var selection: Set<String>

    var body: some View {
        let f = file
        let selected = store.isSelected(torrent: row.id, node: f.id)
        let skipped = f.check == .off
        HStack(spacing: 6) {
            if f.isFolder {
                Button { state.toggleFolder(f.id) } label: {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 8.5, weight: .bold)).foregroundStyle(.secondary)
                        .rotationEffect(.degrees(f.isOpen ? 90 : 0))
                        .frame(width: 12, height: 18).contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help(f.isOpen ? "Collapse this folder" : "Expand this folder")
                .accessibilityLabel(f.isOpen ? "Collapse" : "Expand")
            } else {
                Color.clear.frame(width: 12, height: 1)
            }
            checkbox
            Image(nsImage: FileIcons.icon(ext: f.isFolder ? "" : f.ext)).resizable().frame(width: 14, height: 14)
                .opacity(skipped ? 0.5 : 1)
            Text(f.name)
                .font(.system(size: 11.5))
                .foregroundStyle(skipped ? .tertiary : .secondary)
                .lineLimit(1).truncationMode(.middle)
                .frame(maxWidth: .infinity, alignment: .leading)
            Text(f.rate)
                .font(.system(size: 9.5)).monospacedDigit().foregroundStyle(.tertiary)
                .lineLimit(1)
                .frame(width: FileLayout.rateWidth, alignment: .trailing)
            SizeCapsule(text: f.size, fraction: f.fill)
            progress
            priorityMenu
            playSlot
        }
        .padding(.leading, FileLayout.iconX + CGFloat(f.depth) * FileLayout.indent)
        .padding(.trailing, 10)
        .frame(height: FileLayout.rowHeight)
        .background(RoundedRectangle(cornerRadius: 6).fill(selected ? Color.accentColor.opacity(0.22) : .clear))
        .contentShape(Rectangle())
        .onTapGesture {
            let m = NSEvent.clickModifiers
            selection = []
            store.select(torrent: row.id, node: f.id, command: m.contains(.command), shift: m.contains(.shift))
        }
        .contextMenu { MenuEntriesView(entries: Self.menuEntries(row: row, commands: commands, state: state, store: store, file: f)) }
        .accessibilityElement(children: .combine)
    }

    private var checkbox: some View {
        let c = file.check
        return Button { state.toggle(store.targets(torrent: row.id, node: file.id)) } label: {
            Image(systemName: c == .on ? "checkmark.square.fill" : c == .mixed ? "minus.square.fill" : "square")
                .font(.system(size: 12.5))
                .foregroundStyle(c == .off ? Color.secondary : Color.accentColor)
                .frame(width: 18, height: 18).contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(c == .on ? "Skip this; the file stays on disk if already downloaded" : "Download this")
        .accessibilityLabel(file.name)
        .accessibilityValue(c == .on ? "Downloading" : c == .mixed ? "Some files" : "Skipped")
    }

    /// A check mark when everything wanted is here, else a thin bar and the percent.
    @ViewBuilder private var progress: some View {
        HStack(spacing: 5) {
            if file.check == .off {
                Text("").frame(width: FileLayout.progressWidth)
            } else if file.complete {
                Image(systemName: "checkmark").font(.system(size: 9.5, weight: .semibold)).foregroundStyle(.secondary)
                    .frame(width: FileLayout.progressWidth, alignment: .trailing)
                    .accessibilityLabel("Complete")
            } else {
                ThinProgress(value: file.progress).frame(width: 30)
                Text("\(file.percent)%").font(.system(size: 10)).monospacedDigit().foregroundStyle(.secondary)
                    .frame(width: 28, alignment: .trailing)
            }
        }
        .frame(width: FileLayout.progressWidth, alignment: .trailing)
    }

    private var priorityMenu: some View {
        Menu {
            priorityButtons
        } label: {
            Text(file.priorityLabel).font(.system(size: 10.5)).foregroundStyle(file.priority == .skip ? .tertiary : .secondary)
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .frame(width: FileLayout.priorityWidth, alignment: .trailing)
        .help("Priority")
    }

    @ViewBuilder private var priorityButtons: some View {
        ForEach([FilePriority.high, .normal, .low], id: \.self) { p in
            Button { state.setPriority(store.targets(torrent: row.id, node: file.id), p) } label: {
                if file.priority == p { Label(filePriorityLabel(p: p), systemImage: "checkmark") } else { Text(filePriorityLabel(p: p)) }
            }
        }
    }

    /// A Play button on every video, visible always; it streams a file that is not complete (selecting a
    /// skipped one) and opens a finished one.
    @ViewBuilder private var playSlot: some View {
        ZStack {
            if file.isVideo, let f = state.tree?.torrentFile(id: file.id) {
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

    /// The context menu of a file or folder row (also what QA shows as a native menu).
    static func menuEntries(row: TransferRow, commands: Commands, state: FileTreeState, store: FilesStore, file: FileRow) -> [MenuEntry] {
        let targets = store.targets(torrent: row.id, node: file.id)
        var out: [MenuEntry] = [.item("Show in Finder") { reveal(row, state.tree?.diskLocation(id: file.id), commands) }]
        if file.isVideo, let f = state.tree?.torrentFile(id: file.id) { out += commands.playEntries(row: row, videos: [f]) }
        out.append(.separator)
        out.append(priorityMenu(state, targets))
        return out
    }

    static func priorityMenu(_ state: FileTreeState, _ targets: [UInt32]) -> MenuEntry {
        .submenu("Priority", [
            .item("High") { state.setPriority(targets, .high) },
            .item("Normal") { state.setPriority(targets, .normal) },
            .item("Low") { state.setPriority(targets, .low) },
            .separator,
            .item("Skip") { state.setPriority(targets, .skip) },
        ])
    }

    /// The file or folder in Finder (where the core says it is); the torrent's own folder when it is not on disk (yet).
    static func reveal(_ row: TransferRow, _ location: String?, _ commands: Commands) {
        let target = URL(fileURLWithPath: location ?? row.savePath)
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
