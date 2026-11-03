import AppKit
import SwiftUI

/// The Share dialog (SPEC 7G.1, 7G.5) of a folder or a channel: the settings of a link, and one Copy Link that makes
/// a link with them and copies it; the sharing period (an owner's setting, applied at once: when it ends, every
/// device but the owners' is removed); and the active links, each with Revoke. Opened on a folder not synced yet
/// (Share ▸ Seedwell in Finder), it is add and share in one: Copy Link starts syncing the folder from this Mac first.
/// The form, its rules and every word are the core's (`Core::sync_share_*`, trs-ffi `sync_share.rs`).
@MainActor @Observable final class SyncShareModel {
    @ObservationIgnored let core: Core
    var form: SyncShareForm
    /// The folder was added (by Copy Link): its id.
    @ObservationIgnored var onAdded: (String) -> Void = { _ in }
    private(set) var invites: [SyncInvite] = []
    var error: String?
    var busy = false
    /// "Link copied", shown for a moment after Copy Link.
    var copied = false
    @ObservationIgnored private var copiedTask: Task<Void, Never>?

    /// `copied`: show the "Link copied" state (snapshots).
    init(core: Core, id: String, copied: Bool = false) {
        self.core = core
        form = core.syncShareForm(id: id)
        self.copied = copied
        reload()
    }

    /// A folder not synced yet: shared from this Mac when the first link is made.
    init(core: Core, newFolder path: String) {
        self.core = core
        form = core.syncShareFormNew(path: path)
        reload()
    }

    /// The folder or channel (empty while it is not synced yet).
    var id: String { form.id }

    /// The dialog as the core draws it now.
    func window(now: Date = .now) -> SyncShareWindowModel {
        core.syncShareWindow(form: form, invites: invites, nowUnixS: UInt64(max(0, now.timeIntervalSince1970)), clock: MacClock())
    }

    func reload() {
        let (core, id) = (core, form.id)
        guard form.newPath == nil else { invites = []; return }
        Task {
            let list = await Task.detached(priority: .userInitiated) { core.syncInvites(id: id) }.value
            invites = list
        }
    }

    /// Starts a sharing period of `days` from now, or ends the period (`0`: shared with no end). Owners only.
    func setSharing(days: UInt32) {
        setSharing(until: syncSharingEnd(days: days, nowUnixS: UInt64(Date().timeIntervalSince1970)))
    }

    /// Adds `days` to the running period's end.
    func extendSharing(days: UInt32) {
        let until = core.syncSnapshot().folders.first { $0.id == form.id }?.shareUntilS ?? core.chatChannels().first { $0.id == form.id }?.shareUntilS
        setSharing(until: syncSharingExtended(untilS: until, days: days, nowUnixS: UInt64(Date().timeIntervalSince1970)))
    }

    /// Sets the end of the sharing (Unix seconds; `0` = no end). Applies at once, for every device.
    private func setSharing(until: UInt64) {
        let (core, id) = (core, form.id)
        Task {
            let r: Result<Void, Error> = await catching { try await core.syncSetShareUntil(id: id, untilS: until) }
            if case .failure(let e) = r { error = SyncText.message(e) }
            reload()
        }
    }

    /// Makes a link with the settings and copies it (a secret: shown nowhere, never logged).
    func copyLink(actions: SyncActions) {
        guard window().copyTitle != nil, !busy else { return }
        busy = true
        error = nil
        let (core, form) = (core, form)
        let wasNew = form.newPath != nil
        Task {
            let r: Result<SyncShareCopied, Error> = await catching { try await core.syncShareCopyLink(form: form) }
            busy = false
            switch r {
            case .success(let c):
                self.form = c.form
                if wasNew { onAdded(c.form.id) }
                actions.copy(c.invite.link)
                copied = true
                copiedTask?.cancel()
                let ms = window().copiedMs
                // The enclosing Task already holds `self`; this one keeps it a few seconds at most.
                copiedTask = Task {
                    try? await Task.sleep(for: .milliseconds(Int(ms)))
                    if !Task.isCancelled { self.copied = false }
                }
                reload()
            case .failure(let e):
                // A folder not synced yet may be synced by now (the error says what failed after that).
                if wasNew, let f = core.syncSnapshot().folders.first(where: { $0.path == form.newPath }) {
                    self.form.id = f.id
                    self.form.newPath = nil
                    onAdded(f.id)
                    reload()
                }
                error = SyncText.message(e)
            }
        }
    }

    func revoke(_ linkId: String) {
        let (core, id) = (core, form.id)
        Task {
            let r: Result<Void, Error> = await catching { try await core.syncRevokeInvite(id: id, inviteId: linkId) }
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
        // The countdown ticks while the dialog is open (as often as the core asks).
        TimelineView(.periodic(from: .now, by: 60)) { context in
            content(model.window(now: context.date))
        }
    }

    private func content(_ w: SyncShareWindowModel) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 3) {
                ModalCaption(text: w.caption)
                Text(w.title).font(Theme.title).lineLimit(1).truncationMode(.middle)
            }
            if let gone = w.gone {
                Text(gone).foregroundStyle(.secondary).padding(.top, 16)
            } else if let refusal = w.refusal {
                Text(refusal).foregroundStyle(.secondary).padding(.top, 16).fixedSize(horizontal: false, vertical: true)
            } else {
                if let note = w.newNote {
                    Text(note).font(Theme.secondary).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true).padding(.top, 6)
                        .accessibilityIdentifier("sync-share-new")
                }
                settings(w).padding(.top, 18)
                Text(w.summary)
                    .font(Theme.secondary).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    .padding(.top, 14)
                    .accessibilityIdentifier("sync-share-summary")
                if let s = w.sharing {
                    Divider().padding(.top, 16)
                    sharing(s).padding(.top, 12)
                }
                if !w.links.isEmpty { links(w).padding(.top, 20) }
            }
            if let e = model.error { ModalError(text: e).padding(.top, 14) }
            HStack(spacing: 8) {
                if model.copied {
                    Label(w.copiedTitle, systemImage: "checkmark").font(Theme.secondary).foregroundStyle(.secondary)
                        .transition(.opacity)
                }
                if model.busy { ProgressView().controlSize(.small) }
                Spacer(minLength: 0)
                Button(w.closeTitle, action: done).keyboardShortcut(.cancelAction).help(w.closeHelp)
                if let copy = w.copyTitle {
                    Button(copy) { model.copyLink(actions: actions) }
                        .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction).disabled(model.busy)
                        .help(w.copyHelp)
                        .accessibilityIdentifier("sync-share-copy")
                }
            }
            .animation(.easeOut(duration: 0.15), value: model.copied)
            .padding(.top, 22)
        }
        .padding(22)
        .frame(width: 460)
        .background(.background)
        .font(Theme.body)
    }

    private func settings(_ w: SyncShareWindowModel) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Picker("", selection: $model.form.role) {
                ForEach(w.roles, id: \.role) { Text($0.title).tag($0.role) }
            }
            .pickerStyle(.segmented).labelsHidden()
            .frame(maxWidth: .infinity)
            .help(w.roleHelp)
            .accessibilityIdentifier("sync-share-role")
            row(w.singleUse, on: $model.form.singleUse) { menu($model.form.uses, w.singleUse.choices) }
            row(w.linkExpires, on: $model.form.linkExpires) { menu($model.form.linkDays, w.linkExpires.choices) }
            if let chat = w.chat {
                row(label: chat.label, help: chat.help, on: $model.form.withChat) {
                    if chat.on {
                        Picker("", selection: $model.form.chat) {
                            ForEach(Array(chat.choices.enumerated()), id: \.element.id) { n, c in
                                if n == 1 { Divider() }
                                Text(c.title).tag(c.id)
                            }
                        }
                        .labelsHidden().fixedSize().controlSize(.small)
                    }
                }
                .accessibilityIdentifier("sync-share-chat")
            }
        }
    }

    /// The sharing period: an owner starts it (applied at once, for every device), then sees it count down and can
    /// extend it or lift the end; an admin sees the countdown.
    @ViewBuilder private func sharing(_ s: SyncShareSharing) -> some View {
        if let countdown = s.countdown {
            HStack(spacing: 10) {
                Image(systemName: "timer").foregroundStyle(.secondary)
                Text(countdown)
                    .monospacedDigit()
                    .accessibilityIdentifier("sync-share-countdown")
                Spacer()
                if let extend = s.extendTitle {
                    Menu(extend) {
                        ForEach(s.extendChoices, id: \.value) { d in
                            Button(d.label) { model.extendSharing(days: d.value) }
                        }
                        Divider()
                        Button(s.noEndTitle) { model.setSharing(days: 0) }
                    }
                    .menuStyle(.button).fixedSize().controlSize(.small)
                    .help(s.extendHelp)
                    .accessibilityIdentifier("sync-share-extend")
                }
            }
            .frame(height: 22)
        } else if let start = s.start {
            row(start, on: Binding(get: { false }, set: { if $0 { model.setSharing(days: model.form.shareDays) } })) {
                menu($model.form.shareDays, start.choices)
            }
            .accessibilityIdentifier("sync-share-until")
        }
        Text(s.line)
            .font(Theme.secondary).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
    }

    private func row<V: View>(_ s: SyncShareSetting, on: Binding<Bool>, @ViewBuilder value: () -> V) -> some View {
        row(label: s.label, help: s.help, on: on, value: value)
    }

    /// A setting: its name on the left; its value (when on) and its switch on the right, the switches in one column.
    private func row<V: View>(label: String, help: String, on: Binding<Bool>, @ViewBuilder value: () -> V) -> some View {
        HStack(spacing: 10) {
            Text(label)
            Spacer()
            value()
            Toggle(label, isOn: on).toggleStyle(.switch).controlSize(.small).labelsHidden()
        }
        .frame(height: 22)
        .help(help)
    }

    /// A setting's value menu, while the core gives it choices.
    @ViewBuilder private func menu(_ value: Binding<UInt32>, _ choices: [SyncChoice]) -> some View {
        if !choices.isEmpty {
            Picker("", selection: value) {
                ForEach(choices, id: \.value) { Text($0.label).tag($0.value) }
            }
            .labelsHidden().fixedSize().controlSize(.small)
        }
    }

    private func links(_ w: SyncShareWindowModel) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            ModalCaption(text: w.linksCaption)
            ForEach(Array(w.links.enumerated()), id: \.element.id) { n, l in
                if n > 0 { Divider().opacity(0.6) }
                HStack(spacing: 8) {
                    VStack(alignment: .leading, spacing: 1) {
                        Text(l.line)
                        if !l.detail.isEmpty { Text(l.detail).font(Theme.caption).foregroundStyle(.secondary) }
                    }
                    Spacer()
                    Button(w.revokeTitle) { model.revoke(l.id) }.controlSize(.small).help(w.revokeHelp)
                }
                .font(Theme.secondary)
                .padding(.vertical, 3)
            }
            if let more = w.linksMore {
                Text(more).font(Theme.caption).foregroundStyle(.secondary)
            }
        }
    }
}
