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

/// A menu of a conversation's page as the core lists it (the ⋯ menu, a card's tags, a member's role menu): items,
/// on/off items, submenus, notes and separators. `run` gets an item's command and the dialog it asks first.
struct ChatPageMenuItems: View {
    let entries: [ChatPageMenuEntry]
    let run: (ChatPageCommand, ChatDialog?) -> Void

    var body: some View {
        ForEach(Array(entries.enumerated()), id: \.offset) { _, e in entry(e) }
    }

    @ViewBuilder private func entry(_ e: ChatPageMenuEntry) -> some View {
        switch e {
        case .separator:
            Divider()
        case .note(let text):
            Text(text)
        case .submenu(let title, let entries):
            Menu(title) { ChatPageMenuItems(entries: entries, run: run) }
        case let .item(title, command, checked, toggle, enabled, destructive, help, dialog):
            if toggle {
                Toggle(title, isOn: Binding(get: { checked }, set: { _ in run(command, dialog) }))
                    .disabled(!enabled).modifier(HelpIfAny(text: help))
            } else {
                Button(role: destructive ? .destructive : nil) { run(command, dialog) } label: {
                    if checked { Label(title, systemImage: "checkmark") } else { Text(title) }
                }
                .disabled(!enabled).modifier(HelpIfAny(text: help))
            }
        }
    }
}

/// A tooltip when there is one.
struct HelpIfAny: ViewModifier {
    let text: String

    func body(content: Content) -> some View {
        if text.isEmpty { content } else { content.help(text) }
    }
}

/// A conversation's controls in the window's toolbar, next to its name (the core's `ChatPageToolbar`): a channel's
/// agents tag, Members, Share… and its ⋯ menu; a private conversation's ⋯ menu.
struct ChatToolbarControls: View {
    @Bindable var model: LibraryModel
    let page: ChatPageModel
    let actions: ChatActions
    @State private var showMembers = false

    var body: some View {
        let t = page.toolbar
        HStack(spacing: 8) {
            if let tag = t.tag { ChatTagCapsule(text: tag.text, help: tag.help) }
            if let m = t.members {
                Button(m.title) { run(m.command, nil); showMembers = true }
                    .help(m.help)
                    .popover(isPresented: $showMembers, arrowEdge: .bottom) {
                        if let panel = model.chatPage?.members { ChatMembersView(panel: panel) { run($0, nil) } }
                    }
            }
            if let s = t.share { Button(s.title) { run(s.command, nil) }.help(s.help) }
            Menu { ChatPageMenuItems(entries: t.menu.entries, run: run) } label: { Image(systemName: "ellipsis.circle") }
                .menuIndicator(.hidden).fixedSize()
                .help(t.menu.help)
                .accessibilityIdentifier(t.members == nil ? "chat-direct-menu" : "chat-channel-menu")
        }
    }

    private func run(_ command: ChatPageCommand, _ dialog: ChatDialog?) {
        if case .messagePrivately = command { showMembers = false }
        actions.run(command, dialog: dialog, model: model)
    }
}

/// One conversation's page (a channel's General or a topic, or a private one), as the core's `ChatPageModel` has it:
/// the messages and the composer. Its name and line are the window's title, its controls in the toolbar
/// (`ChatToolbarControls`). Here: drawing, the field's text, the reply being written and the attachments waiting.
struct ChatConversationView: View {
    @Bindable var model: LibraryModel
    let page: ChatPageModel
    let actions: ChatActions
    @State private var draft = ""
    /// The message being answered.
    @State private var replyTo: ChatMessageCard?
    @FocusState private var typing: Bool
    /// Images and files waiting to be sent.
    @State private var images = ChatComposerImages()

    var body: some View {
        VStack(spacing: 0) {
            messages
            Divider()
            if page.composer.canPost { composer.padding(12) } else {
                Text(page.composer.cannotPost ?? "").font(.system(size: 12)).foregroundStyle(.secondary).padding(12)
            }
        }
        .chatImageInput(images, core: model.core, enabled: page.composer.attach, focused: typing)
    }

    private var messages: some View {
        ScrollViewReader { scroll in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    if let empty = page.empty {
                        Text(empty).font(.system(size: 13)).foregroundStyle(.secondary).padding(.top, 30)
                            .frame(maxWidth: .infinity).multilineTextAlignment(.center)
                    }
                    ForEach(page.messages, id: \.id) { card in
                        ChatMessageRow(card: card, core: model.core) { run($0, card) }
                            .id(card.id)
                    }
                }
                .padding(.horizontal, 22).padding(.vertical, 12)
            }
            .onChange(of: page.messages.last?.id) { _, last in
                if let last { withAnimation(.easeOut(duration: 0.15)) { scroll.scrollTo(last, anchor: .bottom) } }
            }
            .onAppear { if let last = page.messages.last?.id { scroll.scrollTo(last, anchor: .bottom) } }
        }
    }

    private var composer: some View {
        let c = page.composer
        return VStack(alignment: .leading, spacing: 6) {
            if let r = replyTo {
                HStack(spacing: 6) {
                    Image(systemName: "arrowshape.turn.up.left").font(.system(size: 11))
                    Text(r.replying).lineLimit(1)
                    Spacer()
                    Button { replyTo = nil } label: { Image(systemName: "xmark.circle.fill") }.buttonStyle(.plain)
                        .help(c.replyCancelHelp)
                }
                .font(.system(size: 11.5)).foregroundStyle(.secondary)
            }
            if !c.pending.isEmpty {
                ChatPendingSendsView(core: model.core, place: page.place, pending: c.pending) { [model] in model.loadChatMessages() }
            }
            if !images.items.isEmpty { ChatComposerChips(images: images, core: model.core) }
            HStack(alignment: .bottom, spacing: 8) {
                if c.attach { ChatAttachButton(images: images, core: model.core).padding(.bottom, 3) }
                TextField(c.placeholder, text: $draft, axis: .vertical)
                    .textFieldStyle(.roundedBorder).lineLimit(1...6)
                    .focused($typing)
                    .onKeyPress(.return, phases: .down) { press in
                        let key = chatComposerKey(shift: press.modifiers.contains(.shift), alt: press.modifiers.contains(.option))
                        guard key == .send else { return .ignored }
                        send()
                        return .handled
                    }
                    .accessibilityIdentifier("chat-field")
                Button(c.sendTitle, action: send)
                    .disabled(!chatComposerCanSend(draft: draft, attachments: UInt32(images.items.count)))
                    .help(c.sendHelp)
            }
        }
    }

    /// A card's command: Reply is the composer's; the rest are the page's.
    private func run(_ command: ChatPageCommand, _ card: ChatMessageCard) {
        if case .reply = command {
            replyTo = card
            typing = true
            return
        }
        actions.run(command, dialog: nil, model: model)
    }

    private func send() {
        let (text, reply) = (draft, replyTo?.id)
        if !images.items.isEmpty {
            draft = ""
            replyTo = nil
            images.send(core: model.core, place: page.place, topic: page.topic, text: text, replyTo: reply) { [model] sent in
                if sent { model.loadChatMessages() } else if draft.isEmpty { draft = text }
            }
            return
        }
        draft = ""
        replyTo = nil
        actions.send(to: model.section, text, replyTo: reply) { [model] sent in
            if sent { model.loadChatMessages() } else if draft.isEmpty { draft = text }
        }
    }
}

/// A message's card as the core lays it out (`ChatMessageCard`): the author and time at the start of a run, a quoted
/// reply, the text (Markdown, a mail, JSON), its images and files, and its tags; Tag and Reply under the pointer.
struct ChatMessageRow: View {
    let card: ChatMessageCard
    /// Draws its images and files (none without it).
    var core: Core? = nil
    /// Runs a command of the card (Reply, a tag).
    var run: (ChatPageCommand) -> Void = { _ in }
    @State private var hovered = false

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            if let a = card.author {
                HStack(spacing: 6) {
                    Text(a.name).font(.system(size: 12.5, weight: .semibold))
                    if let badge = a.badge { ChatTagCapsule(text: badge.text, help: badge.help) }
                    if let computer = a.computer {
                        Label(computer, systemImage: "desktopcomputer").labelStyle(.titleAndIcon)
                            .font(.system(size: 11.5)).foregroundStyle(.secondary)
                    }
                    Text(a.time).font(.system(size: 11)).foregroundStyle(.secondary)
                }
                .padding(.bottom, 2)
            }
            if let quote = card.replyQuote {
                Text(quote).font(.system(size: 11.5)).foregroundStyle(.secondary).lineLimit(1)
            }
            HStack(alignment: .top) {
                Group {
                    switch card.body {
                    case .markdown(let blocks):
                        ChatMarkdownView(blocks: blocks)
                    case .mail(let banner, let blocks):
                        // A mail between projects: its header as a line, then its text.
                        VStack(alignment: .leading, spacing: 6) {
                            ChatMailBannerView(banner: banner)
                            if !blocks.isEmpty { ChatMarkdownView(blocks: blocks) }
                        }
                    case .json(let text):
                        Text(text).font(.system(size: 12, design: .monospaced)).textSelection(.enabled)
                    case .unsupported(let text):
                        Text(text).font(.system(size: 13)).foregroundStyle(.secondary)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                if hovered, let menu = card.tagMenu { tagMenu(menu) }
                if hovered, let reply = card.reply {
                    Button { run(reply.command) } label: { Image(systemName: "arrowshape.turn.up.left") }
                        .buttonStyle(.plain).foregroundStyle(.secondary).help(reply.help)
                }
            }
            if let core, !card.images.isEmpty { ChatImagesView(core: core, channelId: card.place, images: card.images) }
            if let core, !card.files.isEmpty { ChatFilesView(core: core, place: card.place, files: card.files) }
            if !card.tags.isEmpty { tagsLine }
        }
        // Each message is a card, so one ends visibly where the next begins; yours have the accent's tint.
        .padding(.horizontal, 12).padding(.vertical, 8)
        .background(RoundedRectangle(cornerRadius: 10, style: .continuous).fill(cardFill))
        .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).strokeBorder(Color.secondary.opacity(0.12)))
        .contentShape(Rectangle())
        .onHover { hovered = $0 }
        // A run's first card stands apart from the one before; the rest of the run sits close.
        .padding(.top, card.author != nil ? 10 : 4)
    }

    /// Who tagged it what: "kglite: Seen, Backlog", "You: Needs triage".
    private var tagsLine: some View {
        HStack(spacing: 10) {
            ForEach(Array(card.tags.enumerated()), id: \.offset) { _, t in
                Label(t.text, systemImage: t.seen ? "eye" : "tag")
            }
        }
        .font(.system(size: 11)).foregroundStyle(.secondary)
        .padding(.top, 2)
    }

    /// Your tags: the usual ones to switch on and off, and clearing them.
    private func tagMenu(_ menu: ChatPageMenu) -> some View {
        Menu { ChatPageMenuItems(entries: menu.entries) { c, _ in run(c) } } label: { Image(systemName: "tag") }
            .menuStyle(.borderlessButton).menuIndicator(.hidden).fixedSize()
            .foregroundStyle(.secondary).help(menu.help)
    }

    private var cardFill: Color {
        card.mine ? Color.accentColor.opacity(hovered ? 0.16 : 0.11) : Color.secondary.opacity(hovered ? 0.11 : 0.07)
    }
}

/// The members popover (the core's `ChatMembersPanel`): every device in the channel with its role, an owner's role
/// menu for the others, and the devices gone from it with Remove from Channel…, which asks first.
struct ChatMembersView: View {
    let panel: ChatMembersPanel
    let run: (ChatPageCommand) -> Void
    /// The gone device whose removal is being asked about.
    @State private var ask: ChatGoneRow?

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(panel.title).font(.system(size: 12, weight: .semibold)).foregroundStyle(.secondary)
            ForEach(panel.rows, id: \.deviceId) { m in
                HStack(spacing: 10) {
                    Circle().fill(m.online ? Color.green : Color.secondary.opacity(0.4)).frame(width: 7, height: 7)
                    Text(m.name).font(.system(size: 13))
                    Spacer(minLength: 12)
                    if let menu = m.roleMenu {
                        Menu(menu.title) { ChatPageMenuItems(entries: menu.entries) { c, _ in run(c) } }
                            .menuStyle(.button).fixedSize().controlSize(.small)
                    } else {
                        Text(m.role).font(.system(size: 12)).foregroundStyle(.secondary)
                    }
                    if let b = m.message {
                        Button { run(b.command) } label: { Image(systemName: "envelope") }
                            .buttonStyle(.borderless).help(b.help)
                            .accessibilityLabel(b.accessibilityLabel ?? b.title)
                    }
                }
            }
            if !panel.gone.isEmpty { Divider().padding(.vertical, 2) }
            ForEach(panel.gone, id: \.deviceId) { g in
                HStack(spacing: 10) {
                    Text(g.line).font(.system(size: 12)).foregroundStyle(.secondary)
                    Spacer(minLength: 12)
                    Button(g.button.title) { ask = g }.controlSize(.small).help(g.button.help)
                        .accessibilityLabel(g.button.accessibilityLabel ?? g.button.title)
                }
            }
        }
        .padding(14)
        .frame(minWidth: 300)
        .confirmationDialog(ask?.button.question?.title ?? "", isPresented: Binding(get: { ask != nil }, set: { if !$0 { ask = nil } })) {
            if let g = ask, let q = g.button.question {
                Button(q.confirmTitle, role: q.destructive ? .destructive : nil) {
                    run(g.button.command)
                    ask = nil
                }
            }
            Button("Cancel", role: .cancel) { ask = nil }
        } message: {
            if let q = ask?.button.question { Text(q.text) }
        }
    }
}
