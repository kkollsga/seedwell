import AppKit
import SwiftUI

/// Every channel command (SPEC 7G.7), in one place: the Chats page, the New Channel window and notifications go through
/// here. Sharing, members and roles are folder sync's (`SyncActions`): a channel is shared like a folder.
@MainActor final class ChatActions {
    let core: Core
    let sync: SyncActions
    /// Wired by the app delegate: show a conversation's page (a channel's General or topic, or a private one), and
    /// whether one is on screen (no banner for it then).
    var show: (_ place: LibraryModel.Section) -> Void = { _ in }
    var isShowing: (_ place: LibraryModel.Section) -> Bool = { _ in false }
    /// A private conversation was started with a device (its name, for the page before the first message).
    var noteDirectName: (_ device: String, _ name: String) -> Void = { _, _ in }
    private var windows = ChatWindows()
    private var timer: Timer?
    /// The unread counts last seen, per conversation, for notifications (nil until the first look).
    private var seen: [LibraryModel.Section: UInt32]?

    init(core: Core, sync: SyncActions) {
        self.core = core
        self.sync = sync
    }

    nonisolated func run<T: Sendable>(_ work: @escaping @Sendable () throws -> T) async -> Result<T, Error> {
        await Task.detached(priority: .userInitiated) { Result { try work() } }.value
    }

    func showNew(prefill: String? = nil) { windows.showNew(prefill: prefill, actions: self) }

    func share(_ id: String) { sync.showShare(id) }

    func showChannel(_ id: String) { show(.chat(channelId: id, topicId: nil)) }

    /// Opens the private conversation with a device (from the members of a channel).
    func showDirect(_ device: String, name: String) {
        show(.direct(deviceId: device))
        noteDirectName(device, name)
    }

    /// Sends `body` to a channel's General or topic, or privately to a device (a reply to `replyTo`); errors in an
    /// alert. `done` gets whether it was sent.
    func send(to place: LibraryModel.Section, _ body: String, replyTo: String?, done: @escaping (Bool) -> Void) {
        let text = body.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return done(false) }
        Task {
            let core = self.core
            let r: Result<SyncMessage, Error> = switch place {
            case .chat(let id, let topic): await run { try core.chatSend(id: id, topic: topic, body: text, replyTo: replyTo) }
            case .direct(let device): await run { try core.chatSendDirect(deviceId: device, body: text, replyTo: replyTo) }
            default: .failure(CoreError.Failed(message: "Not a conversation."))
            }
            if case .failure(let e) = r { sync.alert("Couldn't send the message", SyncText.message(e)) }
            done((try? r.get()) != nil)
        }
    }

    func setMuted(_ c: SyncChannel, _ muted: Bool) { core.chatSetMuted(id: c.id, muted: muted) }

    /// The channel's "On all my devices" (the core's line; hidden while this Mac is not paired).
    func followLine(_ c: SyncChannel) -> FollowLine { core.syncFollowLine(id: c.id) }

    /// Puts the channel on all the user's devices, or stops that (nothing is deleted anywhere).
    func setFollow(_ c: SyncChannel, _ on: Bool) {
        do { try core.syncSetFollow(id: c.id, on: on) } catch {
            let a = NSAlert()
            a.messageText = SyncText.message(error)
            a.runModal()
        }
    }

    /// The channel's Agents: Yes/No, for every member device (owners and admins).
    func setAgents(_ c: SyncChannel, _ open: Bool) {
        do { try core.chatSetAgents(id: c.id, open: open) } catch {
            sync.alert(ChatText.agentsMenu(c).failureTitle, SyncText.message(error))
        }
    }

    /// What agents on this Mac may do with private messages (one setting for all of them).
    var directAgents: SyncAgentAccess { core.chatDirectAgents() }

    func setDirectAgents(_ access: SyncAgentAccess) { core.chatSetDirectAgents(access: access) }

    /// Draws one of the core's chat dialogs as an alert: its title, message and field, the action and Cancel at the
    /// bottom right. What the action runs with (`chatDialogCheck` of the field), or nil when it was cancelled.
    private func ask(_ d: ChatDialog) -> ChatDialogInput? {
        let a = NSAlert()
        a.messageText = d.title
        a.informativeText = d.message
        var field: NSTextField?
        if let f = d.field {
            let t = NSTextField(string: f.text)
            if !f.placeholder.isEmpty { t.placeholderString = f.placeholder }
            t.frame = NSRect(x: 0, y: 0, width: 320, height: 24)
            a.accessoryView = t
            a.window.initialFirstResponder = t
            field = t
        }
        a.addButton(withTitle: d.confirmTitle).hasDestructiveAction = d.destructive
        a.addButton(withTitle: d.cancelTitle)
        NSApp.activate()
        guard a.runModal() == .alertFirstButtonReturn else { return nil }
        return chatDialogCheck(dialog: d, input: field?.stringValue ?? "")
    }

    /// Asks for a topic's title and starts it in the channel; its page opens.
    func newTopic(_ c: SyncChannel) {
        let d = chatNewTopicDialog(channel: c)
        guard case .ready(let title?) = ask(d) else { return }
        do { let t = try core.chatNewTopic(channelId: c.id, title: title); show(.chat(channelId: c.id, topicId: t.id)) } catch {
            sync.alert(d.failureTitle, SyncText.message(error))
        }
    }

    /// Asks for the channel's new name (on this Mac only: each device names its channels, as folders).
    func rename(_ c: SyncChannel) {
        let d = chatRenameDialog(channel: c)
        guard case .ready(let name?) = ask(d) else { return }
        do { try core.chatRename(id: c.id, name: name) } catch { sync.alert(d.failureTitle, SyncText.message(error)) }
    }

    /// Asks for the command that wakes an agent when a message arrives (empty: none). It gets the message as JSON on
    /// its standard input, never as arguments.
    func askWakeCommand(_ c: SyncChannel) {
        let d = chatWakeCommandDialog(channel: c)
        guard case .ready(let cmd) = ask(d) else { return }
        do { try core.chatSetWakeCommand(id: c.id, command: cmd) } catch { sync.alert(d.failureTitle, SyncText.message(error)) }
    }

    /// Deletes the private conversation with a device on this Mac (asks first), and goes to Folder Sync. The other
    /// device keeps its copy.
    func deleteDirect(_ device: String, name: String) {
        let d = chatDeleteConversationDialog(name: name)
        guard case .ready = ask(d) else { return }
        do {
            try core.chatDeleteDirect(deviceId: device)
            show(.folderSync)
        } catch { sync.alert(d.failureTitle, SyncText.message(error)) }
    }

    /// Agents: Yes/No… of the sidebar's menu: asks, then switches the channel the other way (owners and admins).
    func askAgents(_ c: SyncChannel) {
        guard case .ready = ask(chatAgentsDialog(channel: c)) else { return }
        setAgents(c, !c.agents)
    }

    /// Leaves the channel on this Mac (asks first). The others keep it.
    func leave(_ c: SyncChannel) {
        let d = chatLeaveDialog(channel: c)
        guard case .ready = ask(d) else { return }
        do { try core.chatLeave(id: c.id) } catch { sync.alert(d.failureTitle, SyncText.message(error)) }
    }

    // MARK: Notifications

    /// Looks for new messages every two seconds, the window open or not, and posts a banner for each conversation
    /// (a channel's General or topic, or a private one) whose unread count went up: the newest message, when it is
    /// from another device, its channel not muted and the conversation not on screen.
    func start() {
        tick()
        timer = Timer.scheduledTimer(withTimeInterval: 2, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.tick() }
        }
    }

    private func tick() {
        var now: [LibraryModel.Section: UInt32] = [:]
        var fresh: [(LibraryModel.Section, String, () -> SyncMessage?)] = []
        let before = seen
        func note(_ place: LibraryModel.Section, _ unread: UInt32, _ title: String, _ last: @escaping () -> SyncMessage?) {
            now[place] = unread
            if let before, unread > (before[place] ?? 0) { fresh.append((place, title, last)) }
        }
        for c in core.chatChannels() {
            let topics = core.chatTopics(channelId: c.id)
            let general = ChatText.generalUnread(c, topics)
            if !c.muted {
                note(.chat(channelId: c.id, topicId: nil), general, ChatText.place(c, nil)) { self.core.chatMessages(id: c.id, topic: nil, before: nil, limit: 1).last }
                for t in topics {
                    note(.chat(channelId: c.id, topicId: t.id), t.unread, ChatText.place(c, t)) {
                        self.core.chatMessages(id: c.id, topic: t.id, before: nil, limit: 1).last
                    }
                }
            }
        }
        for d in core.chatDirects() {
            note(.direct(deviceId: d.deviceId), d.unread, d.name) { self.core.chatDirectMessages(deviceId: d.deviceId, before: nil, limit: 1).last }
        }
        seen = now
        for (place, title, last) in fresh where !isShowing(place) {
            guard let m = last(), !m.isMe else { continue }
            let n = ChatText.notification(title, m)
            Notifier.post(title: n.title, body: n.body, chat: place, key: "chat-\(place)")
        }
    }
}

/// The New Channel window: make a channel, or join one with a link. The core keeps the form's rules and words
/// (`ChatNewForm`, `chatNewWindow`); this only holds it and runs Start off the main thread.
@MainActor @Observable final class ChatNewModel {
    @ObservationIgnored let core: Core
    private(set) var form: ChatNewForm
    var error: String?
    var busy = false

    init(core: Core, prefill: String?) {
        self.core = core
        form = core.chatNewForm(prefill: prefill)
    }

    /// The window as the core draws it from the form.
    var window: ChatNewWindowModel { chatNewWindow(form: form) }

    var mode: ChatNewMode {
        get { form.mode }
        set { form.mode = newValue; error = nil }
    }
    var name: String {
        get { form.name }
        set { form.name = newValue }
    }
    var agents: Bool {
        get { form.agents }
        set { form.agents = newValue }
    }
    var allDevices: Bool {
        get { form.allDevices }
        set { form.allDevices = newValue }
    }

    func setLink(_ text: String) {
        form = core.chatNewSetLink(form: form, text: text)
        error = nil
    }

    func start(done: @escaping (SyncChannel, ChatNewMode) -> Void) {
        guard window.canStart, !busy else { return }
        busy = true
        error = nil
        let (core, form) = (core, form)
        Task {
            let r: Result<SyncChannel, Error> = await Task.detached(priority: .userInitiated) {
                Result { try core.chatNewStart(form: form) }
            }.value
            busy = false
            switch r {
            case .success(let c): done(c, form.mode)
            case .failure(let e): error = SyncText.message(e)
            }
        }
    }
}

struct ChatNewView: View {
    @Bindable var model: ChatNewModel
    var cancel: () -> Void = {}
    var done: (SyncChannel, ChatNewMode) -> Void = { _, _ in }

    var body: some View {
        let w = model.window
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 3) {
                ModalCaption(text: w.caption)
                Text(w.title).font(.system(size: 17, weight: .semibold))
            }
            Picker("", selection: $model.mode) {
                ForEach(w.modes, id: \.mode) { Text($0.title).tag($0.mode) }
            }
            .pickerStyle(.segmented).labelsHidden().padding(.top, 16)
            Group {
                switch model.mode {
                case .new:
                    VStack(alignment: .leading, spacing: 8) {
                        HStack(spacing: 10) {
                            Text(w.nameLabel).frame(width: 60, alignment: .leading)
                            TextField(w.namePlaceholder, text: $model.name).textFieldStyle(.roundedBorder)
                                .accessibilityIdentifier("chat-new-name")
                        }
                        Text(w.nameNote)
                            .font(.system(size: 12)).foregroundStyle(.secondary).padding(.leading, 70)
                            .fixedSize(horizontal: false, vertical: true)
                        HStack(spacing: 10) {
                            Text(w.agentsLabel).frame(width: 60, alignment: .leading)
                            Picker("", selection: $model.agents) {
                                ForEach(w.agentsChoices, id: \.open) { Text($0.title).tag($0.open) }
                            }
                            .pickerStyle(.segmented).labelsHidden().fixedSize()
                            .accessibilityIdentifier("chat-new-agents")
                        }
                        .padding(.top, 6)
                        Text(w.agentsNote)
                            .font(.system(size: 12)).foregroundStyle(.secondary).padding(.leading, 70)
                            .fixedSize(horizontal: false, vertical: true)
                        if let follow = w.follow {
                            FollowToggle(line: follow, isOn: $model.allDevices).padding(.top, 6).padding(.leading, 70)
                        }
                    }
                case .join:
                    HStack(alignment: .firstTextBaseline, spacing: 10) {
                        Text(w.linkLabel).frame(width: 60, alignment: .leading)
                        VStack(alignment: .leading, spacing: 5) {
                            TextField(w.linkPlaceholder, text: Binding(get: { model.form.link }, set: { model.setLink($0) }))
                                .textFieldStyle(.roundedBorder)
                                .accessibilityIdentifier("chat-join-link")
                            if !w.linkLine.isEmpty {
                                Label(w.linkLine, systemImage: w.linkOk ? "checkmark.circle.fill" : "xmark.circle")
                                    .font(.system(size: 12)).foregroundStyle(w.linkOk ? Color.secondary : Color.orange)
                            }
                        }
                    }
                }
            }
            .padding(.top, 18)
            .font(.system(size: 13))
            if let e = model.error { ModalError(text: e).padding(.top, 14) }
            ModalButtons(primary: w.confirmTitle, enabled: w.canStart, busy: model.busy,
                         primaryHelp: w.confirmHelp,
                         cancel: cancel, confirm: { model.start(done: done) })
                .padding(.top, 22)
        }
        .padding(22)
        .frame(width: 440)
        .background(.background)
    }
}

/// The New Channel window (one at a time).
@MainActor final class ChatWindows: NSObject, NSWindowDelegate {
    private var window: NSWindow?

    func showNew(prefill: String?, actions: ChatActions) {
        if let w = window { NSApp.activate(); w.makeKeyAndOrderFront(nil); return }
        let pasted = NSPasteboard.general.string(forType: .string).flatMap { chatLooksLikeLink(text: $0) ? $0 : nil }
        let model = ChatNewModel(core: actions.core, prefill: prefill ?? pasted)
        let view = ChatNewView(model: model, cancel: { [weak self] in self?.close() }, done: { [weak self] c, mode in
            self?.close()
            actions.showChannel(c.id)
            // A new channel: the next step is sharing it.
            if mode == .new { actions.share(c.id) }
        })
        let host = NSHostingController(rootView: view)
        host.sizingOptions = .preferredContentSize
        let w = NSWindow(contentViewController: host)
        w.styleMask = [.titled, .closable, .fullSizeContentView]
        w.titleVisibility = .hidden
        w.titlebarAppearsTransparent = true
        w.isReleasedWhenClosed = false
        w.delegate = self
        w.identifier = NSUserInterfaceItemIdentifier("chat-new")
        w.center()
        window = w
        NSApp.activate()
        w.makeKeyAndOrderFront(nil)
    }

    func close() {
        guard let w = window else { return }
        window = nil
        w.delegate = nil
        w.contentViewController = nil
        w.close()
    }

    func windowWillClose(_ notification: Notification) {
        guard let w = notification.object as? NSWindow, w === window else { return }
        window = nil
        w.delegate = nil
        DispatchQueue.main.async { w.contentViewController = nil }
    }
}
