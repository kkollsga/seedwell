import SwiftUI

struct MainWindowView: View {
    @Bindable var model: LibraryModel
    let commands: Commands
    let settings: SettingsModel
    let nav: SettingsNavigation

    var body: some View {
        NavigationSplitView {
            SidebarView(model: model, commands: commands)
                .navigationSplitViewColumnWidth(min: 200, ideal: 232, max: 280)
        } detail: {
            DetailView(model: model, commands: commands, settings: settings, nav: nav)
        }
        .frame(minWidth: 820, minHeight: 480)
    }
}

struct SidebarView: View {
    @Bindable var model: LibraryModel
    let commands: Commands

    var body: some View {
        let s = model.snapshot
        List(selection: Binding(get: { model.section }, set: { if let new = $0, new != model.section { commands.removal.userChangedSection(); model.select(new) } })) {
            Label {
                HStack(spacing: 6) {
                    Text("Downloads")
                    // A new download arrived while the user was in Settings or editing: a quiet dot, not a switch.
                    if model.downloadsFlag {
                        Circle().fill(Color.accentColor).frame(width: 6, height: 6)
                            .help("A new download started")
                            .accessibilityLabel("New download")
                    }
                }
            } icon: { Image(systemName: "arrow.down.circle") }
                .badge(s.downloading.count + s.queued.count)
                .help("Torrents that are downloading or waiting")
                .tag(LibraryModel.Section.downloads)
            Label("Sharing", systemImage: "arrow.up.circle")
                .badge(Int(s.sharingTotal))
                .help("Torrents you are seeding")
                .tag(LibraryModel.Section.sharing)
            Label("Folder Sync", systemImage: "arrow.triangle.2.circlepath")
                .badge(model.sync.folders.count)
                .help("Folders kept the same on your devices")
                .tag(LibraryModel.Section.folderSync)
            let shownTrackers = TrackerOrder.visible(s.trackers, selected: model.trackerID)
            if !shownTrackers.isEmpty {
            Section("Trackers") {
                ForEach(shownTrackers, id: \.id) { t in
                    TrackerRow(tracker: t, editing: commands.favorites)
                        .tag(LibraryModel.Section.tracker(t.id))
                        .help(t.favorite ? "Favorite tracker: every torrent on \(t.name)" : "Every torrent on \(t.name), downloading and sharing")
                        .contextMenu { MenuEntriesView(entries: TrackerMenus.entries(for: t, commands: commands)) }
                }
            }
            }
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom, spacing: 0) {
            HStack(spacing: 10) {
                if s.online {
                    // Only the live rates; the limits are in the tooltips.
                    Text("↓ \(Fmt.rate(s.downRate))")
                        .font(.system(size: 13, weight: .medium)).foregroundStyle(.primary)
                        .help(RateText.limitHelp("Download", limit: s.downLimit, altSpeed: s.altSpeed))
                    Text("↑ \(Fmt.rate(s.upRate))")
                        .font(.system(size: 13, weight: .medium)).foregroundStyle(.primary)
                        .help(RateText.limitHelp("Upload", limit: s.upLimit, altSpeed: s.altSpeed))
                    if s.altSpeed {
                        Image(systemName: "tortoise.fill").font(.system(size: 10)).foregroundStyle(.secondary)
                            .help("Limit Speed is on (⌥⌘L)")
                    }
                } else {
                    // Quiet, not an alert: the engine holds its requests and checks again by itself.
                    Text("Offline")
                        .help("No network. Seedwell sends almost nothing until it is back, and picks up by itself.")
                }
                Spacer()
                SidebarIconButton(symbol: model.section == .settings ? "gearshape.fill" : "gearshape", help: "Settings (⌘,)",
                                  active: model.section == .settings) { commands.showSettings(nil) }
            }
            .font(.system(size: 11.5)).monospacedDigit()
            .lineLimit(1).minimumScaleFactor(0.7)
            .foregroundStyle(.secondary)
            .padding(.leading, 18).padding(.trailing, 10).padding(.vertical, 8)
        }
    }
}

/// A borderless sidebar button: a secondary-colour symbol with a soft highlight under the pointer.
struct SidebarIconButton: View {
    let symbol: String
    let help: String
    var active = false
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 13, weight: .regular))
                .foregroundStyle(active ? Color.accentColor : hovering ? Color.primary : Color.secondary)
                .frame(width: 26, height: 24)
                .background(RoundedRectangle(cornerRadius: 6, style: .continuous).fill(Color.primary.opacity(hovering || active ? 0.10 : 0)))
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .help(help)
        .accessibilityLabel(help.replacingOccurrences(of: " (⌘,)", with: ""))
    }
}

struct DetailView: View {
    @Bindable var model: LibraryModel
    let commands: Commands
    let settings: SettingsModel
    let nav: SettingsNavigation

    var body: some View {
        content
            .navigationTitle(AppInfo.name)
            .toolbar(removing: .title)
            .toolbar {
                TitleToolbarItem(title: model.title, summary: model.summary, favorite: model.tracker?.favorite == true)
                RemoveConfirmToolbarItem(commands: commands)
            }
            .sheet(isPresented: $model.adding) { AddSheet(commands: commands, isPresented: $model.adding) }
            .dropDestination(for: URL.self) { urls, _ in commands.add(urls: urls); return true }
            .overlay(alignment: .bottom) { PlayNoticeView(notice: commands.playNotice) }
            .animation(.easeOut(duration: 0.15), value: commands.playNotice.text)
    }

    @ViewBuilder private var content: some View {
        switch model.section {
        case .settings:
            SettingsView(model: settings, nav: nav)
        case .folderSync:
            SyncPage(model: model, actions: commands.sync)
        case .downloads:
            let groups = Lists.downloads(model, commands: commands)
            RowList(model: model, commands: commands, groups: groups) {
                if groups.isEmpty { EmptyListNote(text: model.search.isEmpty && model.categoryFilter == nil ? EmptyListNote.downloads : EmptyListNote.noMatches) }
            }
        case .sharing:
            let groups = Lists.sharing(model, commands: commands)
            RowList(model: model, commands: commands, groups: groups) {
                if groups.isEmpty { EmptyListNote(text: model.search.isEmpty && model.categoryFilter == nil ? EmptyListNote.sharing : EmptyListNote.noMatches) }
            }
        case .tracker where model.trackerInfo && model.tracker != nil:
            TrackerInfoPage(model: model, tracker: model.tracker!, commands: commands)
        case .tracker:
            let groups = Lists.tracker(model, commands: commands)
            RowList(model: model, commands: commands, groups: groups) {
                if let t = model.tracker {
                    if let q = commands.favorites.removal, q.id == t.id {
                        FavoriteRemoveStrip(question: q, confirm: commands.favorites.confirmRemoval, cancel: commands.favorites.cancelRemoval)
                            .padding(.horizontal, 10).padding(.top, 8)
                    }
                    if let stats = model.trackerStats { TrackerHeader(tracker: t, stats: stats, daily: model.daily, down: model.trackerRates.down, up: model.trackerRates.up) }
                }
                if groups.isEmpty {
                    Text(model.search.isEmpty && model.categoryFilter == nil ? "No torrents on \(model.tracker?.name ?? "this tracker")" : "No matches on \(model.tracker?.name ?? "this tracker")")
                        .font(.system(size: 13)).foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity).padding(.top, 60)
                }
            }
        }
    }
}

/// The quiet line of a list with nothing in it, the same as a tracker tab's "No torrents on …".
struct EmptyListNote: View {
    static let downloads = "No downloads"
    static let sharing = "Nothing is being shared"
    static let noMatches = "No matches"
    let text: String

    var body: some View {
        Text(text)
            .font(.system(size: 13)).foregroundStyle(.secondary)
            .frame(maxWidth: .infinity).padding(.top, 60)
    }
}

struct AddSheet: View {
    let commands: Commands
    @Binding var isPresented: Bool
    @State private var text = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Add a Download").font(.headline)
            TextField("Magnet link or URL", text: $text).textFieldStyle(.roundedBorder).onSubmit(add)
            HStack {
                Button("Choose .torrent File…") { isPresented = false; DispatchQueue.main.async { commands.chooseTorrentFile() } }
                    .help("Pick a .torrent file from disk instead")
                Spacer()
                Button("Cancel", role: .cancel) { isPresented = false }.keyboardShortcut(.cancelAction)
                    .help("Close without adding")
                Button("Add", action: add).keyboardShortcut(.defaultAction).disabled(text.trimmingCharacters(in: .whitespaces).isEmpty)
                    .help("Add this magnet link or URL as a download")
            }
        }
        .padding(20)
        .frame(width: 440)
    }

    private func add() {
        let t = text.trimmingCharacters(in: .whitespaces)
        guard !t.isEmpty else { return }
        commands.add(t)
        isPresented = false
    }
}

/// The page title at the leading edge of the title bar: the section's name (a tracker's own name, with its
/// star when it is a favorite), and under it the section's calm summary line. On macOS 26+ toolbar items
/// share a glass capsule by default; the title is not a control, so it sits directly on the bar.
struct TitleToolbarItem: ToolbarContent {
    let title: String
    let summary: String
    var favorite = false

    var body: some ToolbarContent {
        if #available(macOS 26.0, *) {
            ToolbarItem(placement: .navigation) { label }.sharedBackgroundVisibility(.hidden)
        } else {
            ToolbarItem(placement: .navigation) { label }
        }
    }

    private var label: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 6) {
                if favorite { Image(systemName: "star.fill").font(.system(size: 12)).foregroundStyle(Color.accentColor.opacity(0.85)).accessibilityHidden(true) }
                Text(title).font(.system(size: 18, weight: .semibold)).lineLimit(1)
            }
            Text(summary).font(.system(size: 11.5)).foregroundStyle(.secondary).monospacedDigit().lineLimit(1)
        }
        .fixedSize()
        .accessibilityElement(children: .contain)
    }
}
