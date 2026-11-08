import AppKit
import SwiftUI

/// The Share dialog (SPEC 7G.1, 7G.5) of a folder or a channel: the settings of a link, and one Copy Link that makes
/// a link with them and copies it; the sharing period (an owner's setting, applied at once: when it ends, every
/// device but the owners' is removed); and the active links, each with Revoke. Opened on a folder not synced yet
/// (Share ▸ Seedwell in Finder), it is add and share in one: Copy Link starts syncing the folder from this Mac first.
/// The form, its rules and every word are the core's (`Core::sync_share_*`, seedwell-ffi `sync_share.rs`).
///
/// The core draws the dialog after each change only (the form, the links, a switch applied), and again as often as its
/// `refreshMs` asks while the dialog is open (the countdown, and what changed elsewhere: the folder removed on another
/// device, this computer's role, Read only or On all my devices switched), off the main thread; a drawing the same as the
/// last is not shown again. The view draws the last drawing and never asks the core itself, so a render (and SwiftUI
/// renders often) costs the core nothing. Opening, the folder's form is read off the main thread too; until the first
/// drawing is there the dialog says it is opening.
@MainActor @Observable final class SyncShareModel {
    @ObservationIgnored let core: Core
    /// What is chosen. Each change is drawn by the core (`window`).
    var form: SyncShareForm {
        didSet { if opened && form != oldValue { draw() } }
    }
    /// The folder was added (by Copy Link): its id.
    @ObservationIgnored var onAdded: (String) -> Void = { _ in }
    private(set) var invites: [SyncInvite] = []
    /// The dialog as the core drew it after the last change; `nil` while it opens.
    private(set) var window: SyncShareWindowModel?
    var error: String?
    var busy = false
    /// "Link copied", shown for a moment after Copy Link.
    var copied = false
    @ObservationIgnored private var copiedTask: Task<Void, Never>?
    /// The form is the folder's (read from the core), not the opening one: its changes are drawn.
    @ObservationIgnored private var opened = false
    /// The latest drawing asked for (an older one that comes back after it is dropped), and the countdown's next one.
    @ObservationIgnored private var drawing: Task<Void, Never>?
    @ObservationIgnored private var drawn = 0
    @ObservationIgnored private var tick: Task<Void, Never>?
    /// The links being read.
    @ObservationIgnored private var loading: Task<Void, Never>?
    @ObservationIgnored private var loads = 0
    /// How many times the core drew the dialog (tests: after changes only, never once per render), and how many of
    /// those drawings were new and shown (tests: a refresh that finds nothing changed shows nothing).
    @ObservationIgnored private(set) var draws = 0
    @ObservationIgnored private(set) var shown = 0
    /// The wait the core's `refreshMs` asks for (tests: shorter).
    @ObservationIgnored var refreshAfter: (UInt32) -> Duration = { .milliseconds(Int($0)) }
    /// The first real content was drawn in the view (the window centres itself again on it).
    @ObservationIgnored var onFirstContent: () -> Void = {}
    @ObservationIgnored private var contentShown = false

    /// `copied`: show the "Link copied" state (snapshots).
    init(core: Core, id: String, copied: Bool = false) {
        self.core = core
        form = syncShareFormOpening(id: id)
        self.copied = copied
        Task { [weak self] in
            let f = await Task.detached(priority: .userInitiated) { core.syncShareForm(id: id) }.value
            guard let self else { return }
            form = f
            opened = true
            draw()
            reload()
        }
    }

    /// A folder not synced yet: shared from this Mac when the first link is made.
    init(core: Core, newFolder path: String) {
        self.core = core
        form = core.syncShareFormNew(path: path)
        opened = true
        draw()
        reload()
    }

    /// The folder or channel (empty while it is not synced yet).
    var id: String { form.id }

    /// The view drew its first real content (not "opening"): told once.
    func firstContentShown() {
        guard !contentShown else { return }
        contentShown = true
        onFirstContent()
    }

    /// Once the dialog is drawn as it is now (the form read, the last change drawn), or after `limit` at most (the
    /// renderer, tests, and the window waiting a moment before it shows the opening state).
    func ready(within limit: Duration = .seconds(5)) async {
        let end = ContinuousClock.now + limit
        while ContinuousClock.now < end {
            if isReady { return }
            try? await Task.sleep(for: .milliseconds(5))
        }
    }

    /// The dialog is drawn as it is now: the form read, the links read, the last change drawn. (Tests check it after
    /// `ready(within:)`: on a busy machine its limit can pass first, with an older drawing shown.)
    var isReady: Bool { window != nil && opened && drawing == nil && loading == nil }

    /// The core draws the dialog as it is now, off the main thread; then, after `refreshMs`, reads the links and draws it
    /// again (the countdown, changes made elsewhere).
    func draw() {
        tick?.cancel()
        drawn += 1
        let (core, form, invites, n) = (core, form, invites, drawn)
        drawing = Task { [weak self] in
            let w = await Task.detached(priority: .userInitiated) {
                core.syncShareWindow(form: form, invites: invites, nowUnixS: UInt64(max(0, Date().timeIntervalSince1970)), clock: MacClock())
            }.value
            // A change since: its own drawing is on its way.
            guard let self, n == drawn else { return }
            draws += 1
            // The same as shown: nothing to draw again (a refresh mostly finds nothing changed).
            if w != window {
                window = w
                shown += 1
            }
            drawing = nil
            guard let ms = w.refreshMs else { return }
            let wait = refreshAfter(ms)
            tick = Task { [weak self] in
                try? await Task.sleep(for: wait)
                if !Task.isCancelled { self?.reload() }
            }
        }
    }

    func reload() {
        let (core, id) = (core, form.id)
        guard form.newPath == nil else { invites = []; return }
        loads += 1
        let n = loads
        loading = Task {
            let list = await Task.detached(priority: .userInitiated) { core.syncInvites(id: id) }.value
            // A later read is on its way (the countdown's, while the opening one was slow): this older list is not
            // drawn over it, and the dialog is not ready before it.
            guard n == loads else { return }
            if list != invites { invites = list }
            loading = nil
            draw()
        }
    }

    /// Starts a sharing period of `days` from now, or ends the period (`0`: shared with no end). Owners only.
    func setSharing(days: UInt32) {
        let until = syncSharingEnd(days: days, nowUnixS: UInt64(Date().timeIntervalSince1970))
        let (core, id) = (core, form.id)
        changeSharing { try await core.syncSetShareUntil(id: id, untilS: until) }
    }

    /// Adds `days` to the running period's end (the core reads it).
    func extendSharing(days: UInt32) {
        let (core, id) = (core, form.id)
        changeSharing { try await core.syncShareExtend(id: id, days: days, nowUnixS: UInt64(Date().timeIntervalSince1970)) }
    }

    /// Changes the sharing period (applied at once, for every device), then reads the links and draws it again.
    private func changeSharing(_ call: @escaping @Sendable () async throws -> Void) {
        Task {
            let r: Result<Void, Error> = await catching { try await call() }
            if case .failure(let e) = r { error = SyncText.message(e) }
            reload()
        }
    }

    /// "On all my devices" switched (`on`): applied at once, for the user's other devices.
    func setDevices(on: Bool) {
        apply { core, form in try await core.syncShareSetDevices(form: form, on: on) }
    }

    /// "Read only" switched (`on`, the owner's folder setting): applied at once; the next link's role follows it (the
    /// core's choice).
    func setReadOnly(on: Bool) {
        apply { core, form in try await core.syncShareSetReadOnly(form: form, on: on) }
    }

    /// Runs a switch's call off the main thread, then takes the form's next role or shows why it failed.
    private func apply(_ call: @escaping @Sendable (Core, SyncShareForm) async throws -> SyncShareForm) {
        guard !busy else { return }
        busy = true
        error = nil
        let (core, form) = (core, form)
        Task {
            let r: Result<SyncShareForm, Error> = await catching { try await call(core, form) }
            busy = false
            switch r {
            case .success(let f): self.form.role = f.role
            case .failure(let e): error = SyncText.message(e)
            }
            // The switch as it is now (applied, or not).
            draw()
        }
    }

    /// Makes a link with the settings and copies it (a secret: shown nowhere, never logged).
    func copyLink(actions: SyncActions) {
        guard window?.copyTitle != nil, !busy else { return }
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
                let ms = window?.copiedMs ?? 0
                // The enclosing Task already holds `self`; this one keeps it a few seconds at most.
                copiedTask = Task {
                    try? await Task.sleep(for: .milliseconds(Int(ms)))
                    if !Task.isCancelled { self.copied = false }
                }
                reload()
            case .failure(let e):
                // A folder not synced yet may be synced by now (the error says what failed after that).
                let path = form.newPath
                var added: SyncFolder?
                if wasNew { added = await Task.detached { core.syncSnapshot().folders.first { $0.path == path } }.value }
                if let f = added {
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
        // The core's last drawing (the model asks for the next after a change, and as often as the countdown needs).
        if let w = model.window {
            content(w)
                // Its window was made around "opening" and grows to this: it centres itself again.
                .onAppear { model.firstContentShown() }
        } else {
            // Opening: the folder's form and the first drawing are read off the main thread.
            ProgressView().controlSize(.small)
                .frame(width: 460, height: 160)
                .background(.background)
                .accessibilityIdentifier("sync-share-opening")
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
                if let devices = w.devices {
                    Divider().padding(.top, 16)
                    FollowToggle(line: devices, isOn: Binding(get: { devices.on }, set: { model.setDevices(on: $0) }))
                        .disabled(model.busy)
                        .padding(.top, 12)
                }
                if let readOnly = w.readOnly {
                    Divider().padding(.top, 16)
                    ReadOnlyToggle(line: readOnly, isOn: Binding(get: { readOnly.on }, set: { model.setReadOnly(on: $0) }))
                        .disabled(model.busy)
                        .padding(.top, 12)
                }
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
