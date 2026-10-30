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
        let sb = model.view.sidebar
        List(selection: Binding(get: { model.section }, set: { if let new = $0, new != model.section { model.select(new) } })) {
            ForEach(sb.places, id: \.place) { p in
                Label {
                    HStack(spacing: 6) {
                        Text(p.title)
                        // A new download arrived while the user was in Settings or editing: a quiet dot, not a switch.
                        if let flag = p.flag {
                            Circle().fill(Color.accentColor).frame(width: 6, height: 6)
                                .help(flag.help)
                                .accessibilityLabel(flag.accessibility)
                        }
                    }
                } icon: { Image(systemName: SidebarSymbols.symbol(p.icon)) }
                    .badge(Int(p.badge))
                    .help(p.help)
                    .tag(p.place)
            }
            // Channels (SPEC 7G.7): each is a tab of its own, like a tracker, its topics under it.
            Section(sb.chats.title) {
                ForEach(sb.chats.channels, id: \.item.place) { c in
                    DisclosureGroup(isExpanded: Binding(get: { c.open }, set: { model.setChannelOpen(c.item.place, $0) })) {
                        ForEach(c.topics, id: \.place) { t in
                            ChatSidebarRow(icon: SidebarSymbols.symbol(t.icon), name: t.title, unread: t.unread)
                                .tag(t.place)
                                .help(t.help)
                        }
                        if let add = c.newTopic {
                            Button { SidebarMenus.run(.newTopic, on: c.item.place, commands: commands) } label: {
                                Label(add.title, systemImage: "plus").foregroundStyle(.secondary)
                            }
                            .buttonStyle(.plain)
                            .help(add.help)
                        }
                    } label: {
                        // Open, the channel's row is its General; closed, it counts its topics too.
                        ChatSidebarRow(icon: SidebarSymbols.symbol(c.item.icon), name: c.item.title, unread: c.item.unread)
                            .tag(c.item.place)
                            .help(c.item.help)
                            .contextMenu { MenuEntriesView(entries: SidebarMenus.entries(for: c.item.place, model: model, commands: commands)) }
                    }
                }
                Button { commands.chat.showNew() } label: {
                    Label(sb.chats.add.title, systemImage: "plus").foregroundStyle(.secondary)
                }
                .buttonStyle(.plain)
                .help(sb.chats.add.help)
                .accessibilityIdentifier("chat-new-channel")
            }
            // Private conversations (end-to-end encrypted, carried by the channels' members).
            if let directs = sb.directs {
                Section(directs.title) {
                    ForEach(directs.items, id: \.place) { d in
                        ChatSidebarRow(icon: SidebarSymbols.symbol(d.icon), name: d.title, unread: d.unread, online: d.online)
                            .tag(d.place)
                            .help(d.help)
                    }
                }
            }
            if let trackers = sb.trackers {
                Section(trackers.title) {
                    ForEach(trackers.items, id: \.place) { t in
                        TrackerRow(item: t, editing: commands.favorites)
                            .tag(t.place)
                            .help(t.help)
                            .contextMenu { MenuEntriesView(entries: SidebarMenus.entries(for: t.place, model: model, commands: commands)) }
                    }
                }
            }
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom, spacing: 0) {
            let f = sb.footer
            HStack(spacing: 10) {
                // Only the live rates; the limits are in the tooltips.
                if let down = f.down {
                    Text(down.text)
                        .font(.system(size: 13, weight: .medium)).foregroundStyle(.primary)
                        .help(down.help)
                }
                if let up = f.up {
                    Text(up.text)
                        .font(.system(size: 13, weight: .medium)).foregroundStyle(.primary)
                        .help(up.help)
                }
                if let help = f.limitSpeed {
                    Image(systemName: "tortoise.fill").font(.system(size: 10)).foregroundStyle(.secondary)
                        .help(help)
                }
                // Quiet, not an alert: the engine holds its requests and checks again by itself.
                if let offline = f.offline {
                    Text(offline.text).help(offline.help)
                }
                Spacer()
                SidebarIconButton(symbol: f.settingsActive ? "gearshape.fill" : "gearshape", help: f.settingsHelp, label: f.settingsLabel,
                                  active: f.settingsActive) { commands.showSettings(nil) }
            }
            .font(.system(size: 11.5)).monospacedDigit()
            .lineLimit(1).minimumScaleFactor(0.7)
            .foregroundStyle(.secondary)
            .padding(.leading, 18).padding(.trailing, 10).padding(.vertical, 8)
        }
    }
}

/// The SF Symbols of the sidebar's items.
enum SidebarSymbols {
    static func symbol(_ icon: SidebarIcon) -> String {
        switch icon {
        case .downloads: "arrow.down.circle"
        case .sharing: "arrow.up.circle"
        case .folderSync: "arrow.triangle.2.circlepath"
        case .channel: "bubble.left.and.bubble.right"
        case .mutedChannel: "bell.slash"
        case .topic: "number"
        case .person: "person.crop.circle"
        case .favoriteTracker: "star.fill"
        case .publicTracker: "globe"
        case .tracker: "dot.radiowaves.left.and.right"
        }
    }
}

/// The context menus of the sidebar's items as the core lists them, run here (the window and QA both draw them).
@MainActor enum SidebarMenus {
    static func entries(for place: LibraryPlace, model: LibraryModel, commands: Commands) -> [MenuEntry] {
        model.library.sidebarMenu(place: place).map { e in
            switch e {
            case .separator:
                return .separator
            case let .item(title, command, destructive):
                return .item(title, role: destructive ? .destructive : .normal) { run(command, on: place, commands: commands) }
            }
        }
    }

    static func run(_ command: SidebarCommand, on place: LibraryPlace, commands: Commands) {
        switch place {
        case .tracker(let id):
            let fav = commands.favorites
            switch command {
            case .trackerInfo: fav.openInfo(id)
            case .renameTracker: fav.beginRename(id)
            case .removeFavorite: fav.askRemove(id)
            case .saveAsFavorite: fav.saveAsFavorite(id)
            default: break
            }
        case .chat(let id, _):
            // The channel as it is now (the menu may have been open a while).
            guard let c = commands.core.chatChannels().first(where: { $0.id == id }) else { return }
            let chat = commands.chat
            switch command {
            case .shareChannel: chat.share(c.id)
            case .newTopic: chat.newTopic(c)
            case .mute: chat.setMuted(c, true)
            case .unmute: chat.setMuted(c, false)
            case .renameChannel: chat.rename(c)
            case .leaveChannel: chat.leave(c)
            default: break
            }
        default:
            break
        }
    }
}

/// A borderless sidebar button: a secondary-colour symbol with a soft highlight under the pointer.
struct SidebarIconButton: View {
    let symbol: String
    let help: String
    /// What a screen reader says (the help without its shortcut).
    let label: String
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
        .accessibilityLabel(label)
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
                TitleToolbarItem(title: model.view.title, summary: model.view.summary, favorite: model.view.titleStar)
                ChatToolbarItem(model: model, actions: commands.chat)
                RemoveConfirmToolbarItem(model: model)
            }
            .sheet(isPresented: Binding(get: { model.view.addSheet != nil }, set: { if !$0 { model.closeAddSheet() } })) {
                if let sheet = model.addSheet {
                    AddSheet(commands: commands, sheet: sheet, text: Binding(get: { model.addSheet?.text ?? "" }, set: { model.setAddSheetText($0) }),
                             close: model.closeAddSheet)
                }
            }
            .dropDestination(for: URL.self) { urls, _ in commands.add(urls: urls); return true }
            .overlay(alignment: .bottom) { PlayNoticeView(notice: commands.playNotice) }
            .animation(.easeOut(duration: 0.15), value: commands.playNotice.text)
    }

    @ViewBuilder private var content: some View {
        let v = model.view
        switch v.page {
        case .settings:
            SettingsView(model: settings, nav: nav)
        case .folderSync:
            SyncPage(model: model, actions: commands.sync)
        case .missing:
            EmptyListNote(text: v.missing ?? "")
        case .conversation:
            if model.directID != nil {
                // Only while the two Macs are connected directly (a conversation just started from a channel's members
                // is not listed yet: the core says why if it cannot be written to).
                ChatConversationView(model: model, place: model.section, name: model.directName, canPost: model.direct?.online ?? true,
                                     cannotPost: model.core.chatDirectOfflineText(),
                                     empty: "No messages yet. Only you and \(model.directName) can read what you write here.", actions: commands.chat)
            } else if let c = model.chat {
                ChatConversationView(model: model, place: model.section, name: model.chatTopic?.title ?? c.name, canPost: c.role != .readOnly,
                                     actions: commands.chat)
            }
        case .trackerInfo:
            if let t = model.tracker { TrackerInfoPage(model: model, tracker: t, commands: commands) }
        case .list:
            if let list = v.list {
                RowList(model: model, commands: commands, list: list) {
                    if let t = model.tracker {
                        if let q = commands.favorites.removal, q.id == t.id {
                            FavoriteRemoveStrip(question: q, confirm: commands.favorites.confirmRemoval, cancel: commands.favorites.cancelRemoval)
                                .padding(.horizontal, 10).padding(.top, 8)
                        }
                        if let stats = model.trackerStats {
                            TrackerHeader(tracker: t, stats: stats, daily: model.daily, down: v.trackerRates?.down ?? 0, up: v.trackerRates?.up ?? 0)
                        }
                    }
                    if let empty = list.empty { EmptyListNote(text: empty) }
                }
            }
        }
    }
}

/// A channel's controls at the toolbar's trailing end (SPEC 7G.7), in the style of the other items there.
struct ChatToolbarItem: ToolbarContent {
    @Bindable var model: LibraryModel
    let actions: ChatActions

    var body: some ToolbarContent {
        if #available(macOS 26.0, *) {
            ToolbarItem(placement: .primaryAction) { controls }.sharedBackgroundVisibility(.hidden)
        } else {
            ToolbarItem(placement: .primaryAction) { controls }
        }
    }

    @ViewBuilder private var controls: some View {
        if let c = model.chat { ChatToolbarControls(model: model, channel: c, actions: actions) }
        else if let d = model.directID { ChatDirectToolbarControls(actions: actions, device: d, name: model.directName) }
    }
}

/// The quiet line of a list with nothing in it ("No downloads", "No torrents on …").
struct EmptyListNote: View {
    let text: String

    var body: some View {
        Text(text)
            .font(.system(size: 13)).foregroundStyle(.secondary)
            .frame(maxWidth: .infinity).padding(.top, 60)
    }
}

/// "Add a Download" (File › Add Magnet Link or URL…), as the core words it and accepts the link.
struct AddSheet: View {
    let commands: Commands
    let sheet: AddLinkSheet
    /// What was typed: the core keeps it while the sheet is open (a click in the notifier does not lose it).
    @Binding var text: String
    let close: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(sheet.title).font(.headline)
            TextField(sheet.placeholder, text: $text).textFieldStyle(.roundedBorder).onSubmit(add)
            HStack {
                Button(sheet.chooseFileTitle) { close(); DispatchQueue.main.async { commands.chooseTorrentFile() } }
                    .help(sheet.chooseFileHelp)
                Spacer()
                Button(sheet.cancelTitle, role: .cancel, action: close).keyboardShortcut(.cancelAction)
                    .help(sheet.cancelHelp)
                Button(sheet.confirmTitle, action: add).keyboardShortcut(.defaultAction).disabled(addLinkSource(text: text) == nil)
                    .help(sheet.confirmHelp)
            }
        }
        .padding(20)
        .frame(width: 440)
    }

    private func add() {
        guard let source = addLinkSource(text: text) else { return }
        commands.add(source)
        close()
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
