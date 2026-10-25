import SwiftUI

/// Folder Sync in the main window (SPEC 7G.5): the folders on the left; the selected one's page on the right with its
/// header (name, path, status), the devices it syncs with, its conflict copies and the paths it cannot sync. Type and
/// spacing, no boxes, as the rest of the window.
struct SyncPage: View {
    @Bindable var model: LibraryModel
    let actions: SyncActions

    var body: some View {
        let s = model.sync
        if s.folders.isEmpty {
            VStack(spacing: 10) {
                Text(s.running || s.problem == nil ? "Keep folders the same on your devices" : (s.problem ?? ""))
                    .font(.system(size: 13)).foregroundStyle(.secondary)
                Button("Add Folder…") { actions.showAdd(nil) }.help("Sync a folder of this Mac, or one shared with you")
            }
            .frame(maxWidth: .infinity).padding(.top, 60)
            .frame(maxHeight: .infinity, alignment: .top)
        } else {
            HStack(spacing: 0) {
                folderList(s)
                    .frame(width: 236)
                Divider()
                if let f = model.syncFolder {
                    ScrollView { SyncFolderPage(model: model, folder: f, devices: s.devices.filter { $0.folders.contains(f.id) }, actions: actions) }
                        .frame(maxWidth: .infinity)
                } else {
                    Spacer()
                }
            }
        }
    }

    private func folderList(_ s: SyncSnapshot) -> some View {
        VStack(spacing: 0) {
            ScrollView {
                LazyVStack(spacing: 2) {
                    ForEach(s.folders, id: \.id) { f in
                        SyncListRow(folder: f, selected: model.syncSelection == f.id)
                            .contentShape(Rectangle())
                            .onTapGesture { model.syncSelection = f.id }
                            .contextMenu {
                                Button("Show in Finder") { actions.reveal(f) }
                                if SyncText.canShare(f.role) { Button("Share…") { actions.showShare(f.id) } }
                                if let c = f.channelId, model.chats.contains(where: { $0.id == c }) { Button("Open Chat") { actions.showChat(c) } }
                                Button("Edit…") { actions.showEdit(f.id) }
                                Divider()
                                Button("Stop Syncing…") { model.showSyncFolder(f.id, askRemove: true) }
                            }
                            .accessibilityAddTraits(model.syncSelection == f.id ? [.isSelected, .isButton] : .isButton)
                    }
                }
                .padding(.horizontal, 8).padding(.top, 10)
            }
            Divider()
            HStack {
                Button { actions.showAdd(nil) } label: { Label("Add Folder…", systemImage: "plus") }
                    .buttonStyle(.plain).foregroundStyle(.secondary).font(.system(size: 12.5))
                    .help("Sync a folder of this Mac, or one shared with you")
                Spacer()
            }
            .padding(.horizontal, 14).padding(.vertical, 9)
        }
    }
}

/// A folder in the list: its status image, the name and the grey line.
struct SyncListRow: View {
    let folder: SyncFolder
    let selected: Bool

    var body: some View {
        HStack(spacing: 9) {
            SyncBadgeView(badge: SyncText.badge(folder))
            VStack(alignment: .leading, spacing: 1) {
                Text(folder.name).font(.system(size: 13)).lineLimit(1).truncationMode(.middle)
                Text(SyncText.subtitle(folder)).font(.system(size: 11.5)).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8).padding(.vertical, 6)
        .background(RoundedRectangle(cornerRadius: 7).fill(selected ? Color.primary.opacity(0.10) : .clear))
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

struct SyncFolderPage: View {
    @Bindable var model: LibraryModel
    let folder: SyncFolder
    let devices: [SyncDevice]
    let actions: SyncActions
    /// The member whose removal is being asked about.
    @State private var askRemoveMember: SyncMember?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            if model.syncAskRemove == folder.id { removeStrip.padding(.top, 12) }
            section("Members") {
                if model.syncMembers.isEmpty {
                    quiet(folder.state == .waitingForAdmin ? "Waiting for the folder’s owner or an admin to let this Mac in" : "No members known yet")
                } else {
                    ForEach(Array(SyncText.ordered(model.syncMembers).enumerated()), id: \.element.deviceId) { n, m in
                        if n > 0 { Divider().opacity(0.6) }
                        memberRow(m, device: devices.first { $0.id == m.deviceId })
                    }
                }
            }
            if folder.conflicts > 0 || !model.syncConflicts.isEmpty {
                section("Conflicts") {
                    if model.syncConflicts.isEmpty { quiet(Fmt.count(folder.conflicts, "conflict copy")) }
                    ForEach(Array(model.syncConflicts.enumerated()), id: \.element.path) { n, c in
                        if n > 0 { Divider().opacity(0.6) }
                        conflictRow(c)
                    }
                }
            }
            if !folder.issues.isEmpty {
                section("Issues") {
                    ForEach(Array(folder.issues.enumerated()), id: \.offset) { n, i in
                        if n > 0 { Divider().opacity(0.6) }
                        VStack(alignment: .leading, spacing: 2) {
                            Text(i.path.replacingOccurrences(of: "\r", with: "␍")).font(.system(size: 13)).lineLimit(1).truncationMode(.middle)
                            Text(i.reason).font(.system(size: 11.5)).foregroundStyle(.secondary)
                        }
                        .padding(.vertical, 6)
                    }
                }
            }
        }
        .padding(.horizontal, 26).padding(.vertical, 22)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Text(folder.name).font(.system(size: 20, weight: .semibold)).lineLimit(1).truncationMode(.middle)
                Spacer(minLength: 8)
                if let c = folder.channelId, let channel = model.chats.first(where: { $0.id == c }) {
                    Button("Chat") { actions.showChat(c) }.help("Open “\(channel.name)”, this folder’s channel")
                }
                if SyncText.canShare(folder.role) {
                    Button("Share…") { actions.showShare(folder.id) }.help("Make a link that lets another Mac join this folder")
                }
                Button("Edit…") { actions.showEdit(folder.id) }.help("Rename or move the folder, options")
                Button("Stop Syncing…") { model.syncTrash = false; model.syncAskRemove = folder.id }
                    .help("Stop syncing this folder on this Mac; other devices keep their copy")
            }
            HStack(spacing: 6) {
                Text(Notifier.abbreviate(folder.path)).lineLimit(1).truncationMode(.middle)
                Button { actions.reveal(folder) } label: { Image(systemName: "magnifyingglass") }
                    .buttonStyle(.plain).foregroundStyle(.secondary).help("Show in Finder")
                    .accessibilityLabel("Show \(folder.name) in Finder")
            }
            .font(.system(size: 12.5)).foregroundStyle(.secondary)
            HStack(spacing: 6) {
                SyncBadgeView(badge: SyncText.badge(folder), size: 14)
                Text(statusLine).font(.system(size: 12.5))
            }
            .padding(.top, 2)
            if let e = folder.error, folder.state == .error { Text(e).font(.system(size: 12)).foregroundStyle(.red) }
            if folder.state == .missing {
                Text("The folder or its disk is not there. Nothing is deleted on other devices; syncing starts again when it is back.")
                    .font(.system(size: 12)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private var statusLine: String {
        var parts = [SyncText.subtitle(folder), SyncText.role(folder.role)]
        if let until = folder.shareUntilS { parts.append("shared until \(SyncText.day(until))") }
        if folder.files > 0 { parts.append("\(Fmt.count(folder.files, "item")) · \(Fmt.bytes(folder.bytes))") }
        if folder.state == .upToDate, let t = folder.lastSyncedS { parts.append("synced \(Fmt.ago(t))") }
        return parts.joined(separator: " · ")
    }

    private var removeStrip: some View {
        HStack(spacing: 10) {
            Image(systemName: model.syncTrash ? "trash" : "minus.circle").font(.system(size: 11)).foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 6) {
                Text(SyncRemoval.question(folder.name)).font(.system(size: 12)).foregroundStyle(.secondary)
                Toggle(SyncRemoval.trashLabel, isOn: $model.syncTrash).toggleStyle(.checkbox).font(.system(size: 12))
            }
            Spacer(minLength: 8)
            Button("Cancel") { model.syncAskRemove = nil }.controlSize(.small).keyboardShortcut(.cancelAction).help("Keep syncing this folder")
            Button(role: .destructive) {
                let trash = model.syncTrash
                model.syncAskRemove = nil
                actions.remove(folder, trash: trash)
            } label: { Text("Remove").foregroundStyle(.red) }
                .controlSize(.small).help(model.syncTrash ? "Stop syncing and move the folder to the Trash" : "Stop syncing; the files stay")
        }
        .padding(.horizontal, 12).padding(.vertical, 8)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.05)))
        .transition(.move(edge: .top).combined(with: .opacity))
    }

    private func section<C: View>(_ title: String, @ViewBuilder _ content: () -> C) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.system(size: 12, weight: .semibold)).foregroundStyle(.secondary)
                .padding(.bottom, 2)
            content()
        }
        .padding(.top, 26)
    }

    private func quiet(_ t: String) -> some View {
        Text(t).font(.system(size: 13)).foregroundStyle(.secondary).padding(.vertical, 4)
    }

    /// A member: how it is reached (when connected), its name, role and state; the owner's menu for the others.
    private func memberRow(_ m: SyncMember, device d: SyncDevice?) -> some View {
        HStack(spacing: 10) {
            Image(systemName: m.isMe ? "laptopcomputer" : d.map { $0.via == "LAN" ? "wifi" : $0.via == "Relay" ? "arrow.triangle.branch" : "globe" } ?? "moon.zzz")
                .font(.system(size: 12)).foregroundStyle(.secondary).frame(width: 18)
            VStack(alignment: .leading, spacing: 1) {
                Text(m.isMe ? "\(m.name.isEmpty ? "This Mac" : m.name) (this Mac)" : (m.name.isEmpty ? "Unnamed device" : m.name))
                    .font(.system(size: 13))
                let line = SyncText.memberLine(m, device: d)
                if !line.isEmpty { Text(line).font(.system(size: 11.5)).foregroundStyle(.secondary) }
            }
            Spacer()
            if let d {
                Text("↓ \(Fmt.bytes(d.rxBytes))   ↑ \(Fmt.bytes(d.txBytes))")
                    .font(.system(size: 12)).monospacedDigit().foregroundStyle(.secondary)
                    .help("Received from and sent to this device since it connected")
            }
            if folder.role == .owner, !m.isMe {
                Menu(SyncText.role(m.role)) {
                    ForEach([SyncRole.owner, .admin, .readWrite, .readOnly], id: \.self) { r in
                        Button { actions.setRole(folder.id, m, r) { [model] in model.loadMembers() } } label: {
                            if r == m.role { Label(SyncText.role(r), systemImage: "checkmark") } else { Text(SyncText.role(r)) }
                        }
                        .disabled(r == m.role)
                    }
                    Divider()
                    Button("Remove from Folder…") { askRemoveMember = m }
                }
                .menuStyle(.button).fixedSize().controlSize(.small)
                .help("Change what \(m.name) may do in this folder, or remove it")
                .accessibilityIdentifier("sync-member-role")
            } else {
                Text(SyncText.role(m.role)).font(.system(size: 12)).foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 6)
        .accessibilityElement(children: .combine)
        .confirmationDialog("Remove \(askRemoveMember?.name ?? "") from “\(folder.name)”?", isPresented: Binding(
            get: { askRemoveMember?.deviceId == m.deviceId }, set: { if !$0 { askRemoveMember = nil } })) {
            Button("Remove", role: .destructive) { actions.removeDevice(folder.id, name: folder.name, m) { [model] in model.loadMembers() }; askRemoveMember = nil }
            Button("Cancel", role: .cancel) { askRemoveMember = nil }
        } message: {
            Text("It stops syncing at once, on every device. It keeps the files it already has.")
        }
    }

    private func resolve(_ c: SyncConflict, _ keep: SyncKeep) {
        actions.resolve(folder, c, keep: keep) { [model] in model.loadConflicts() }
    }

    private func conflictRow(_ c: SyncConflict) -> some View {
        HStack(spacing: 10) {
            Image(systemName: "doc.on.doc").font(.system(size: 12)).foregroundStyle(.orange).frame(width: 18)
            VStack(alignment: .leading, spacing: 1) {
                Text(c.original).font(.system(size: 13)).lineLimit(1).truncationMode(.middle)
                Text("\(c.device)’s version · \(c.when)").font(.system(size: 11.5)).foregroundStyle(.secondary)
            }
            Spacer()
            Button("Show in Finder") { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: c.path)]) }
                .controlSize(.small).help("Reveal the conflict copy next to the file")
            Menu("Keep") {
                Button("Current Version") { resolve(c, .original) }
                Button(c.device.isEmpty ? "The Copy" : "\(c.device)’s Version") { resolve(c, .copy) }
                Button("Both") { resolve(c, .both) }
            }
            .menuStyle(.button).fixedSize().controlSize(.small)
            .help("Keep one version (the other goes to the folder’s archive on this Mac), or both as separate files")
            .accessibilityIdentifier("sync-conflict-keep")
        }
        .padding(.vertical, 6)
    }
}
