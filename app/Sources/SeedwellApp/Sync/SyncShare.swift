import AppKit
import SwiftUI

/// What is being shared: a synced folder or a channel (both have members, roles, links and a sharing period).
struct ShareTarget: Equatable {
    let id: String
    let name: String
    let role: SyncRole
    let shareUntilS: UInt64?
    let isChannel: Bool
    /// A folder's chat: the channel its links can invite to as well.
    let channelId: String?

    init(_ f: SyncFolder) { (id, name, role, shareUntilS, isChannel, channelId) = (f.id, f.name, f.role, f.shareUntilS, false, f.channelId) }
    init(_ c: SyncChannel) { (id, name, role, shareUntilS, isChannel, channelId) = (c.id, c.name, c.role, c.shareUntilS, true, nil) }
    /// A folder not synced yet (the system Share menu): this Mac will be its owner.
    init(newFolder path: String) {
        (id, name, role, shareUntilS, isChannel, channelId) = ("", (path as NSString).lastPathComponent, .owner, nil, false, nil)
    }
}

/// The Share dialog (SPEC 7G.1, 7G.5) of a folder or a channel: the settings of a link, and one Copy Link that makes
/// a link with them and copies it; the sharing period (an owner's setting, applied at once: when it ends, every
/// device but the owners' is removed); and the active links, each with Revoke. A link admits devices at its role
/// until it is used up, expires or is revoked; only the owner and admins may share.
/// Opened on a folder not synced yet (Share ▸ Seedwell in Finder), it is add and share in one: Copy Link starts
/// syncing the folder from this Mac first, as its owner.
@MainActor @Observable final class SyncShareModel {
    @ObservationIgnored let core: Core
    /// The folder or channel (empty while `newPath` is not synced yet).
    private(set) var id: String
    /// A folder Copy Link starts syncing first (`nil` once it is synced, or for a synced one).
    private(set) var newPath: String?
    /// The folder was added (by Copy Link): its id.
    @ObservationIgnored var onAdded: (String) -> Void = { _ in }
    /// The folder or channel (`nil` once it is gone from this Mac).
    private(set) var target: ShareTarget?
    var role: SyncRole = .readWrite
    /// One use (the default), else `uses` (0 = unlimited).
    var singleUse = true
    var uses: UInt32 = 5
    /// The link stops admitting after `linkDays` (on by default, 7 days).
    var linkExpires = true
    var linkDays: UInt32 = 7
    /// The days chosen for the folder's sharing period (shown while it has none; applied when switched on).
    var shareDays: UInt32 = 7
    /// A folder's link also invites to a channel (the same invite): its chat, a new channel named after the folder
    /// (`SyncShareModel.newChat`), or another channel this Mac may share. On when the folder has a chat already.
    var withChat = false
    var chat = SyncShareModel.newChat
    /// The channels a folder's link can invite to (this Mac may share them).
    private(set) var chatChoices: [SyncChannel] = []
    @ObservationIgnored private var chatChosen = false
    private(set) var invites: [SyncInvite] = []
    var error: String?
    var busy = false
    /// "Link copied", shown for a moment after Copy Link.
    var copied = false
    @ObservationIgnored private var copiedTask: Task<Void, Never>?

    static let newChat = ""
    static let useChoices = syncShareUseChoices()
    static let dayChoices = syncShareDayChoices()

    /// `copied`: show the "Link copied" state (snapshots).
    init(core: Core, id: String, copied: Bool = false) {
        self.core = core
        self.id = id
        self.copied = copied
        reload()
    }

    /// A folder not synced yet: shared from this Mac when the first link is made.
    init(core: Core, newFolder path: String) {
        self.core = core
        self.id = ""
        self.newPath = path
        reload()
    }

    func reload() {
        let (core, id) = (core, id)
        let channels = core.chatChannels()
        target = newPath.map { ShareTarget(newFolder: $0) } ?? core.syncSnapshot().folders.first { $0.id == id }.map(ShareTarget.init)
            ?? channels.first { $0.id == id }.map(ShareTarget.init)
        if let t = target, !t.isChannel {
            chatChoices = channels.filter { SyncText.canShare($0.role) }
            // The first look starts from the folder's chat; later ones keep what the user picked.
            if !chatChosen, let c = t.channelId, chatChoices.contains(where: { $0.id == c }) { chatChosen = true; withChat = true; chat = c }
        }
        guard newPath == nil else { invites = []; return }
        Task {
            let list = await Task.detached(priority: .userInitiated) { core.syncInvites(id: id) }.value
            invites = list
        }
    }

    /// Roles this device may hand out (the core's choice by this device's role).
    var roles: [SyncRole] { syncShareRoles(role: target?.role ?? .readOnly) }

    var canShare: Bool { target.map { SyncText.canShare($0.role) } ?? false }

    /// The name of the channel the next link invites to as well (`nil`: none).
    var chatName: String? {
        guard withChat, target?.isChannel == false else { return nil }
        return chat == Self.newChat ? target?.name : chatChoices.first { $0.id == chat }?.name
    }

    /// What the next link is: uses and days valid, 0 = no limit.
    var settings: SyncLinkLimits { syncLinkLimits(singleUse: singleUse, uses: uses, expires: linkExpires, linkDays: linkDays) }

    /// The folder is shared for a limited time (an owner set an end).
    var sharingEnds: Bool { target?.shareUntilS != nil }

    /// Starts a sharing period of `days` from now, or ends the period (`0`: shared with no end). Owners only.
    func setSharing(days: UInt32) {
        setSharing(until: syncSharingEnd(days: days, nowUnixS: UInt64(Date().timeIntervalSince1970)))
    }

    /// Adds `days` to the running period's end.
    func extendSharing(days: UInt32) {
        setSharing(until: syncSharingExtended(untilS: target?.shareUntilS, days: days, nowUnixS: UInt64(Date().timeIntervalSince1970)))
    }

    /// Sets the end of the folder's sharing (Unix seconds; `0` = no end). Applies at once, for every device.
    private func setSharing(until: UInt64) {
        let (core, id) = (core, id)
        Task {
            let r: Result<Void, Error> = await Task.detached(priority: .userInitiated) {
                Result { try core.syncSetShareUntil(id: id, untilS: until) }
            }.value
            if case .failure(let e) = r { error = SyncText.message(e) }
            reload()
        }
    }

    /// The links that still admit someone, newest first (as many as the dialog lists, and how many more there are).
    func active(now: UInt64) -> SyncActiveLinks { syncActiveLinks(invites: invites, nowUnixS: now) }

    /// Makes a link with the settings and copies it (a secret: shown nowhere, never logged).
    func copyLink(actions: SyncActions) {
        guard canShare, !busy else { return }
        busy = true
        error = nil
        let (core, id, role, s) = (core, id, role, settings)
        // With a chat: a new channel is made first (named after the folder), and the folder's chat is the one chosen.
        let name = chatName
        let isNew = chat == Self.newChat
        let newName: String? = isNew ? name : nil
        let existing: String? = name != nil && !isNew ? chat : nil
        let current = target?.channelId
        let path = newPath
        Task {
            // A folder not synced yet is added first; once it is, a failure after that leaves it synced (and shown so).
            let (added, r): (String?, Result<SyncInvite, Error>) = await Task.detached(priority: .userInitiated) {
                var added: String?
                let r = Result {
                    if let path { added = try core.syncAddFolder(path: path).id }
                    let fid = added ?? id
                    var channel = existing
                    if let newName { channel = try core.chatNew(name: newName).id }
                    if let channel, channel != current { try core.syncSetChannel(id: fid, channelId: channel) }
                    return try core.syncCreateInvite(id: fid, role: role, maxUses: s.uses, validDays: s.linkDays, channelId: channel)
                }
                return (added, r)
            }.value
            busy = false
            if let added {
                self.id = added
                newPath = nil
                onAdded(added)
                reload()
            }
            switch r {
            case .success(let i):
                actions.copy(i.link)
                copied = true
                copiedTask?.cancel()
                // The enclosing Task already holds `self`; this one keeps it 3 s at most.
                copiedTask = Task {
                    try? await Task.sleep(for: .seconds(3))
                    if !Task.isCancelled { self.copied = false }
                }
                reload()
            case .failure(let e):
                error = SyncText.message(e)
            }
        }
    }

    func revoke(_ i: SyncInvite) {
        let (core, id, inviteId) = (core, id, i.id)
        Task {
            let r: Result<Void, Error> = await Task.detached(priority: .userInitiated) {
                Result { try core.syncRevokeInvite(id: id, inviteId: inviteId) }
            }.value
            if case .failure(let e) = r { error = SyncText.message(e) }
            reload()
        }
    }
}

struct SyncShareView: View {
    @Bindable var model: SyncShareModel
    let actions: SyncActions
    var done: () -> Void = {}

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 3) {
                ModalCaption(text: "Folder Sync")
                Text("Share “\(model.target?.name ?? "Folder")”").font(.system(size: 17, weight: .semibold)).lineLimit(1).truncationMode(.middle)
            }
            if model.target == nil {
                Text("This is no longer on this Mac.").foregroundStyle(.secondary).padding(.top, 16)
            } else if !model.canShare {
                Text("Only the owner and admins can share it. Ask one of them for a link.")
                    .foregroundStyle(.secondary).padding(.top, 16).fixedSize(horizontal: false, vertical: true)
            } else {
                if let path = model.newPath {
                    Text(syncShareNewNote(pathShown: Notifier.abbreviate(path))).font(.system(size: 12)).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true).padding(.top, 6)
                        .accessibilityIdentifier("sync-share-new")
                }
                settings.padding(.top, 18)
                let s = model.settings
                Text(syncShareSummary(role: model.role, uses: s.uses, linkDays: s.linkDays, chat: model.chatName))
                    .font(.system(size: 12)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    .padding(.top, 14)
                    .accessibilityIdentifier("sync-share-summary")
                // The sharing period is the folder's: set once it is synced (after the first link).
                if model.newPath == nil {
                    Divider().padding(.top, 16)
                    sharing.padding(.top, 12)
                }
                let now = UInt64(Date().timeIntervalSince1970)
                let active = model.active(now: now)
                if !active.links.isEmpty { links(active).padding(.top, 20) }
            }
            if let e = model.error { ModalError(text: e).padding(.top, 14) }
            HStack(spacing: 8) {
                if model.copied {
                    Label("Link copied", systemImage: "checkmark").font(.system(size: 12)).foregroundStyle(.secondary)
                        .transition(.opacity)
                }
                if model.busy { ProgressView().controlSize(.small) }
                Spacer(minLength: 0)
                if model.newPath != nil {
                    Button("Cancel", action: done).keyboardShortcut(.cancelAction).help("Close; the folder is not synced")
                } else {
                    Button("Done", action: done).keyboardShortcut(.cancelAction).help("Close; links keep working until used up, expired or revoked")
                }
                if model.canShare {
                    Button("Copy Link") { model.copyLink(actions: actions) }
                        .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction).disabled(model.busy)
                        .help("Make a link with these settings and copy it")
                        .accessibilityIdentifier("sync-share-copy")
                }
            }
            .animation(.easeOut(duration: 0.15), value: model.copied)
            .padding(.top, 22)
        }
        .padding(22)
        .frame(width: 460)
        .background(.background)
        .font(.system(size: 13))
    }

    private var settings: some View {
        VStack(alignment: .leading, spacing: 12) {
            Picker("", selection: $model.role) {
                ForEach(model.roles, id: \.self) { Text(SyncText.role($0)).tag($0) }
            }
            .pickerStyle(.segmented).labelsHidden()
            .frame(maxWidth: .infinity)
            .help(syncRoleNote(role: model.role))
            .accessibilityIdentifier("sync-share-role")
            row("Single use", on: $model.singleUse, help: "The link stops working once one Mac has joined with it") {
                if !model.singleUse { menu($model.uses, SyncShareModel.useChoices) }
            }
            row("Link expires", on: $model.linkExpires, help: "How long the link can be used to join") {
                if model.linkExpires { menu($model.linkDays, SyncShareModel.dayChoices) }
            }
            if model.target?.isChannel == false {
                row("Include a chat", on: $model.withChat,
                    help: "The same link also joins a channel, for the people and agents of this folder") {
                    if model.withChat {
                        Picker("", selection: $model.chat) {
                            Text("New channel “\(model.target?.name ?? "")”").tag(SyncShareModel.newChat)
                            if !model.chatChoices.isEmpty { Divider() }
                            ForEach(model.chatChoices, id: \.id) { Text($0.name).tag($0.id) }
                        }
                        .labelsHidden().fixedSize().controlSize(.small)
                    }
                }
                .accessibilityIdentifier("sync-share-chat")
            }
        }
    }

    /// The folder's sharing period: an owner starts it (applied at once, for every device), then sees it count down
    /// and can extend it or lift the end; an admin sees the countdown.
    @ViewBuilder private var sharing: some View {
        let until = model.target?.shareUntilS
        let owner = model.target?.role == .owner
        if let until {
            // Ticks once a minute while the dialog is open.
            TimelineView(.periodic(from: .now, by: 60)) { context in
                HStack(spacing: 10) {
                    Image(systemName: "timer").foregroundStyle(.secondary)
                    Text(syncSharingCountdown(untilS: until, nowUnixS: UInt64(context.date.timeIntervalSince1970)))
                        .monospacedDigit()
                        .accessibilityIdentifier("sync-share-countdown")
                    Spacer()
                    if owner {
                        Menu("Extend") {
                            ForEach(syncSharingExtendChoices(), id: \.value) { d in
                                Button(d.label) { model.extendSharing(days: d.value) }
                            }
                            Divider()
                            Button("No End Date") { model.setSharing(days: 0) }
                        }
                        .menuStyle(.button).fixedSize().controlSize(.small)
                        .help("Share for longer, from the current end; or share with no end")
                        .accessibilityIdentifier("sync-share-extend")
                    }
                }
                .frame(height: 22)
            }
        } else if owner {
            row("Stop sharing after", on: Binding(get: { false }, set: { if $0 { model.setSharing(days: model.shareDays) } }),
                help: "When the time is up, every Mac but yours leaves the folder (each keeps the files it has), and links stop working") {
                menu($model.shareDays, SyncShareModel.dayChoices)
            }
            .accessibilityIdentifier("sync-share-until")
        }
        Text(syncSharingLine(untilDay: until.map { Fmt.day($0) }, owner: owner))
            .font(.system(size: 12)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
    }

    /// A setting: its name on the left; its value (when on) and its switch on the right, the switches in one column.
    private func row<V: View>(_ title: String, on: Binding<Bool>, help: String, @ViewBuilder value: () -> V) -> some View {
        HStack(spacing: 10) {
            Text(title)
            Spacer()
            value()
            Toggle(title, isOn: on).toggleStyle(.switch).controlSize(.small).labelsHidden()
        }
        .frame(height: 22)
        .help(help)
    }

    private func menu(_ value: Binding<UInt32>, _ choices: [SyncChoice]) -> some View {
        Picker("", selection: value) {
            ForEach(choices, id: \.value) { Text($0.label).tag($0.value) }
        }
        .labelsHidden().fixedSize().controlSize(.small)
    }

    private func links(_ active: SyncActiveLinks) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            ModalCaption(text: "Active links")
            ForEach(Array(active.links.enumerated()), id: \.element.id) { n, i in
                if n > 0 { Divider().opacity(0.6) }
                HStack(spacing: 8) {
                    VStack(alignment: .leading, spacing: 1) {
                        Text(syncInviteLine(invite: i, nowUnixS: UInt64(Date().timeIntervalSince1970)))
                        let detail = syncInviteDetail(invite: i, expiresDay: i.expiresS.map { Fmt.day($0) })
                        if !detail.isEmpty { Text(detail).font(.system(size: 11.5)).foregroundStyle(.secondary) }
                    }
                    Spacer()
                    Button("Revoke") { model.revoke(i) }.controlSize(.small).help("Stop this link from admitting anyone else")
                }
                .font(.system(size: 12.5))
                .padding(.vertical, 3)
            }
            if let more = active.more {
                Text(more).font(.system(size: 11.5)).foregroundStyle(.secondary)
            }
        }
    }
}
