import SwiftUI

// MARK: Folders (concept 7, SPEC §7A.3, §7A.4)

/// The orange explanation under a pair of locations on different disks (SPEC §7A.4).
struct DifferentDisksWarning: View {
    var body: some View {
        Label("On different disks: finishing a download copies the files instead of an instant move.",
              systemImage: "exclamationmark.triangle.fill")
            .font(.system(size: 12))
            .foregroundStyle(Color.orange)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14).padding(.vertical, 9)
    }
}

struct FoldersTab: View {
    @Bindable var model: SettingsModel
    @State private var expanded: Set<String> = []
    @State private var selectedWatch: Int?

    private var f: Binding<FolderSettings> { $model.app.folders }

    var body: some View {
        SettingsPage {
            SettingsGroup(title: "Locations") {
                SettingsRow(label: "Incomplete", hint: "Where a download is written until it finishes. Same disk = instant move when done.") {
                    PathField(path: f.incompleteDir)
                }
                SettingsRow(label: "Finished", hint: "Where finished files live and are seeded from. One copy only.") {
                    PathField(path: f.downloadDir)
                }
                if model.volumeWarnings.contains(where: { $0.category == nil }) { DifferentDisksWarning() }
            }
            watchGroup
            HStack(alignment: .top, spacing: 18) {
                SettingsGroup(title: "Organize", fillHeight: true) {
                    ToggleRow(title: "Organize by seed status", isOn: f.byStatus)
                    Text("Finished torrents sit in Active, Dormant, Stopped or Dead folders inside the Finished location, and move between them as their sharing state changes. Off keeps every torrent directly in the Finished location.")
                        .font(.system(size: 12)).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.horizontal, 14).padding(.vertical, 9)
                }
                SettingsGroup(title: "Detection", fillHeight: true) {
                    ToggleRow(title: "Skip samples, .nfo and junk files", isOn: f.skipJunk)
                    ToggleRow(title: "Ask when less than 80% confident", isOn: f.askWhenUnsure)
                    ToggleRow(title: "Look up titles online (TMDB, MusicBrainz)", isOn: f.onlineLookup)
                        .disabled(true).opacity(0.5)
                        .help("Not available yet")
                }
            }
            typesGroup
            SettingsGroup(title: "After adding a .torrent") {
                SettingsRow(label: "The .torrent file") {
                    Picker("", selection: f.torrentAfterAdd) {
                        Text("Move to Trash").tag(TorrentFileAction.trash)
                        Text("Move to an archive folder").tag(TorrentFileAction.archive)
                        Text("Leave it").tag(TorrentFileAction.leave)
                    }
                    .labelsHidden().fixedSize()
                    .help("What happens to the .torrent file once Seedwell has added it")
                }
                if f.wrappedValue.torrentAfterAdd == .archive {
                    SettingsRow(label: "Archive folder") {
                        PathField(path: Binding(get: { f.wrappedValue.archiveDir ?? "" }, set: { f.wrappedValue.archiveDir = $0.isEmpty ? nil : $0 }))
                    }
                }
            }
        }
    }

    // MARK: Watch folders

    private var watchGroup: some View {
        VStack(alignment: .leading, spacing: 8) {
            SettingsGroup(title: "Watch folders") {
                if f.wrappedValue.watch.isEmpty {
                    Text("No folder is watched. Add one to pick up new .torrent files by themselves.")
                        .font(.system(size: 12.5)).foregroundStyle(.secondary).padding(14)
                }
                ForEach(f.watch.indices, id: \.self) { i in
                    watchRow(i)
                }
            }
            ControlGroup {
                Button { addWatch() } label: { Image(systemName: "plus") }.help("Add a folder to watch")
                Button { removeWatch() } label: { Image(systemName: "minus") }
                    .disabled(selectedWatch == nil).help("Stop watching the selected folder")
            }
            .fixedSize()
        }
    }

    private func watchRow(_ i: Int) -> some View {
        let w = f.wrappedValue.watch[i]
        let wb = f.watch[i]
        return HStack(spacing: 12) {
            Image(systemName: w.category.map(\.symbol) ?? "tray.and.arrow.down.fill")
                .font(.system(size: 20)).foregroundStyle(Color.accentColor).frame(width: 30)
            VStack(alignment: .leading, spacing: 1) {
                Text(w.path).font(.system(size: 14, weight: .semibold)).lineLimit(1).truncationMode(.middle)
                Text(Self.detail(w)).font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer()
            Picker("", selection: wb.mode) {
                Text("Auto-add").tag(WatchModeSetting.auto)
                Text("Ask first").tag(WatchModeSetting.ask)
            }
            .labelsHidden().fixedSize()
            .help("Add new .torrent files from this folder by themselves, or ask first")
            Toggle("", isOn: wb.enabled).labelsHidden().toggleStyle(.switch)
                .help("Turn watching this folder on or off")
        }
        .padding(.horizontal, 14).padding(.vertical, 9)
        .background(selectedWatch == i ? Color.accentColor.opacity(0.14) : .clear)
        .contentShape(Rectangle())
        .onTapGesture { selectedWatch = selectedWatch == i ? nil : i }
        .contextMenu {
            Menu("Treat as") {
                Button("Detect the content type") { f.wrappedValue.watch[i].category = nil }
                Divider()
                ForEach(Category.all, id: \.self) { c in
                    Button(c.label) { f.wrappedValue.watch[i].category = c }
                }
            }
            Menu("Then") {
                Button("Move .torrent to Trash") { f.wrappedValue.watch[i].afterAdd = .trash }
                Button("Leave .torrent") { f.wrappedValue.watch[i].afterAdd = .leave }
            }
        }
    }

    private static func detail(_ w: WatchFolderSetting) -> String {
        var parts = [w.mode == .auto ? "Add automatically" : "Ask first"]
        switch w.afterAdd {
        case .trash: parts.append("then move .torrent to Trash")
        case .archive: parts.append("then archive .torrent")
        case .leave: parts.append("keep .torrent")
        }
        var s = parts.joined(separator: " · ")
        if let c = w.category { s = "Always treat as \(c.label) · " + s }
        return s
    }

    private func addWatch() {
        guard let path = FolderPicker.choose(start: "~") else { return }
        guard !f.wrappedValue.watch.contains(where: { $0.path == path }) else { return }
        f.wrappedValue.watch.append(WatchFolderSetting(path: path, enabled: true, mode: .ask, afterAdd: .leave,
                                                       archiveDir: nil, category: nil, rule: nil, since: nil))
        selectedWatch = f.wrappedValue.watch.count - 1
    }

    private func removeWatch() {
        guard let i = selectedWatch, f.wrappedValue.watch.indices.contains(i) else { return }
        f.wrappedValue.watch.remove(at: i)
        selectedWatch = nil
    }

    // MARK: Content types

    /// The content type only decides which optional folders a download uses (it never renames or
    /// nests anything): each type may have its own Incomplete and Finished folder.
    private var typesGroup: some View {
        SettingsGroup(title: "Folders by content type (optional)") {
            ForEach(f.wrappedValue.categories.indices, id: \.self) { i in
                let cat = f.wrappedValue.categories[i].category
                let key = cat.label
                VStack(spacing: 0) {
                    HStack(spacing: 10) {
                        Button {
                            if expanded.contains(key) { expanded.remove(key) } else { expanded.insert(key) }
                        } label: {
                            Image(systemName: "chevron.right").font(.system(size: 10, weight: .semibold))
                                .rotationEffect(.degrees(expanded.contains(key) ? 90 : 0))
                                .foregroundStyle(.secondary).frame(width: 14)
                        }
                        .buttonStyle(.plain)
                        .help("Folders for \(cat.label)")
                        Image(systemName: cat.symbol).frame(width: 22).foregroundStyle(.secondary)
                        Text(cat.label).fontWeight(.semibold).frame(width: 82, alignment: .leading)
                        Spacer(minLength: 8)
                        if overrideActive(cat) {
                            Text("custom folders").font(.system(size: 11.5)).foregroundStyle(Color.accentColor)
                        } else {
                            Text("same as above").font(.system(size: 12)).foregroundStyle(.secondary)
                        }
                        if model.volumeWarnings.contains(where: { $0.category == cat }) {
                            Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(Color.orange)
                                .help("On different disks: finishing a download copies the files")
                        }
                    }
                    .font(.system(size: 13))
                    .padding(.horizontal, 14).padding(.vertical, 8)
                    if expanded.contains(key), let ci = f.wrappedValue.categories.firstIndex(where: { $0.category == cat }) {
                        overrides(ci, cat)
                    }
                }
            }
        }
    }

    private func overrides(_ ci: Int, _ cat: Category) -> some View {
        VStack(spacing: 0) {
            SettingsRow(label: "Incomplete", hint: "Default: \(f.wrappedValue.incompleteDir)") {
                OptionalPathField(path: f.categories[ci].incompleteDir, placeholder: "Default", width: 220)
            }
            SettingsRow(label: "Finished", hint: "Default: \(f.wrappedValue.downloadDir)") {
                OptionalPathField(path: f.categories[ci].finishedDir, placeholder: "Default", width: 220)
            }
            if model.volumeWarnings.contains(where: { $0.category == cat }) { DifferentDisksWarning() }
        }
        .padding(.leading, 28)
        .background(Color.primary.opacity(0.03))
    }

    private func overrideActive(_ cat: Category) -> Bool {
        guard let o = f.wrappedValue.categories.first(where: { $0.category == cat }) else { return false }
        return o.incompleteDir != nil || o.finishedDir != nil
    }
}
