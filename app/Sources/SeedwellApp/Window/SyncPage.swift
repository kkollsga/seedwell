import SwiftUI

/// Folder Sync in the main window (SPEC 7G.5): the folders on the left; the selected one's page on the right with its
/// header (name, path, status), the devices it syncs with, its conflict copies and the paths it cannot sync. Type and
/// spacing, no boxes, as the rest of the window. What it shows, every word and what each item does are the core's
/// `SyncPageModel` (seedwell-ffi `sync_page.rs`); this draws it and runs its commands.
struct SyncPage: View {
    @Bindable var model: LibraryModel
    let actions: SyncActions
    /// The question an item asks before its command runs, and for which folder.
    @State private var asking: Asking?

    struct Asking: Equatable {
        let question: SyncQuestion
        let command: SyncPageCommand
        let folder: String
    }

    var body: some View {
        let m = model.folderSyncPage(onDemandOff: { id in actions.fileProvider?.isOff(id) ?? false })
        Group {
            if let e = m.empty {
                VStack(spacing: 10) {
                    Text(e.text).font(Theme.body).foregroundStyle(.secondary)
                    if let add = e.add { Button(add.title) { actions.showAdd(nil) }.help(add.help) }
                }
                .frame(maxWidth: .infinity).padding(.top, 60)
                .frame(maxHeight: .infinity, alignment: .top)
            } else {
                HStack(spacing: 0) {
                    folderList(m)
                        .frame(width: 236)
                    Divider()
                    if let p = m.page {
                        ScrollView { SyncFolderPageView(page: p, run: { run($0, folder: p.folderId) }, ask: ask, trash: trashBinding) }
                            .frame(maxWidth: .infinity)
                    } else {
                        Spacer()
                    }
                }
            }
        }
        .confirmationDialog(asking?.question.title ?? "", isPresented: Binding(get: { asking != nil }, set: { if !$0 { asking = nil } }),
                            presenting: asking) { a in
            Button(a.question.confirmTitle, role: a.question.destructive ? .destructive : nil) { asking = nil; run(a.command, folder: a.folder) }
            Button(a.question.cancelTitle, role: .cancel) { asking = nil }
        } message: { a in
            Text(a.question.text)
        }
    }

    private var trashBinding: Binding<Bool> {
        Binding(get: { model.syncRemovalTrash }, set: { model.setSyncTrash($0) })
    }

    private func ask(_ q: SyncQuestion, _ c: SyncPageCommand, _ folder: String) { asking = Asking(question: q, command: c, folder: folder) }

    private func folderList(_ m: SyncPageModel) -> some View {
        VStack(spacing: 0) {
            ScrollView {
                LazyVStack(spacing: 2) {
                    ForEach(m.folders, id: \.id) { f in
                        SyncListRow(row: f)
                            .contentShape(Rectangle())
                            .onTapGesture { model.syncSelection = f.id }
                            .contextMenu {
                                SyncMenuEntries(entries: f.menu, run: { run($0, folder: f.id) }, ask: { q, c in ask(q, c, f.id) })
                            }
                            .accessibilityAddTraits(f.selected ? [.isSelected, .isButton] : .isButton)
                    }
                }
                .padding(.horizontal, 8).padding(.top, 10)
            }
            if let add = m.add {
                Divider()
                HStack {
                    Button { actions.showAdd(nil) } label: { Label(add.title, systemImage: "plus") }
                        .buttonStyle(.plain).foregroundStyle(.secondary).font(Theme.secondary)
                        .help(add.help)
                    Spacer()
                }
                .padding(.horizontal, 14).padding(.vertical, 9)
            }
        }
    }

    /// Runs a command of the page on folder `id` (the core's `SyncPageCommand`).
    private func run(_ c: SyncPageCommand, folder id: String) {
        guard let f = model.sync.folders.first(where: { $0.id == id }) else { return }
        switch c {
        case .reveal: actions.reveal(f)
        case .share: actions.showShare(id)
        case .openChat(let channel): actions.showChat(channel)
        case .edit: actions.showEdit(id)
        case .askRemove:
            model.syncSelection = id
            model.askSyncRemove(id)
        case .cancelRemove: model.cancelSyncRemove()
        case .remove:
            let trash = model.syncRemovalTrash
            model.cancelSyncRemove()
            actions.remove(f, trash: trash)
        case .addFolder: actions.showAdd(nil)
        case .allowOnDemand: FileProviderSettings.open()
        case .setRole(let device, let role):
            let name = model.syncMembers.first { $0.deviceId == device }?.name ?? ""
            actions.setRole(id, device: device, deviceName: name, role) { [model] in model.loadMembers() }
        case .removeDevice(let device):
            let name = model.syncMembers.first { $0.deviceId == device }?.name ?? f.gone.first { $0.deviceId == device }?.name ?? ""
            actions.removeDevice(id, name: f.name, device: device, deviceName: name) { [model] in model.loadMembers() }
        case .reshare: actions.reshare(f)
        case .revealConflict(let path): NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
        case .resolve(let copy, let keep):
            let original = model.syncConflicts.first { $0.copy == copy }?.original ?? copy
            actions.resolve(f, copy: copy, original: original, keep: keep) { [model] in model.loadConflicts() }
        }
    }
}

/// The entries of a menu of the page: items (checked, disabled, in the warning colour, asking first) and separators.
struct SyncMenuEntries: View {
    let entries: [SyncPageMenuEntry]
    let run: (SyncPageCommand) -> Void
    let ask: (SyncQuestion, SyncPageCommand) -> Void

    var body: some View {
        ForEach(Array(entries.enumerated()), id: \.offset) { _, e in
            switch e {
            case .separator: Divider()
            case .item(let title, let command, let checked, let enabled, let destructive, let question):
                Button(role: destructive ? .destructive : nil) {
                    if let question { ask(question, command) } else { run(command) }
                } label: {
                    if checked { Label(title, systemImage: "checkmark") } else { Text(title) }
                }
                .disabled(!enabled)
            }
        }
    }
}

/// A folder in the list: its status image, the name and the grey line.
struct SyncListRow: View {
    let row: SyncPageFolderRow

    var body: some View {
        HStack(spacing: 9) {
            SyncBadgeView(badge: row.badge)
            VStack(alignment: .leading, spacing: 1) {
                Text(row.title).font(Theme.body).lineLimit(1).truncationMode(.middle)
                Text(row.subtitle).font(Theme.caption).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8).padding(.vertical, 6)
        .background(RoundedRectangle(cornerRadius: 7).fill(row.selected ? Color.primary.opacity(0.10) : .clear))
        .accessibilityElement(children: .combine)
    }
}

/// The status image in SwiftUI: tick, ring, pause, warning, missing.
struct SyncBadgeView: View {
    let badge: SyncBadge
    var size: CGFloat = 16

    var body: some View {
        Group {
            switch badge {
            case .upToDate: Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
            case .progress(let f):
                ZStack {
                    Circle().stroke(Color.primary.opacity(0.2), lineWidth: 2)
                    Circle().trim(from: 0, to: max(0.04, f)).stroke(Color.accentColor, style: StrokeStyle(lineWidth: 2, lineCap: .round)).rotationEffect(.degrees(-90))
                }
                .padding(2)
            case .waiting: Image(systemName: "clock.fill").foregroundStyle(.secondary)
            case .paused: Image(systemName: "pause.circle.fill").foregroundStyle(.secondary)
            case .attention: Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
            case .missing: Image(systemName: "questionmark.folder.fill").foregroundStyle(.secondary)
            }
        }
        .font(.system(size: size - 2))
        .frame(width: size, height: size)
        .accessibilityLabel(badge.accessibilityWord)
    }
}

struct SyncFolderPageView: View {
    let page: SyncFolderPage
    let run: (SyncPageCommand) -> Void
    let ask: (SyncQuestion, SyncPageCommand, String) -> Void
    /// The Stop Syncing question's Trash option.
    @Binding var trash: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            if !page.gone.isEmpty || page.reshare != nil { away.padding(.top, 12) }
            if let r = page.removal { removeStrip(r).padding(.top, 12) }
            section(page.members.title) {
                if let e = page.members.empty { quiet(e) }
                ForEach(Array(page.members.rows.enumerated()), id: \.element.deviceId) { n, m in
                    if n > 0 { Divider().opacity(0.6) }
                    memberRow(m)
                }
            }
            if let c = page.conflicts {
                section(c.title) {
                    if let e = c.empty { quiet(e) }
                    ForEach(Array(c.rows.enumerated()), id: \.element.path) { n, row in
                        if n > 0 { Divider().opacity(0.6) }
                        conflictRow(row)
                    }
                }
            }
            if let issues = page.issues {
                section(issues.title) {
                    ForEach(Array(issues.rows.enumerated()), id: \.offset) { n, i in
                        if n > 0 { Divider().opacity(0.6) }
                        VStack(alignment: .leading, spacing: 2) {
                            if let path = i.path { Text(path).font(Theme.body).lineLimit(1).truncationMode(.middle) }
                            Text(i.reason).font(Theme.caption).foregroundStyle(.secondary)
                        }
                        .padding(.vertical, 6)
                    }
                }
            }
        }
        .padding(.horizontal, 26).padding(.vertical, 22)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// A button of the page: its command, or its question first.
    private func press(_ b: SyncPageButton) {
        if let q = b.question { ask(q, b.command, page.folderId) } else { run(b.command) }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Text(page.title).font(Theme.title).lineLimit(1).truncationMode(.middle)
                Spacer(minLength: 8)
                ForEach(page.buttons, id: \.title) { b in
                    Button(b.title) { press(b) }.help(b.help)
                }
            }
            HStack(spacing: 6) {
                Text(page.place).lineLimit(1).truncationMode(.middle)
                Button { press(page.reveal) } label: { Image(systemName: "magnifyingglass") }
                    .buttonStyle(.plain).foregroundStyle(.secondary).help(page.reveal.help)
                    .accessibilityLabel(page.reveal.accessibilityLabel ?? page.reveal.title)
            }
            .font(Theme.secondary).foregroundStyle(.secondary)
            HStack(spacing: 6) {
                SyncBadgeView(badge: page.badge, size: 14)
                Text(page.status).font(Theme.secondary)
            }
            .padding(.top, 2)
            if let e = page.error { Text(e).font(Theme.secondary).foregroundStyle(.red) }
            if let n = page.note {
                Text(n).font(Theme.secondary).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            // macOS keeps a new File Provider domain switched off until the user allows it.
            if let off = page.onDemandOff {
                HStack(spacing: 8) {
                    Text(off.text).font(Theme.secondary).foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true)
                    Spacer(minLength: 8)
                    if let b = off.button {
                        Button(b.title) { press(b) }.controlSize(.small).help(b.help)
                            .accessibilityIdentifier("sync-on-demand-allow")
                    }
                }
                .padding(.top, 2)
            }
        }
    }

    /// Devices gone from the folder (SPEC 7G.3): for an owner, each with *Remove…*; for a member whose owners and admins
    /// are all gone, *Share Again from This Mac…*. Each action asks first.
    private var away: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(page.gone, id: \.deviceId) { g in
                HStack(spacing: 10) {
                    Image(systemName: "moon.zzz").font(.system(size: Theme.Icon.small)).foregroundStyle(.secondary)
                    Text(g.line).font(Theme.secondary).foregroundStyle(.secondary)
                    Spacer(minLength: 8)
                    Button(g.button.title) { press(g.button) }.controlSize(.small).help(g.button.help)
                        .accessibilityLabel(g.button.accessibilityLabel ?? g.button.title)
                        .accessibilityIdentifier("sync-remove-gone")
                }
            }
            if let r = page.reshare {
                HStack(spacing: 10) {
                    Image(systemName: "person.crop.circle.badge.questionmark").font(.system(size: Theme.Icon.small)).foregroundStyle(.secondary)
                    Text(r.note).font(Theme.secondary).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    Spacer(minLength: 8)
                    Button(r.button.title) { press(r.button) }.controlSize(.small).help(r.button.help)
                        .accessibilityIdentifier("sync-reshare")
                }
            }
        }
        .padding(.horizontal, 12).padding(.vertical, 8)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.05)))
    }

    private func removeStrip(_ r: SyncMenuConfirm) -> some View {
        HStack(spacing: 10) {
            Image(systemName: r.trash ? "trash" : "minus.circle").font(.system(size: Theme.Icon.small)).foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 6) {
                Text(r.question).font(Theme.secondary).foregroundStyle(.secondary)
                Toggle(r.trashLabel, isOn: $trash).toggleStyle(.checkbox).font(Theme.secondary)
            }
            Spacer(minLength: 8)
            Button(r.cancelTitle) { run(.cancelRemove) }.controlSize(.small).keyboardShortcut(.cancelAction).help(r.cancelHelp)
            Button(role: .destructive) { run(.remove) } label: { Text(r.removeTitle).foregroundStyle(.red) }
                .controlSize(.small).help(r.removeHelp)
        }
        .padding(.horizontal, 12).padding(.vertical, 8)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.05)))
        .transition(.move(edge: .top).combined(with: .opacity))
    }

    private func section<C: View>(_ title: String, @ViewBuilder _ content: () -> C) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(Theme.secondary.weight(.semibold)).foregroundStyle(.secondary)
                .padding(.bottom, 2)
            content()
        }
        .padding(.top, 26)
    }

    private func quiet(_ t: String) -> some View {
        Text(t).font(Theme.body).foregroundStyle(.secondary).padding(.vertical, 4)
    }

    private static func symbol(_ r: SyncMemberReach) -> String {
        switch r {
        case .thisDevice: "laptopcomputer"
        case .localNetwork: "wifi"
        case .relay: "arrow.triangle.branch"
        case .internet: "globe"
        case .away: "moon.zzz"
        }
    }

    /// A member: how it is reached, its name, line and role; the owner's menu for the others.
    private func memberRow(_ m: SyncPageMember) -> some View {
        HStack(spacing: 10) {
            Image(systemName: Self.symbol(m.reach)).font(.system(size: Theme.Icon.regular)).foregroundStyle(.secondary).frame(width: 18)
            VStack(alignment: .leading, spacing: 1) {
                Text(m.name).font(Theme.body)
                if !m.line.isEmpty { Text(m.line).font(Theme.caption).foregroundStyle(.secondary) }
            }
            Spacer()
            if let t = m.transfer {
                Text(t).font(Theme.secondary).monospacedDigit().foregroundStyle(.secondary).help(m.transferHelp)
            }
            if let menu = m.roleMenu {
                Menu(menu.title) {
                    SyncMenuEntries(entries: menu.entries, run: run, ask: { q, c in ask(q, c, page.folderId) })
                }
                .menuStyle(.button).fixedSize().controlSize(.small)
                .help(menu.help)
                .accessibilityIdentifier("sync-member-role")
            } else {
                Text(m.role).font(Theme.secondary).foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 6)
        .accessibilityElement(children: .combine)
    }

    private func conflictRow(_ c: SyncPageConflict) -> some View {
        HStack(spacing: 10) {
            Image(systemName: "doc.on.doc").font(.system(size: Theme.Icon.regular)).foregroundStyle(.orange).frame(width: 18)
            VStack(alignment: .leading, spacing: 1) {
                Text(c.title).font(Theme.body).lineLimit(1).truncationMode(.middle)
                Text(c.line).font(Theme.caption).foregroundStyle(.secondary)
            }
            Spacer()
            Button(c.reveal.title) { press(c.reveal) }.controlSize(.small).help(c.reveal.help)
            Menu(c.keep.title) {
                SyncMenuEntries(entries: c.keep.entries, run: run, ask: { q, cmd in ask(q, cmd, page.folderId) })
            }
            .menuStyle(.button).fixedSize().controlSize(.small)
            .help(c.keep.help)
            .accessibilityIdentifier("sync-conflict-keep")
        }
        .padding(.vertical, 6)
    }
}
