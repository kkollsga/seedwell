import SwiftUI

/// A small quiet word in a capsule: a message's "agent" badge, the "agents" tag of a channel open to agents (SPEC 7G.7).
/// One look wherever it is drawn.
struct ChatTagCapsule: View {
    let text: String
    let help: String

    var body: some View {
        Text(text).font(.system(size: 10.5, weight: .medium))
            .padding(.horizontal, 5).padding(.vertical, 1)
            .background(Capsule().fill(Color.secondary.opacity(0.16)))
            .foregroundStyle(.secondary)
            .fixedSize()
            .help(help)
    }
}

/// A conversation in the sidebar, under Chats (SPEC 7G.7): a channel, one of its topics or a private conversation,
/// with its unread count, like a tracker row. A channel open to agents carries its tag after its name.
struct ChatSidebarRow: View {
    let icon: String
    let name: String
    let unread: UInt32
    var online: Bool?
    var tag: SidebarTag?

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: icon).frame(width: 20).foregroundStyle(.secondary)
            Text(name).lineLimit(1).fontWeight(unread > 0 ? .semibold : .regular)
            if let tag { ChatTagCapsule(text: tag.text, help: tag.help) }
            if let online {
                Circle().fill(online ? Color.green : Color.secondary.opacity(0.4)).frame(width: 6, height: 6)
                    .accessibilityLabel(online ? "Online" : "Offline")
            }
            Spacer(minLength: 4)
            if unread > 0 {
                Text("\(unread)").font(.system(size: 11, weight: .semibold)).monospacedDigit()
                    .padding(.horizontal, 6).padding(.vertical, 1)
                    .background(Capsule().fill(Color.accentColor)).foregroundStyle(.white)
            }
        }
        .accessibilityElement(children: .combine)
    }
}

/// The channel's controls in the window's toolbar, next to its name: Members, Share… and its settings menu.
struct ChatToolbarControls: View {
    @Bindable var model: LibraryModel
    let channel: SyncChannel
    let actions: ChatActions
    @State private var showMembers = false

    var body: some View {
        HStack(spacing: 8) {
            // A channel open to agents says so by its name, as in the sidebar.
            if let tag = ChatText.agentTag(channel) { ChatTagCapsule(text: tag.text, help: tag.help) }
            Button("Members") { model.loadChatMembers(); showMembers = true }
                .help("Who is in this channel")
                .popover(isPresented: $showMembers, arrowEdge: .bottom) {
                    ChatMembersView(model: model, channel: channel, actions: actions) { m in
                        showMembers = false
                        actions.showDirect(m.deviceId, name: ChatText.named(m.name, user: m.user))
                    }
                }
            if SyncText.canShare(channel.role) {
                Button("Share…") { actions.share(channel.id) }.help("Make a link that lets another Mac join this channel")
            }
            Menu {
                Button(channel.muted ? "Unmute" : "Mute") { actions.setMuted(channel, !channel.muted) }
                ChatAgentsMenuView(menu: ChatText.agentsMenu(channel)) { actions.setAgents(channel, $0) }
                let follow = actions.followLine(channel)
                if follow.shown {
                    Toggle(follow.menuTitle, isOn: Binding(get: { follow.on }, set: { actions.setFollow(channel, $0) }))
                        .disabled(!follow.enabled).help(follow.line)
                }
                Button("Wake Command…") { actions.askWakeCommand(channel) }.disabled(!channel.agents)
                Divider()
                Button("New Topic…") { actions.newTopic(channel) }.disabled(channel.role == .readOnly)
                Button("Rename…") { actions.rename(channel) }
                Divider()
                Button("Leave Channel…") { actions.leave(channel) }
            } label: { Image(systemName: "ellipsis.circle") }
                .menuIndicator(.hidden).fixedSize()
                .help("Mute, whether agents may use this channel, the wake command, a new topic, rename, leave")
                .accessibilityIdentifier("chat-channel-menu")
        }
    }
}

/// A channel's Agents: Yes/No submenu (the core's title, choices and note): one setting for the whole channel, which
/// its owners and admins change; for anyone else the choices are shown disabled, with the reason.
struct ChatAgentsMenuView: View {
    let menu: ChatAgentsMenu
    let set: (Bool) -> Void

    var body: some View {
        Menu(menu.title) {
            ForEach(menu.choices, id: \.open) { c in
                Button { set(c.open) } label: {
                    if c.checked { Label(c.title, systemImage: "checkmark") } else { Text(c.title) }
                }
                .disabled(!menu.canChange || c.checked)
            }
            Divider()
            Text(menu.note)
        }
    }
}

/// A private conversation's control in the toolbar: what agents on this Mac may do with private messages, and
/// deleting the conversation.
struct ChatDirectToolbarControls: View {
    let actions: ChatActions
    let device: String
    let name: String
    @State private var access: SyncAgentAccess = .off

    var body: some View {
        Menu {
            Menu("Agents on This Mac") {
                ForEach([SyncAgentAccess.off, .read, .readPost], id: \.self) { a in
                    Button { actions.setDirectAgents(a); access = a } label: {
                        if a == access { Label(ChatText.agents(a), systemImage: "checkmark") } else { Text(ChatText.agents(a)) }
                    }
                }
            }
            Divider()
            Button("Delete Conversation…") { actions.deleteDirect(device, name: name) }
        } label: { Image(systemName: "ellipsis.circle") }
            .menuIndicator(.hidden).fixedSize()
            .help("What agents on this Mac may do with private messages, and deleting this conversation")
            .accessibilityIdentifier("chat-direct-menu")
            .onAppear { access = actions.directAgents }
    }
}

/// One conversation's page (a channel's General or a topic, or a private one): the messages and the message field.
/// Its name and line are the window's title, its controls in the toolbar (`ChatToolbarControls`).
struct ChatConversationView: View {
    @Bindable var model: LibraryModel
    let place: LibraryModel.Section
    /// The field's placeholder names it: "Message Team", "Message Dinner", "Message Kari's iPhone".
    let name: String
    let canPost: Bool
    /// What shows instead of the message field when it cannot post.
    var cannotPost = "Read only: you see the messages but cannot post."
    var empty = "No messages yet."
    let actions: ChatActions
    @State private var draft = ""
    @State private var replyTo: SyncMessage?
    @FocusState private var typing: Bool
    /// Images and files waiting to be sent.
    @State private var images = ChatComposerImages()

    /// Where attachments go: a channel (and its topic), or `dm:<device>`.
    private var attachPlace: (place: String, topic: String?)? {
        switch place {
        case .chat(let id, let topic): return (id, topic)
        case .direct(let id): return ("dm:\(id)", nil)
        default: return nil
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            messages
            Divider()
            if canPost { composer.padding(12) } else {
                Text(cannotPost).font(.system(size: 12)).foregroundStyle(.secondary).padding(12)
            }
        }
        .chatImageInput(images, core: model.core, enabled: canPost && attachPlace != nil, focused: typing)
    }

    private var messages: some View {
        ScrollViewReader { scroll in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    // A tag note is a line on the message it is about, not a card.
                    let (ms, tags) = ChatMail.cards(model.chatMessages)
                    if ms.isEmpty {
                        Text(empty).font(.system(size: 13)).foregroundStyle(.secondary).padding(.top, 30)
                            .frame(maxWidth: .infinity).multilineTextAlignment(.center)
                    }
                    ForEach(Array(ms.enumerated()), id: \.element.id) { n, m in
                        ChatMessageRow(message: m, header: ChatText.startsRun(m, after: n > 0 ? ms[n - 1] : nil),
                                       reply: m.replyTo.flatMap { r in ms.first { $0.id == r } },
                                       tags: tags[String(m.id.prefix(8)).lowercased()] ?? [],
                                       onTag: canPost ? { set in tag(m, set) } : nil,
                                       onReply: canPost ? { replyTo = m; typing = true } : nil, core: model.core)
                            .id(m.id)
                    }
                }
                .padding(.horizontal, 22).padding(.vertical, 12)
            }
            .onChange(of: model.chatMessages.last?.id) { _, last in
                if let last { withAnimation(.easeOut(duration: 0.15)) { scroll.scrollTo(last, anchor: .bottom) } }
            }
            .onAppear { if let last = model.chatMessages.last?.id { scroll.scrollTo(last, anchor: .bottom) } }
        }
    }

    private var composer: some View {
        VStack(alignment: .leading, spacing: 6) {
            if let r = replyTo {
                HStack(spacing: 6) {
                    Image(systemName: "arrowshape.turn.up.left").font(.system(size: 11))
                    Text("Replying to \(ChatText.author(r)): \(ChatText.preview(r, limit: 60))").lineLimit(1)
                    Spacer()
                    Button { replyTo = nil } label: { Image(systemName: "xmark.circle.fill") }.buttonStyle(.plain)
                        .help("Don't reply to it")
                }
                .font(.system(size: 11.5)).foregroundStyle(.secondary)
            }
            if let a = attachPlace, !model.chatPending.isEmpty {
                ChatPendingSendsView(core: model.core, place: a.place, pending: model.chatPending) { [model] in model.loadChatMessages() }
            }
            if !images.items.isEmpty { ChatComposerChips(images: images, core: model.core) }
            HStack(alignment: .bottom, spacing: 8) {
                if attachPlace != nil { ChatAttachButton(images: images, core: model.core).padding(.bottom, 3) }
                TextField("Message \(name)", text: $draft, axis: .vertical)
                    .textFieldStyle(.roundedBorder).lineLimit(1...6)
                    .focused($typing)
                    .onKeyPress(.return, phases: .down) { press in
                        if press.modifiers.contains(.shift) || press.modifiers.contains(.option) { return .ignored }
                        send()
                        return .handled
                    }
                    .accessibilityIdentifier("chat-field")
                Button("Send", action: send).disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && images.items.isEmpty)
                    .help("Send (Return; Shift-Return for a new line)")
            }
        }
    }

    /// Your tags on `m` are now `set` (a note in its conversation; others keep theirs).
    private func tag(_ m: SyncMessage, _ set: [String]) {
        actions.send(to: place, chatTagNote(messageId: m.id, tags: set), replyTo: nil) { [model] sent in
            if sent { model.loadChatMessages() }
        }
    }

    private func send() {
        let (text, reply) = (draft, replyTo?.id)
        if let a = attachPlace, !images.items.isEmpty {
            draft = ""
            replyTo = nil
            images.send(core: model.core, place: a.place, topic: a.topic, text: text, replyTo: reply) { [model] sent in
                if sent { model.loadChatMessages() } else if draft.isEmpty { draft = text }
            }
            return
        }
        draft = ""
        replyTo = nil
        actions.send(to: place, text, replyTo: reply) { [model] sent in
            if sent { model.loadChatMessages() } else if draft.isEmpty { draft = text }
        }
    }
}

/// A message: the author (with an agent label) and time at the start of a run, a quoted reply, the text.
struct ChatMessageRow: View {
    let message: SyncMessage
    let header: Bool
    let reply: SyncMessage?
    /// Its tags, by owner: shown as a line under the text.
    var tags: [ChatTagOwner] = []
    /// Sets your tags on it (the whole set); `nil` where you cannot post.
    var onTag: (([String]) -> Void)? = nil
    let onReply: (() -> Void)?
    /// Draws its images (none without it: the snapshots).
    var core: Core? = nil
    @State private var hovered = false

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            if header {
                // A person: their name and device, or the device alone ("You" for this Mac). An agent: its name, a
                // badge, and the computer it runs on.
                HStack(spacing: 6) {
                    if let agent = message.agent {
                        Text(agent).font(.system(size: 12.5, weight: .semibold))
                        ChatTagCapsule(text: "agent", help: "Sent by an agent through Seedwell on \(ChatText.computer(message))")
                        Label(ChatText.computer(message), systemImage: "desktopcomputer").labelStyle(.titleAndIcon)
                            .font(.system(size: 11.5)).foregroundStyle(.secondary)
                    } else if !message.isMe, !message.authorUser.isEmpty {
                        Text(message.authorUser).font(.system(size: 12.5, weight: .semibold))
                        Label(ChatText.computer(message), systemImage: "desktopcomputer").labelStyle(.titleAndIcon)
                            .font(.system(size: 11.5)).foregroundStyle(.secondary)
                    } else {
                        Text(message.isMe ? "You" : ChatText.computer(message)).font(.system(size: 12.5, weight: .semibold))
                    }
                    Text(ChatText.time(message.atS)).font(.system(size: 11)).foregroundStyle(.secondary)
                }
                .padding(.bottom, 2)
            }
            if let reply {
                Text("↳ \(ChatText.author(reply)): \(ChatText.preview(reply, limit: 80))")
                    .font(.system(size: 11.5)).foregroundStyle(.secondary).lineLimit(1)
            }
            HStack(alignment: .top) {
                Group {
                    switch message.kind {
                    case .text:
                        if let mail = chatMail(text: message.body) {
                            // A mail between projects: its header as a line, then its text.
                            VStack(alignment: .leading, spacing: 6) {
                                ChatMailHeader(mail: mail)
                                if !mail.body.isEmpty { ChatMarkdownView(text: mail.body) }
                            }
                        } else {
                            ChatMarkdownView(text: message.body)
                        }
                    case .json: Text(message.body).font(.system(size: 12, design: .monospaced)).textSelection(.enabled)
                    case .other: Text(ChatText.preview(message)).font(.system(size: 13)).foregroundStyle(.secondary)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                if hovered, let onTag { tagMenu(onTag) }
                if hovered, let onReply {
                    Button(action: onReply) { Image(systemName: "arrowshape.turn.up.left") }
                        .buttonStyle(.plain).foregroundStyle(.secondary).help("Reply")
                }
            }
            if let core, !message.images.isEmpty { ChatImagesView(core: core, channelId: message.place, images: message.images) }
            if let core, !message.files.isEmpty { ChatFilesView(core: core, place: message.place, files: message.files) }
            if !tags.isEmpty { tagsLine }
        }
        // Each message is a card, so one ends visibly where the next begins; yours have the accent's tint.
        .padding(.horizontal, 12).padding(.vertical, 8)
        .background(RoundedRectangle(cornerRadius: 10, style: .continuous).fill(cardFill))
        .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).strokeBorder(Color.secondary.opacity(0.12)))
        .contentShape(Rectangle())
        .onHover { hovered = $0 }
        // A run's first card stands apart from the one before; the rest of the run sits close.
        .padding(.top, header ? 10 : 4)
    }

    /// Who tagged it what: "kglite: Seen, Backlog · You: Needs triage".
    private var tagsLine: some View {
        HStack(spacing: 10) {
            ForEach(tags, id: \.owner) { t in
                Label("\(t.owner): \(t.tags.map { chatTagLabel(tag: $0) }.joined(separator: ", "))", systemImage: t.tags == ["seen"] ? "eye" : "tag")
            }
        }
        .font(.system(size: 11)).foregroundStyle(.secondary)
        .padding(.top, 2)
    }

    /// Your tags: the usual ones to switch on and off, and clearing them.
    private func tagMenu(_ onTag: @escaping ([String]) -> Void) -> some View {
        let yours = tags.first { $0.isYou }?.tags ?? []
        return Menu {
            ForEach(chatKnownTags(), id: \.self) { t in
                Toggle(chatTagLabel(tag: t), isOn: Binding(get: { yours.contains(t) },
                                                        set: { on in onTag(on ? yours + [t] : yours.filter { $0 != t }) }))
            }
            if !yours.isEmpty {
                Divider()
                Button("Clear My Tags") { onTag([]) }
            }
        } label: { Image(systemName: "tag") }
            .menuStyle(.borderlessButton).menuIndicator(.hidden).fixedSize()
            .foregroundStyle(.secondary).help("Tag it (your tags; others see them under your name)")
    }

    private var cardFill: Color {
        let mine = message.isMe && message.agent == nil
        return mine ? Color.accentColor.opacity(hovered ? 0.16 : 0.11) : Color.secondary.opacity(hovered ? 0.11 : 0.07)
    }
}

/// The members popover: every device in the channel with its role; an owner's role menu for the others.
struct ChatMembersView: View {
    @Bindable var model: LibraryModel
    let channel: SyncChannel
    let actions: ChatActions
    /// Opens the private conversation with a member (and closes the popover).
    let message: (SyncMember) -> Void
    /// The gone device whose removal is being asked about.
    @State private var askRemoveGone: SyncGoneDevice?

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Members").font(.system(size: 12, weight: .semibold)).foregroundStyle(.secondary)
            ForEach(SyncText.ordered(model.chatMembers), id: \.deviceId) { m in
                HStack(spacing: 10) {
                    Circle().fill(m.online || m.isMe ? Color.green : Color.secondary.opacity(0.4)).frame(width: 7, height: 7)
                    Text(m.isMe ? "\(ChatText.named(m.name, user: m.user)) (this Mac)" : ChatText.named(m.name, user: m.user)).font(.system(size: 13))
                    Spacer(minLength: 12)
                    if channel.role == .owner, !m.isMe {
                        Menu(SyncText.role(m.role)) {
                            ForEach([SyncRole.owner, .admin, .readWrite, .readOnly], id: \.self) { r in
                                Button(SyncText.role(r)) { actions.sync.setRole(channel.id, m, r) { [model] in model.loadChatMembers() } }
                                    .disabled(r == m.role)
                            }
                            Divider()
                            Button("Remove from Channel") {
                                actions.sync.removeDevice(channel.id, name: channel.name, m) { [model] in model.loadChatMembers() }
                            }
                        }
                        .menuStyle(.button).fixedSize().controlSize(.small)
                    } else {
                        Text(SyncText.role(m.role)).font(.system(size: 12)).foregroundStyle(.secondary)
                    }
                    if !m.isMe {
                        Button { message(m) } label: { Image(systemName: "envelope") }
                            .buttonStyle(.borderless).help("Message \(ChatText.named(m.name, user: m.user)) privately")
                            .accessibilityLabel("Message Privately")
                    }
                }
            }
            // An owner's: the devices gone from the channel (SPEC 7G.3), each with *Remove…*, which asks first.
            if !channel.gone.isEmpty { Divider().padding(.vertical, 2) }
            ForEach(channel.gone, id: \.deviceId) { d in
                let p = SyncText.gonePrompt(d, name: channel.name, channel: true)
                HStack(spacing: 10) {
                    Text(p.line).font(.system(size: 12)).foregroundStyle(.secondary)
                    Spacer(minLength: 12)
                    Button(p.button) { askRemoveGone = d }.controlSize(.small).help(p.help).accessibilityLabel(p.help)
                }
            }
        }
        .padding(14)
        .frame(minWidth: 300)
        .confirmationDialog(askRemoveGone.map { SyncText.gonePrompt($0, name: channel.name, channel: true).title } ?? "",
                            isPresented: Binding(get: { askRemoveGone != nil }, set: { if !$0 { askRemoveGone = nil } })) {
            if let d = askRemoveGone {
                let p = SyncText.gonePrompt(d, name: channel.name, channel: true)
                Button(p.confirm, role: .destructive) {
                    actions.sync.removeGone(channel.id, name: channel.name, d) { [model] in model.loadChatMembers() }
                    askRemoveGone = nil
                }
            }
            Button("Cancel", role: .cancel) { askRemoveGone = nil }
        } message: {
            if let d = askRemoveGone { Text(SyncText.gonePrompt(d, name: channel.name, channel: true).text) }
        }
    }
}
