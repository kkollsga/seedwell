import AppKit
import SwiftUI

/// The Share dialog (SPEC 7G.1, 7G.5): the settings of a link, and one Copy Link that makes a link with them and
/// copies it; the folder's sharing period (an owner's setting, applied at once: when it ends, every device but the
/// owners' is removed); and the folder's active links, each with Revoke. A link admits devices at its role until it
/// is used up, expires or is revoked; only the folder's owner and admins may share.
@MainActor @Observable final class SyncShareModel {
    @ObservationIgnored let core: Core
    let id: String
    private(set) var folder: SyncFolder?
    var role: SyncRole = .readWrite
    /// One use (the default), else `uses` (0 = unlimited).
    var singleUse = true
    var uses: UInt32 = 5
    /// The link stops admitting after `linkDays` (on by default, 7 days).
    var linkExpires = true
    var linkDays: UInt32 = 7
    /// The days chosen for the folder's sharing period (shown while it has none; applied when switched on).
    var shareDays: UInt32 = 7
    private(set) var invites: [SyncInvite] = []
    var error: String?
    var busy = false
    /// "Link copied", shown for a moment after Copy Link.
    var copied = false
    @ObservationIgnored private var copiedTask: Task<Void, Never>?

    static let useChoices: [(UInt32, String)] = [(5, "5 times"), (10, "10 times"), (0, "Unlimited")]
    static let dayChoices: [(UInt32, String)] = [(1, "1 day"), (7, "7 days"), (30, "30 days")]

    /// `copied`: show the "Link copied" state (snapshots).
    init(core: Core, id: String, copied: Bool = false) {
        self.core = core
        self.id = id
        self.copied = copied
        reload()
    }

    func reload() {
        folder = core.syncSnapshot().folders.first { $0.id == id }
        let (core, id) = (core, id)
        Task {
            let list = await Task.detached(priority: .userInitiated) { core.syncInvites(id: id) }.value
            invites = list.sorted { $0.createdS > $1.createdS }
        }
    }

    /// Roles this device may hand out: an owner any of them but Owner (co-owners are made on the folder page),
    /// an admin Read & write and Read only.
    var roles: [SyncRole] { folder?.role == .owner ? [.readWrite, .readOnly, .admin] : [.readWrite, .readOnly] }

    var canShare: Bool { folder.map { SyncText.canShare($0.role) } ?? false }

    /// What the next link is: (uses, days valid), 0 = no limit.
    var settings: (uses: UInt32, linkDays: UInt32) { (singleUse ? 1 : uses, linkExpires ? linkDays : 0) }

    /// The folder is shared for a limited time (an owner set an end).
    var sharingEnds: Bool { folder?.shareUntilS != nil }

    /// Starts a sharing period of `days` from now, or ends the period (`0`: shared with no end). Owners only.
    func setSharing(days: UInt32) {
        setSharing(until: days == 0 ? 0 : UInt64(Date().timeIntervalSince1970) + UInt64(days) * 86_400)
    }

    /// Adds `days` to the running period's end.
    func extendSharing(days: UInt32) {
        guard let until = folder?.shareUntilS else { return setSharing(days: days) }
        setSharing(until: max(until, UInt64(Date().timeIntervalSince1970)) + UInt64(days) * 86_400)
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

    /// The links that still admit someone, newest first.
    func active(now: UInt64) -> [SyncInvite] { invites.filter { SyncText.inviteState($0, now: now) == .open } }

    /// Makes a link with the settings and copies it (a secret: shown nowhere, never logged).
    func copyLink(actions: SyncActions) {
        guard canShare, !busy else { return }
        busy = true
        error = nil
        let (core, id, role, s) = (core, id, role, settings)
        Task {
            let r: Result<SyncInvite, Error> = await Task.detached(priority: .userInitiated) {
                Result { try core.syncCreateInvite(id: id, role: role, maxUses: s.uses, validDays: s.linkDays) }
            }.value
            busy = false
            switch r {
            case .success(let i):
                actions.copy(i.link)
                copied = true
                copiedTask?.cancel()
                copiedTask = Task { [weak self] in
                    try? await Task.sleep(for: .seconds(3))
                    if !Task.isCancelled { self?.copied = false }
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
                Text("Share “\(model.folder?.name ?? "Folder")”").font(.system(size: 17, weight: .semibold)).lineLimit(1).truncationMode(.middle)
            }
            if model.folder == nil {
                Text("This folder is no longer synced on this Mac.").foregroundStyle(.secondary).padding(.top, 16)
            } else if !model.canShare {
                Text("Only the folder’s owner and admins can share it. Ask one of them for a link.")
                    .foregroundStyle(.secondary).padding(.top, 16).fixedSize(horizontal: false, vertical: true)
            } else {
                settings.padding(.top, 18)
                let s = model.settings
                Text(SyncText.shareSummary(role: model.role, uses: s.uses, linkDays: s.linkDays))
                    .font(.system(size: 12)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    .padding(.top, 14)
                    .accessibilityIdentifier("sync-share-summary")
                Divider().padding(.top, 16)
                sharing.padding(.top, 12)
                let now = UInt64(Date().timeIntervalSince1970)
                let active = model.active(now: now)
                if !active.isEmpty { links(active).padding(.top, 20) }
            }
            if let e = model.error { ModalError(text: e).padding(.top, 14) }
            HStack(spacing: 8) {
                if model.copied {
                    Label("Link copied", systemImage: "checkmark").font(.system(size: 12)).foregroundStyle(.secondary)
                        .transition(.opacity)
                }
                if model.busy { ProgressView().controlSize(.small) }
                Spacer(minLength: 0)
                Button("Done", action: done).keyboardShortcut(.cancelAction).help("Close; links keep working until used up, expired or revoked")
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
            .help(SyncText.roleNote(model.role))
            .accessibilityIdentifier("sync-share-role")
            row("Single use", on: $model.singleUse, help: "The link stops working once one Mac has joined with it") {
                if !model.singleUse { menu($model.uses, SyncShareModel.useChoices) }
            }
            row("Link expires", on: $model.linkExpires, help: "How long the link can be used to join") {
                if model.linkExpires { menu($model.linkDays, SyncShareModel.dayChoices) }
            }
        }
    }

    /// The folder's sharing period: an owner starts it (applied at once, for every device), then sees it count down
    /// and can extend it or lift the end; an admin sees the countdown.
    @ViewBuilder private var sharing: some View {
        let until = model.folder?.shareUntilS
        let owner = model.folder?.role == .owner
        if let until {
            // Ticks once a minute while the dialog is open.
            TimelineView(.periodic(from: .now, by: 60)) { context in
                HStack(spacing: 10) {
                    Image(systemName: "timer").foregroundStyle(.secondary)
                    Text(SyncText.countdown(until: until, now: UInt64(context.date.timeIntervalSince1970)))
                        .monospacedDigit()
                        .accessibilityIdentifier("sync-share-countdown")
                    Spacer()
                    if owner {
                        Menu("Extend") {
                            ForEach(SyncShareModel.dayChoices, id: \.0) { d in
                                Button("+ \(d.1)") { model.extendSharing(days: d.0) }
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
        Text(SyncText.sharingLine(until: until, owner: owner))
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

    private func menu(_ value: Binding<UInt32>, _ choices: [(UInt32, String)]) -> some View {
        Picker("", selection: value) {
            ForEach(choices, id: \.0) { Text($0.1).tag($0.0) }
        }
        .labelsHidden().fixedSize().controlSize(.small)
    }

    private func links(_ active: [SyncInvite]) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            ModalCaption(text: "Active links")
            ForEach(Array(active.prefix(6).enumerated()), id: \.element.id) { n, i in
                if n > 0 { Divider().opacity(0.6) }
                HStack(spacing: 8) {
                    VStack(alignment: .leading, spacing: 1) {
                        Text(SyncText.inviteLine(i, now: UInt64(Date().timeIntervalSince1970)))
                        let detail = SyncText.inviteDetail(i)
                        if !detail.isEmpty { Text(detail).font(.system(size: 11.5)).foregroundStyle(.secondary) }
                    }
                    Spacer()
                    Button("Revoke") { model.revoke(i) }.controlSize(.small).help("Stop this link from admitting anyone else")
                }
                .font(.system(size: 12.5))
                .padding(.vertical, 3)
            }
            if active.count > 6 {
                Text("and \(active.count - 6) more").font(.system(size: 11.5)).foregroundStyle(.secondary)
            }
        }
    }
}
