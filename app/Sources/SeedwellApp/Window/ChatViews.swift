import AppKit
import SwiftUI

/// How a conversation looks (drawing only; whose a message is and every word are the core's `ChatPageModel`).
enum ChatLook {
    /// The widest the conversation's column (and its composer) is drawn; it is centred in a wider window.
    static let column: CGFloat = 820
    /// The widest a bubble of a private conversation is.
    static let bubble: CGFloat = 520
    /// The agent's symbol, beside an agent's name and in a round mark at the end of another person's agent's run.
    static let agentSymbol = "sparkles"
    /// The line under a run (who wrote it; the pointer's controls): its height, and its gap to the message above.
    static let metaHeight: CGFloat = 18
    static let metaGap: CGFloat = 4

    /// The user's agents: the accent, muted and greyed, so the user's and their agents' read as one family.
    static var agentAccent: Color {
        Color(nsColor: NSColor(name: nil) { appearance in
            let dark = appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            var accent = NSColor.controlAccentColor
            appearance.performAsCurrentDrawingAppearance { accent = NSColor.controlAccentColor.usingColorSpace(.sRGB) ?? .systemBlue }
            return accent.blended(withFraction: dark ? 0.68 : 0.62, of: .systemGray) ?? .systemGray
        })
    }

    /// The edge on the left of the user's (the accent) and their agents' (the muted accent) messages in a channel.
    /// Every message's left edge: the user's the accent, their agents' the muted accent, each other person's their own
    /// calm colour (the core's `palette` index), their agents' a muted shade of it.
    static func edge(_ who: ChatAuthorKind, _ palette: UInt32?) -> Color {
        switch who {
        case .mine: .accentColor
        case .ourAgent: agentAccent
        case .theirs: person(palette ?? 0, agent: false)
        case .theirAgent: person(palette ?? 0, agent: true)
        }
    }

    /// The hues of other people's edges (`chatPaletteSize()` of them, in the core's order): amber, ochre, khaki, teal,
    /// lavender, plum, mauve, dusty rose; soft and low in saturation, away from the accent's blue and from the status
    /// red and green.
    static let hues: [CGFloat] = [28, 44, 62, 168, 262, 288, 318, 342]

    /// A person's colour (`index`), or their agents' muted shade of it, for dark and light.
    static func person(_ index: UInt32, agent: Bool) -> Color {
        let hue = hues[Int(index) % hues.count] / 360
        return Color(nsColor: NSColor(name: nil) { appearance in
            let dark = appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            let saturation: CGFloat = (dark ? 0.36 : 0.42) * (agent ? 0.45 : 1)
            return NSColor(hue: hue, saturation: saturation, brightness: dark ? 0.78 : 0.68, alpha: 1)
        })
    }

    /// A message's background in a channel: only the user's own are tinted (faintly, with the accent); the rest have
    /// none, a soft grey under the pointer.
    static func fill(_ who: ChatAuthorKind, hovered: Bool) -> Color {
        if who == .mine { return Color.accentColor.opacity(hovered ? 0.11 : 0.07) }
        return Color.primary.opacity(hovered ? 0.045 : 0)
    }

    /// A bubble of a private conversation: the user's in the accent (the muted accent for their agents), the other's in
    /// a neutral grey.
    static func bubble(_ who: ChatAuthorKind) -> Color {
        switch who {
        case .mine: .accentColor
        case .ourAgent: agentAccent
        case .theirs, .theirAgent: Color.primary.opacity(0.08)
        }
    }
}

extension ChatAuthorKind {
    /// The user's or their agents'.
    var ours: Bool { self == .mine || self == .ourAgent }
    /// Posted by an agent.
    var agent: Bool { self == .ourAgent || self == .theirAgent }
}

/// A small quiet word in a capsule: the "agents" tag of a channel open to agents in the sidebar (SPEC 7G.7).
struct ChatTagCapsule: View {
    let text: String
    let help: String

    var body: some View {
        Text(text).font(Theme.micro)
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
    /// What a screen reader says for the icon (a group's: "Sparkles"); nil: the symbol's own.
    var iconLabel: String?
    /// The icon in the accent colour: a closed group that holds the channel shown (`SidebarChatGroup.holdsSelection`).
    var iconAccent = false

    var body: some View {
        HStack(spacing: 8) {
            if iconAccent {
                SidebarAccentMark { Image(systemName: icon).frame(width: 20).foregroundStyle($0) }
                    .accessibilityLabel(iconLabel ?? "")
            } else {
                let image = Image(systemName: icon).frame(width: 20).foregroundStyle(.secondary)
                if let iconLabel { image.accessibilityLabel(iconLabel) } else { image }
            }
            Text(name).lineLimit(1).fontWeight(unread > 0 ? .semibold : .regular)
            if let tag { ChatTagCapsule(text: tag.text, help: tag.help) }
            if let online {
                Circle().fill(online ? Color.green : Color.secondary.opacity(0.4)).frame(width: 6, height: 6)
                    .accessibilityLabel(online ? "Online" : "Offline")
            }
            Spacer(minLength: 4)
            if unread > 0 { UnreadCapsule(count: unread) }
        }
        .accessibilityElement(children: .combine)
    }
}

/// A row's unread count: white on an accent capsule; in a selected row of a list that has the keyboard (an accent
/// row) it turns over, the accent on a white capsule, as the system's source list badges do, so it does not melt into
/// the row.
struct UnreadCapsule: View {
    let count: UInt32
    @Environment(\.backgroundProminence) private var prominence

    var body: some View {
        let onAccent = prominence == .increased
        Text("\(count)").font(Theme.micro.weight(.semibold)).monospacedDigit()
            .padding(.horizontal, 6).padding(.vertical, 1)
            .background(Capsule().fill(onAccent ? Color.white : Color.accentColor))
            .foregroundStyle(onAccent ? Color.accentColor : Color.white)
    }
}

/// A small mark in the accent colour beside a sidebar item (a place's new-download dot, a favourite tracker's star):
/// white in an accent row, where the accent would not show.
struct SidebarAccentMark<Content: View>: View {
    @ViewBuilder let content: (Color) -> Content
    @Environment(\.backgroundProminence) private var prominence

    var body: some View { content(prominence == .increased ? .white : .accentColor) }
}

/// A menu of a conversation's page as the core lists it (the ⋯ menu, a message's tags, a member's role menu): items,
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

/// A conversation's controls in the window's toolbar, next to its name (the core's `ChatPageToolbar`), as symbols with
/// their words in tooltips: a channel's agents mark, Members, Share… and its ⋯ menu; a private conversation's ⋯ menu.
struct ChatToolbarControls: View {
    @Bindable var model: LibraryModel
    let page: ChatPageModel
    let actions: ChatActions
    @State private var showMembers = false

    var body: some View {
        let t = page.toolbar
        HStack(spacing: Theme.Space.xs) {
            if let tag = t.tag {
                // A mark, not a control: what Agents: Yes means is in its tooltip.
                Image(systemName: ChatLook.agentSymbol).foregroundStyle(.secondary)
                    .help(tag.help).accessibilityLabel(tag.text)
                    .padding(.horizontal, Theme.Space.xs)
            }
            if let m = t.members {
                Button { run(m.command, nil); showMembers = true } label: { Image(systemName: "person.2") }
                    .help(m.help).accessibilityLabel(m.title)
                    .popover(isPresented: $showMembers, arrowEdge: .bottom) {
                        if let panel = model.chatPage?.members { ChatMembersView(panel: panel) { run($0, nil) } }
                    }
            }
            if let s = t.share {
                Button { run(s.command, nil) } label: { Image(systemName: "square.and.arrow.up") }
                    .help(s.help).accessibilityLabel(s.title)
            }
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
/// the messages in a centred column (bubbles in a private conversation) and the composer under them, with the reply
/// bar docked on top of it. Its name and line are the window's title, its controls in the toolbar
/// (`ChatToolbarControls`). Here: drawing, the field's text and the attachments waiting; the reply is the model's.
struct ChatConversationView: View {
    @Bindable var model: LibraryModel
    let page: ChatPageModel
    let actions: ChatActions
    @State private var draft = ""
    /// The message a quote was clicked for, lit for a moment.
    @State private var highlighted: String?
    /// Where the cursor is in the field (UTF-16): an image pasted there leaves its reference there.
    @State private var selection = NSRange(location: 0, length: 0)
    /// The field has the keyboard.
    @State private var typing = false
    /// Images and files waiting to be sent.
    @State private var images = ChatComposerImages()

    var body: some View {
        VStack(spacing: 0) {
            messages
            Group {
                if page.composer.canPost {
                    ChatComposerView(model: model, page: page, draft: $draft, selection: $selection, typing: $typing, images: images, send: send)
                } else {
                    Text(page.composer.cannotPost ?? "").font(Theme.secondary).foregroundStyle(.secondary)
                        .multilineTextAlignment(.center).frame(maxWidth: .infinity).padding(.vertical, Theme.Space.s)
                }
            }
            .frame(maxWidth: ChatLook.column)
            .padding(.horizontal, Theme.Space.xl).padding(.top, Theme.Space.xs).padding(.bottom, Theme.Space.l)
        }
        .chatImageInput(images, core: model.core, enabled: page.composer.attach)
        .onChange(of: model.chatReply?.id) { _, id in if id != nil { typing = true } }
        .onAppear {
            let (draft, selection) = ($draft, $selection)
            images.placed = { first, count, at in Self.place(images: first..<(first + count), at: at, in: draft, selection: selection) }
            images.removed = { n in draft.wrappedValue = chatComposerWithoutImage(draft: draft.wrappedValue, index: UInt32(n)) }
        }
    }

    /// Images pasted or dropped into the text leave a reference each where they went, the cursor after them
    /// (`chatComposerWithImages`); into an empty field they are plain attachments.
    static func place(images: Range<Int>, at: NSRange?, in draft: Binding<String>, selection: Binding<NSRange>) {
        let range = at.flatMap { $0.location == NSNotFound ? nil : ChatTextRange(start: UInt32(clamping: $0.location), length: UInt32(clamping: $0.length)) }
        guard let edit = chatComposerWithImages(draft: draft.wrappedValue, first: UInt32(clamping: images.lowerBound),
                                                count: UInt32(clamping: images.count), at: range) else { return }
        selection.wrappedValue = NSRange(location: Int(edit.caret), length: 0)
        draft.wrappedValue = edit.text
    }

    private var messages: some View {
        ScrollViewReader { scroll in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    if let empty = page.empty {
                        Text(empty).font(Theme.secondary).foregroundStyle(.tertiary).padding(.top, 60)
                            .frame(maxWidth: .infinity).multilineTextAlignment(.center)
                    }
                    ForEach(page.messages, id: \.id) { card in
                        ChatMessageRow(card: card, core: model.core, bubbles: page.bubbles) { run($0, card) }
                            .id(card.id)
                    }
                    .environment(\.chatReveal) { id in withAnimation(.easeOut(duration: 0.2)) { scroll.scrollTo(id, anchor: .top) } }
                    .environment(\.chatShowMessage) { id in
                        withAnimation(.easeOut(duration: 0.2)) { scroll.scrollTo(id, anchor: .center) }
                        highlighted = id
                        Task { @MainActor in
                            try? await Task.sleep(for: .seconds(1.4))
                            if highlighted == id { highlighted = nil }
                        }
                    }
                    .environment(\.chatHighlighted, highlighted)
                }
                .frame(maxWidth: ChatLook.column)
                .frame(maxWidth: .infinity)
                .padding(.horizontal, Theme.Space.xl).padding(.top, Theme.Space.l).padding(.bottom, Theme.Space.s)
            }
            .onChange(of: page.messages.last?.id) { _, last in
                if let last { withAnimation(.easeOut(duration: 0.15)) { scroll.scrollTo(last, anchor: .bottom) } }
            }
            .onAppear { if let last = page.messages.last?.id { scroll.scrollTo(last, anchor: .bottom) } }
        }
    }

    /// A message's command: Reply opens the reply bar; the rest are the page's.
    private func run(_ command: ChatPageCommand, _ card: ChatMessageCard) {
        if case .reply = command {
            model.reply(to: card)
            typing = true
            return
        }
        actions.run(command, dialog: nil, model: model)
    }

    private func send() {
        let (text, reply) = (draft, model.chatReply?.id)
        if !images.items.isEmpty {
            draft = ""
            model.cancelReply()
            images.send(core: model.core, place: page.place, topic: page.topic, text: text, replyTo: reply) { [model] sent in
                if sent { model.loadChatMessages() } else if draft.isEmpty { draft = text }
            }
            return
        }
        guard chatComposerCanSend(draft: text, attachments: 0) else { return }
        draft = ""
        model.cancelReply()
        actions.send(to: model.section, text, replyTo: reply) { [model] sent in
            if sent { model.loadChatMessages() } else if draft.isEmpty { draft = text }
        }
    }
}

/// The composer: one field with the reply bar on top while replying, the waiting sends and attachments, then the
/// attach button, the text (`ChatComposerField`) and Send.
struct ChatComposerView: View {
    @Bindable var model: LibraryModel
    let page: ChatPageModel
    @Binding var draft: String
    @Binding var selection: NSRange
    @Binding var typing: Bool
    let images: ChatComposerImages
    let send: () -> Void

    var body: some View {
        let c = page.composer
        let canSend = chatComposerCanSend(draft: draft, attachments: UInt32(images.items.count))
        return VStack(alignment: .leading, spacing: 0) {
            if let r = model.chatReply {
                ChatReplyBar(quote: r.replying, help: c.replyCancelHelp) { model.cancelReply() }
                Divider().opacity(0.6)
            }
            if !c.pending.isEmpty {
                ChatPendingSendsView(core: model.core, place: page.place, pending: c.pending) { [model] in model.loadChatMessages() }
                    .padding(.horizontal, Theme.Space.m).padding(.top, Theme.Space.s)
            }
            if !images.items.isEmpty {
                ChatComposerChips(images: images, core: model.core).padding(.horizontal, Theme.Space.s).padding(.top, Theme.Space.s)
            }
            HStack(alignment: .bottom, spacing: Theme.Space.xs) {
                if c.attach { ChatAttachButton(images: images, core: model.core) }
                // Escape cancels the reply (and is left to the field when there is none).
                ChatComposerField(text: $draft, selection: $selection, focused: $typing, placeholder: c.placeholder, attach: c.attach,
                                  images: images, core: model.core, send: send, escape: { [model] in model.cancelReply() })
                Button(action: send) {
                    Image(systemName: "arrow.up.circle.fill").font(.system(size: Theme.Icon.send))
                        .foregroundStyle(canSend ? Color.accentColor : Color.secondary.opacity(0.35))
                        .frame(width: 28, height: 30)
                }
                .buttonStyle(.plain)
                .disabled(!canSend)
                .help(c.sendHelp)
                .accessibilityLabel(c.sendTitle)
            }
            .padding(.leading, Theme.Space.xs).padding(.trailing, Theme.Space.xs)
        }
        .background(RoundedRectangle(cornerRadius: Theme.Radius.large, style: .continuous).fill(Color.primary.opacity(0.045)))
        .overlay(RoundedRectangle(cornerRadius: Theme.Radius.large, style: .continuous)
            .strokeBorder(typing ? Color.accentColor.opacity(0.45) : Color.primary.opacity(0.1)))
    }
}

/// The reply bar docked on top of the composer: "Replying to Kari", the message's line, and × (Escape does the same).
struct ChatReplyBar: View {
    let quote: ChatQuote
    let help: String
    let cancel: () -> Void

    var body: some View {
        HStack(spacing: Theme.Space.s) {
            RoundedRectangle(cornerRadius: 1.5).fill(Color.accentColor).frame(width: 3)
            VStack(alignment: .leading, spacing: 1) {
                Text(quote.title).font(Theme.caption.weight(.semibold)).foregroundStyle(Color.accentColor)
                Text(quote.text).font(Theme.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.tail)
            }
            Spacer(minLength: Theme.Space.s)
            Button(action: cancel) { Image(systemName: "xmark").font(Theme.caption.weight(.semibold)).frame(width: 22, height: 22).contentShape(Rectangle()) }
                .buttonStyle(.plain).foregroundStyle(.secondary)
                .help(help).accessibilityLabel(help)
        }
        .fixedSize(horizontal: false, vertical: true)
        .padding(.leading, Theme.Space.m).padding(.trailing, Theme.Space.s).padding(.vertical, Theme.Space.s)
        .accessibilityElement(children: .contain)
    }
}

/// A message as the core lays it out (`ChatMessageCard`): a quoted reply, the text (Markdown, a mail, JSON), its images
/// and files and its tags, and under the last message of a run who wrote it. In a channel one column, the user's and
/// their agents' with an accent edge and a faint tint; in a private conversation bubbles, the user's on the right.
/// While the pointer is on it a small pill floats over its corner with its time, Tag and Reply: nothing moves.
struct ChatMessageRow: View {
    let card: ChatMessageCard
    /// Draws its images and files (none without it).
    var core: Core? = nil
    /// A private conversation's bubbles (else a channel's column).
    var bubbles = false
    /// Runs a command of the message (Reply, a tag).
    var run: (ChatPageCommand) -> Void = { _ in }
    @State private var hovered: Bool
    /// The gallery's image shown, once one was chosen.
    @State private var galleryImage: Int?
    @Environment(\.chatReveal) private var reveal
    private var inRunGap: CGFloat { Theme.inRunGap }
    @Environment(\.chatHighlighted) private var highlighted

    init(card: ChatMessageCard, core: Core? = nil, bubbles: Bool = false, hovered: Bool = false, run: @escaping (ChatPageCommand) -> Void = { _ in }) {
        self.card = card
        self.core = core
        self.bubbles = bubbles
        self.run = run
        _hovered = State(initialValue: hovered)
    }

    var body: some View {
        Group { if bubbles { bubbleRow } else { columnRow } }
            // Shown from a reply's quote: lit for a moment.
            .background {
                RoundedRectangle(cornerRadius: Theme.Radius.medium, style: .continuous)
                    .fill(Color.accentColor.opacity(highlighted == card.id ? 0.16 : 0))
                    .padding(-Theme.Space.xs)
                    .animation(.easeOut(duration: 0.35), value: highlighted == card.id)
            }
            // A run's first message stands apart from the one before; the rest of the run sits close (`Theme.inRunGap`;
            // in a channel with no gap, one block rounded only at its ends; the line under it says who wrote it).
            .padding(.top, card.startsRun ? Theme.Space.m : inRunSpace)
            // A QA run reaches the message's own state through what its pointer and clicks call (`qa chat hover|gallery|chip`).
            .onAppear { if ChatQAHooks.enabled { ChatQAHooks.rows[card.id] = qaHooks } }
            .onDisappear { if ChatQAHooks.enabled { ChatQAHooks.rows[card.id] = nil } }
    }

    /// What the pointer, the gallery and the image references of this message call, for QA.
    private var qaHooks: ChatQAHooks.Row {
        ChatQAHooks.Row(hover: { hovered = $0 }, hovered: { hovered }, gallery: { (galleryImage, card.images.count) },
                        show: { galleryImage = $0 },
                        reference: { n in _ = URL(string: "\(ChatRuns.imageScheme):\(n)").map { openReference($0) } })
    }

    /// The space above a message inside a run.
    private var inRunSpace: CGFloat { (bubbles ? Theme.Space.hair : 0) + inRunGap }

    /// Where the pointer's controls of a message inside a run sit: centred on the space under it.
    private var hoverDrop: CGFloat { ChatLook.metaHeight / 2 + inRunSpace / 2 }

    /// The run's block in a channel: rounded at the run's first and last message only (each, once they stand apart).
    private var runShape: UnevenRoundedRectangle {
        let r = Theme.Radius.small
        let (top, bottom) = (card.startsRun || inRunGap > 0 ? r : 0, card.meta != nil || inRunGap > 0 ? r : 0)
        return UnevenRoundedRectangle(topLeadingRadius: top, bottomLeadingRadius: bottom, bottomTrailingRadius: bottom, topTrailingRadius: top, style: .continuous)
    }

    // MARK: A channel

    /// The message's block (tinted for the user's and their agents'), then, under the run's last message, who wrote it
    /// on a line of its own outside the block. The pointer's controls (time, Tag, Reply) sit on that line's right end,
    /// or in the same place under a message inside a run, drawn over the space below: nothing moves.
    private var columnRow: some View {
        VStack(alignment: .leading, spacing: ChatLook.metaGap) {
            VStack(alignment: .leading, spacing: Theme.Space.xs) {
                if let q = card.replyQuote { ChatQuoteLine(quote: q) }
                images
                content
                files
                if !card.tags.isEmpty { tagsLine }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.leading, Theme.Space.m + 2).padding(.trailing, Theme.Space.m)
            .padding(.top, card.startsRun ? Theme.Space.s - 1 : 3).padding(.bottom, card.meta != nil ? Theme.Space.s - 1 : 3)
            .background(alignment: .leading) {
                ZStack(alignment: .leading) {
                    Rectangle().fill(ChatLook.fill(card.who, hovered: hovered))
                    Rectangle().fill(ChatLook.edge(card.who, card.palette)).frame(width: 2.5)
                }
                .clipShape(runShape)
            }
            .overlay(alignment: .bottomTrailing) {
                // Under a message inside a run (whose messages touch): on the edge below it, at the right, drawn over it.
                if hovered, card.meta == nil { hoverRow.offset(y: hoverDrop).padding(.trailing, Theme.Space.xs) }
            }
            if let meta = card.meta {
                ChatMetaLine(meta: meta, who: card.who)
                    .padding(.leading, Theme.Space.m + 2)
                    .frame(maxWidth: .infinity, minHeight: ChatLook.metaHeight, alignment: .leading)
                    .overlay(alignment: .trailing) { if hovered { hoverRow.padding(.trailing, Theme.Space.xs) } }
            }
        }
        .contentShape(Rectangle())
        .onHover { hovered = $0 }
    }

    // MARK: A private conversation

    /// The bubble (none for an image or a file alone), then the run's line under the last one, on the bubble's side,
    /// with the pointer's controls at the line's other end.
    private var bubbleRow: some View {
        let ours = card.who.ours
        return HStack(spacing: 0) {
            if ours { Spacer(minLength: 80) }
            VStack(alignment: ours ? .trailing : .leading, spacing: Theme.Space.xs) {
                images
                if hasText {
                    VStack(alignment: .leading, spacing: Theme.Space.xs) {
                        if let q = card.replyQuote { ChatQuoteLine(quote: q, onAccent: ours) }
                        content
                    }
                    .padding(.horizontal, Theme.Space.m).padding(.vertical, 7)
                    .foregroundStyle(ours ? Color.white : Color.primary)
                    .tint(ours ? Color.white : nil)
                    .background(RoundedRectangle(cornerRadius: 16, style: .continuous).fill(ChatLook.bubble(card.who)))
                    // Inside a run: the controls hang under the bubble at its far end, over the space below it.
                    .overlay(alignment: ours ? .bottomLeading : .bottomTrailing) {
                        if hovered, card.meta == nil { hoverRow.offset(y: hoverDrop) }
                    }
                    .frame(maxWidth: ChatLook.bubble, alignment: ours ? .trailing : .leading)
                }
                files
                if !card.tags.isEmpty { tagsLine }
                if let meta = card.meta {
                    // The run's line, and the controls beside it on the side away from the edge: their room is kept while
                    // they are hidden, so showing them moves nothing.
                    HStack(spacing: Theme.Space.s) {
                        if ours { hoverRow.opacity(hovered ? 1 : 0).allowsHitTesting(hovered) }
                        ChatMetaLine(meta: meta, who: card.who)
                        if !ours { hoverRow.opacity(hovered ? 1 : 0).allowsHitTesting(hovered) }
                    }
                    .padding(.horizontal, Theme.Space.xs)
                    .frame(minHeight: ChatLook.metaHeight).padding(.top, ChatLook.metaGap - Theme.Space.xs)
                }
            }
            .fixedSize(horizontal: false, vertical: true)
            .overlay(alignment: ours ? .bottomLeading : .bottomTrailing) {
                // An image or a file alone (no bubble) inside a run.
                if hovered, card.meta == nil, !hasText { hoverRow.offset(y: hoverDrop) }
            }
            if !ours { Spacer(minLength: 80) }
        }
        .contentShape(Rectangle())
        .onHover { hovered = $0 }
    }

    /// It has text (or a quote) to show, not only images and files.
    private var hasText: Bool {
        if card.replyQuote != nil { return true }
        if case .markdown(let blocks) = card.body { return !blocks.isEmpty }
        return true
    }

    // MARK: Parts

    private var content: some View {
        bodyContent.environment(\.openURL, openReference)
    }

    @ViewBuilder private var bodyContent: some View {
        let accent = bubbles && card.who.ours
        switch card.body {
        case .markdown(let blocks):
            if !blocks.isEmpty { ChatMarkdownView(blocks: blocks, fill: !bubbles, onAccent: accent) }
        case .mail(let banner, let blocks):
            // A mail between projects: its header as a line, then its text.
            VStack(alignment: .leading, spacing: Theme.Space.s) {
                ChatMailBannerView(banner: banner)
                if !blocks.isEmpty { ChatMarkdownView(blocks: blocks, fill: !bubbles, onAccent: accent) }
            }
        case .json(let text):
            Text(text).font(Theme.mono).textSelection(.enabled)
                .padding(.horizontal, Theme.Space.s + 2).padding(.vertical, Theme.Space.s)
                .background(RoundedRectangle(cornerRadius: Theme.Radius.small).fill(Color.primary.opacity(accent ? 0 : 0.05)))
        case .unsupported(let text):
            Text(text).font(Theme.body).foregroundStyle(.secondary)
        }
    }

    /// The message's image or gallery, at its top.
    @ViewBuilder private var images: some View {
        if let core, !card.images.isEmpty {
            // Centred in a channel's column; on its side, as wide as a bubble at most, in a private conversation.
            let side: Alignment = !bubbles ? .center : card.who.ours ? .trailing : .leading
            ChatImagesView(core: core, channelId: card.place, images: card.images, gallery: card.gallery, alignment: side,
                           maxWidth: bubbles ? ChatLook.bubble : .infinity, selection: $galleryImage)
        }
    }

    /// Its files, in their compact cards under the text.
    @ViewBuilder private var files: some View {
        if let core, !card.files.isEmpty { ChatFilesView(core: core, place: card.place, files: card.files) }
    }

    /// A click on an image reference in the text: the gallery shows that image, and the message's top comes into view.
    private var openReference: OpenURLAction {
        OpenURLAction { url in
            guard url.scheme == ChatRuns.imageScheme, let n = Int(url.absoluteString.dropFirst(ChatRuns.imageScheme.count + 1)) else { return .systemAction }
            galleryImage = n
            reveal(card.id)
            return .handled
        }
    }

    /// Who tagged it what: "kglite: Seen, Backlog", "You: Needs triage".
    private var tagsLine: some View {
        HStack(spacing: Theme.Space.m) {
            ForEach(Array(card.tags.enumerated()), id: \.offset) { _, t in
                Label(t.text, systemImage: t.seen ? "eye" : "tag")
            }
        }
        .font(Theme.caption).foregroundStyle(.secondary)
    }

    /// The time, Tag and Reply, small and quiet, at the end of the line under the message.
    private var hoverRow: some View {
        HStack(spacing: 0) {
            Text(card.time).font(Theme.caption).monospacedDigit().foregroundStyle(.tertiary)
                .padding(.horizontal, Theme.Space.xs)
            if let menu = card.tagMenu {
                Menu { ChatPageMenuItems(entries: menu.entries) { c, _ in run(c) } } label: { Image(systemName: "tag") }
                    .menuStyle(.borderlessButton).menuIndicator(.hidden).fixedSize()
                    .frame(width: 24, height: ChatLook.metaHeight)
                    .help(menu.help).accessibilityLabel(menu.help)
            }
            if let reply = card.reply {
                Button { run(reply.command) } label: {
                    Image(systemName: "arrowshape.turn.up.left").frame(width: 24, height: ChatLook.metaHeight).contentShape(Rectangle())
                }
                .buttonStyle(.plain).help(reply.help).accessibilityLabel(reply.title)
            }
        }
        .font(.system(size: Theme.Icon.small))
        .foregroundStyle(.secondary)
        .padding(.horizontal, Theme.Space.hair)
        .background(.bar, in: Capsule())
        .fixedSize()
    }
}

/// The message a message answers, one quiet line over it: "↩ Kari  Middag på lørdag?".
struct ChatQuoteLine: View {
    let quote: ChatQuote
    var onAccent = false
    @Environment(\.chatShowMessage) private var show

    var body: some View {
        // A small inset block: the quoted author's edge colour, their name (quiet) and one line of what they wrote. A click
        // scrolls to the original and lights it briefly.
        Button { show(quote.messageId) } label: {
            HStack(spacing: Theme.Space.s) {
                RoundedRectangle(cornerRadius: 1.25).fill(onAccent && quote.who.ours ? Color.white : ChatLook.edge(quote.who, quote.palette))
                    .frame(width: 2.5)
                VStack(alignment: .leading, spacing: 1) {
                    Text(quote.title).font(Theme.caption)
                        .foregroundStyle(onAccent ? AnyShapeStyle(Color.white.opacity(0.75)) : AnyShapeStyle(.secondary))
                    Text(quote.text).font(Theme.caption).lineLimit(1).truncationMode(.tail)
                        .foregroundStyle(onAccent ? AnyShapeStyle(Color.white.opacity(0.9)) : AnyShapeStyle(.primary.opacity(0.8)))
                }
            }
            .padding(.vertical, Theme.Space.xs).padding(.leading, Theme.Space.s - 2).padding(.trailing, Theme.Space.s)
            .background(RoundedRectangle(cornerRadius: Theme.Radius.small, style: .continuous)
                .fill(onAccent ? Color.white.opacity(0.16) : Color.primary.opacity(0.05)))
            .fixedSize(horizontal: false, vertical: true)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(quote.title + ": " + quote.text)
        .accessibilityLabel(quote.title + ": " + quote.text)
    }
}

/// Who wrote a run, under its last message: another person's round initial (an agent's symbol in it for their agent),
/// the agent symbol for the user's agent, then "Kari · Kari's iPhone · 14.32". The device is in its tooltip too.
struct ChatMetaLine: View {
    let meta: ChatMessageMeta
    let who: ChatAuthorKind
    /// The round mark's size: a little smaller than the caption line.
    private static let mark: CGFloat = 13

    var body: some View {
        HStack(spacing: Theme.Space.xs) {
            switch who {
            case .theirs:
                Circle().strokeBorder(Color.secondary.opacity(0.35), lineWidth: 1).frame(width: Self.mark, height: Self.mark)
                    .overlay(Text(meta.initial).font(.system(size: Self.mark * 0.55, weight: .medium)))
            case .theirAgent:
                Circle().strokeBorder(Color.secondary.opacity(0.35), lineWidth: 1).frame(width: Self.mark, height: Self.mark)
                    .overlay(Image(systemName: ChatLook.agentSymbol).font(.system(size: Self.mark * 0.5)))
            case .ourAgent:
                Image(systemName: ChatLook.agentSymbol).font(.system(size: Theme.Size.micro))
            case .mine:
                EmptyView()
            }
            Text(meta.name + " · " + meta.detail).font(Theme.caption).monospacedDigit().lineLimit(1)
        }
        .foregroundStyle(.tertiary)
        .fixedSize()
        .modifier(HelpIfAny(text: meta.help))
        .accessibilityElement(children: .combine)
    }
}

/// The members popover (the core's `ChatMembersPanel`): every device in the channel with its role and, under its name,
/// an admin's removal of it waiting or refused; an owner's role menu for the others, and the devices gone from it with Remove from Channel…, which asks first.
struct ChatMembersView: View {
    let panel: ChatMembersPanel
    let run: (ChatPageCommand) -> Void
    /// The gone device whose removal is being asked about.
    @State private var ask: ChatGoneRow?

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            Text(panel.title).font(Theme.secondary.weight(.semibold)).foregroundStyle(.secondary)
            ForEach(panel.rows, id: \.deviceId) { m in
                HStack(spacing: Theme.Space.s + 2) {
                    Circle().fill(m.online ? Color.green : Color.secondary.opacity(0.4)).frame(width: 7, height: 7)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(m.name).font(Theme.body)
                        // An admin's removal of it: waiting for the owner, or why the owner's device refused.
                        if let r = m.removal { Text(r).font(Theme.caption).foregroundStyle(.secondary) }
                    }
                    Spacer(minLength: Theme.Space.m)
                    if let menu = m.roleMenu {
                        Menu(menu.title) { ChatPageMenuItems(entries: menu.entries) { c, _ in run(c) } }
                            .menuStyle(.button).fixedSize().controlSize(.small)
                    } else {
                        Text(m.role).font(Theme.secondary).foregroundStyle(.secondary)
                    }
                    if let b = m.message {
                        Button { run(b.command) } label: { Image(systemName: "envelope") }
                            .buttonStyle(.borderless).help(b.help)
                            .accessibilityLabel(b.accessibilityLabel ?? b.title)
                    }
                }
            }
            if !panel.gone.isEmpty { Divider().padding(.vertical, Theme.Space.hair) }
            ForEach(panel.gone, id: \.deviceId) { g in
                HStack(spacing: Theme.Space.s + 2) {
                    Text(g.line).font(Theme.secondary).foregroundStyle(.secondary)
                    Spacer(minLength: Theme.Space.m)
                    Button(g.button.title) { ask = g }.controlSize(.small).help(g.button.help)
                        .accessibilityLabel(g.button.accessibilityLabel ?? g.button.title)
                }
            }
        }
        .padding(Theme.Space.l)
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
