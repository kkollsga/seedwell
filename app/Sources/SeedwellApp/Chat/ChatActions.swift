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

    func showChannel(_ id: String) { show(.chat(id, topic: nil)) }

    /// Opens the private conversation with a device (from the members of a channel).
    func showDirect(_ device: String, name: String) {
        show(.direct(device))
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

    func setAgents(_ c: SyncChannel, _ access: SyncAgentAccess) { core.chatSetAgents(id: c.id, access: access) }

    /// What agents on this Mac may do with private messages (one setting for all of them).
    var directAgents: SyncAgentAccess { core.chatDirectAgents() }

    func setDirectAgents(_ access: SyncAgentAccess) { core.chatSetDirectAgents(access: access) }

    /// Asks for a topic's title and starts it in the channel; its page opens.
    func newTopic(_ c: SyncChannel) {
        let a = NSAlert()
        a.messageText = "New topic in “\(c.name)”"
        a.informativeText = "A topic keeps a conversation apart from the channel’s General. Everyone in the channel sees it."
        let field = NSTextField(string: "")
        field.placeholderString = "Dinner on Saturday"
        field.frame = NSRect(x: 0, y: 0, width: 320, height: 24)
        a.accessoryView = field
        a.window.initialFirstResponder = field
        a.addButton(withTitle: "Start")
        a.addButton(withTitle: "Cancel")
        NSApp.activate()
        guard a.runModal() == .alertFirstButtonReturn, let title = ChatText.topicTitle(field.stringValue) else { return }
        do { let t = try core.chatNewTopic(channelId: c.id, title: title); show(.chat(c.id, topic: t.id)) } catch {
            sync.alert("Couldn't start the topic", SyncText.message(error))
        }
    }

    /// Asks for the command that wakes an agent when a message arrives (empty: none). It gets the message as JSON on
    /// its standard input, never as arguments.
    func askWakeCommand(_ c: SyncChannel) {
        let a = NSAlert()
        a.messageText = "Wake an agent for “\(c.name)”"
        a.informativeText = "This command runs when a message from another device arrives, with the message as JSON on its standard input. Agents must be allowed to read the channel. Leave it empty for none."
        let field = NSTextField(string: c.wakeCommand ?? "")
        field.placeholderString = "~/bin/wake-agent"
        field.frame = NSRect(x: 0, y: 0, width: 320, height: 24)
        a.accessoryView = field
        a.addButton(withTitle: "Save")
        a.addButton(withTitle: "Cancel")
        NSApp.activate()
        guard a.runModal() == .alertFirstButtonReturn else { return }
        let cmd = field.stringValue.trimmingCharacters(in: .whitespaces)
        do { try core.chatSetWakeCommand(id: c.id, command: cmd.isEmpty ? nil : cmd) } catch {
            sync.alert("Couldn't set the command", SyncText.message(error))
        }
    }

    /// Leaves the channel on this Mac (asks first). The others keep it.
    func leave(_ c: SyncChannel) {
        let a = NSAlert()
        a.messageText = "Leave “\(c.name)”?"
        a.informativeText = c.role == .owner
            ? "Its messages are removed from this Mac. The other members keep the channel; make another device an owner first if it should keep one."
            : "Its messages are removed from this Mac. The other members keep the channel."
        a.addButton(withTitle: "Leave")
        a.addButton(withTitle: "Cancel")
        NSApp.activate()
        guard a.runModal() == .alertFirstButtonReturn else { return }
        do { try core.chatLeave(id: c.id) } catch { sync.alert("Couldn't leave “\(c.name)”", SyncText.message(error)) }
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
                note(.chat(c.id, topic: nil), general, ChatText.place(c, nil)) { self.core.chatMessages(id: c.id, topic: nil, before: nil, limit: 1).last }
                for t in topics {
                    note(.chat(c.id, topic: t.id), t.unread, ChatText.place(c, t)) {
                        self.core.chatMessages(id: c.id, topic: t.id, before: nil, limit: 1).last
                    }
                }
            }
        }
        for d in core.chatDirects() {
            note(.direct(d.deviceId), d.unread, d.name) { self.core.chatDirectMessages(deviceId: d.deviceId, before: nil, limit: 1).last }
        }
        seen = now
        for (place, title, last) in fresh where !isShowing(place) {
            guard let m = last(), !m.isMe else { continue }
            let n = ChatText.notification(title, m)
            Notifier.post(title: n.title, body: n.body, chat: place, key: "chat-\(place)")
        }
    }
}

/// The New Channel window: make a channel, or join one with a link.
@MainActor @Observable final class ChatNewModel {
    enum Mode: String, CaseIterable, Identifiable {
        case new = "New Channel", join = "Join with a Link"
        var id: String { rawValue }
    }

    @ObservationIgnored let core: Core
    var mode: Mode = .new
    var name = ""
    var link = ""
    private(set) var info: SyncLinkInfo?
    private(set) var problem: String?
    var error: String?
    var busy = false

    init(core: Core, prefill: String?) {
        self.core = core
        if let prefill { mode = .join; setLink(prefill) }
    }

    func setLink(_ text: String) {
        link = text
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        info = t.isEmpty ? nil : core.chatParseLink(text: t)
        problem = info == nil && !t.isEmpty ? core.syncLinkProblem(text: t) : nil
        error = nil
    }

    var linkLine: String { SyncText.linkLine(link, info: info, problem: problem) }

    var canStart: Bool {
        switch mode {
        case .new: ChatText.channelName(name) != nil
        case .join: info.map { !$0.expired && !$0.alreadySynced } ?? false
        }
    }

    func start(done: @escaping (SyncChannel, Mode) -> Void) {
        guard canStart, !busy else { return }
        busy = true
        error = nil
        let (core, mode, name, link) = (core, mode, ChatText.channelName(name) ?? "", link.trimmingCharacters(in: .whitespacesAndNewlines))
        Task {
            let r: Result<SyncChannel, Error> = await Task.detached(priority: .userInitiated) {
                Result { mode == .new ? try core.chatNew(name: name) : try core.chatJoin(link: link) }
            }.value
            busy = false
            switch r {
            case .success(let c): done(c, mode)
            case .failure(let e): error = SyncText.message(e)
            }
        }
    }
}

struct ChatNewView: View {
    @Bindable var model: ChatNewModel
    var cancel: () -> Void = {}
    var done: (SyncChannel, ChatNewModel.Mode) -> Void = { _, _ in }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 3) {
                ModalCaption(text: "Chats")
                Text(model.mode == .new ? "New Channel" : "Join a Channel").font(.system(size: 17, weight: .semibold))
            }
            Picker("", selection: $model.mode) {
                ForEach(ChatNewModel.Mode.allCases) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.segmented).labelsHidden().padding(.top, 16)
            Group {
                switch model.mode {
                case .new:
                    VStack(alignment: .leading, spacing: 8) {
                        HStack(spacing: 10) {
                            Text("Name").frame(width: 60, alignment: .leading)
                            TextField("Family, Studio, Agents…", text: $model.name).textFieldStyle(.roundedBorder)
                                .accessibilityIdentifier("chat-new-name")
                        }
                        Text("You become its owner. After Start, Share… makes a link for each Mac that should join.")
                            .font(.system(size: 12)).foregroundStyle(.secondary).padding(.leading, 70)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                case .join:
                    HStack(alignment: .firstTextBaseline, spacing: 10) {
                        Text("Link").frame(width: 60, alignment: .leading)
                        VStack(alignment: .leading, spacing: 5) {
                            TextField("seedwell://chat/…", text: Binding(get: { model.link }, set: { model.setLink($0) }))
                                .textFieldStyle(.roundedBorder)
                                .accessibilityIdentifier("chat-join-link")
                            if !model.linkLine.isEmpty {
                                Label(model.linkLine, systemImage: model.canStart ? "checkmark.circle.fill" : "xmark.circle")
                                    .font(.system(size: 12)).foregroundStyle(model.canStart ? Color.secondary : Color.orange)
                            }
                        }
                    }
                }
            }
            .padding(.top, 18)
            .font(.system(size: 13))
            if let e = model.error { ModalError(text: e).padding(.top, 14) }
            ModalButtons(primary: "Start", enabled: model.canStart, busy: model.busy,
                         primaryHelp: model.mode == .new ? "Make the channel" : "Join the channel",
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
        let pasted = NSPasteboard.general.string(forType: .string).flatMap { ChatText.looksLikeLink($0) ? $0 : nil }
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
