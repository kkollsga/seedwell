import Foundation
import Testing
@testable import SeedwellApp

/// Channels (SPEC 7G.7): the conversation page is the core's (crates/ffi/src/chat_page.rs, chat_markdown.rs, with their
/// own tests); these check what the app does with it: the runs draw as Foundation's Markdown drew them, the page and
/// banners carry the core's words, and the dialogs' checks.
@Suite(.timeLimit(.minutes(1))) struct ChatTests {
    static func channel(_ name: String = "Team", unread: UInt32 = 0, members: UInt32 = 3, online: UInt32 = 2, muted: Bool = false,
                        agents: Bool = false, role: SyncRole = .owner) -> SyncChannel {
        SyncChannel(id: "C-\(name)", name: name, role: role, shareUntilS: nil, members: members, online: online, unread: unread,
                    lastMessageS: nil, muted: muted, agents: agents, wakeCommand: nil, progress: nil, problem: nil, waiting: nil, gone: [])
    }

    static func message(_ body: String, by device: String = "D1", name: String = "Studio iMac", user: String = "", at: UInt64 = 1_800_000_000,
                        me: Bool = false, agent: String? = nil, reply: String? = nil, kind: SyncMessageKind = .text) -> SyncMessage {
        SyncMessage(id: "M-\(at)-\(body.count)-0000", channelId: "C-Team", authorDevice: device, authorName: name, authorUser: user, isMe: me, agent: agent,
                    atS: at, replyTo: reply, topic: nil, kind: kind, body: body)
    }

    static func topic(_ title: String = "Dinner", unread: UInt32 = 0, messages: UInt32 = 12) -> SyncTopic {
        SyncTopic(id: "T-\(title)", channelId: "C-Team", title: title, authorName: "Kari's iPhone", createdS: 1_800_000_000,
                  lastMessageS: nil, unread: unread, messages: messages)
    }

    static func page(_ c: SyncChannel, _ messages: [SyncMessage], host: [HostWindow] = hostWindowsAll()) -> ChatPageModel? {
        let read = ChatPageRead(messages: messages, pending: [], follow: nil, directAgents: .off)
        let input = ChatPageInput(place: .chat(channelId: c.id, topicId: nil), title: c.name, channel: c, topic: nil, direct: nil, read: read,
                                  members: [], nowUnixS: 1_800_000_000)
        return chatPageModel(input: input, clock: MacClock(), host: host)
    }

    /// An attributed string as its runs: text, presentation intents and link, as the window's `Text` reads them.
    static func describe(_ a: AttributedString) -> [String] {
        a.runs.map { r in
            let i = r.inlinePresentationIntent ?? []
            let flags = [(InlinePresentationIntent.stronglyEmphasized, "B"), (.emphasized, "I"), (.code, "C"), (.strikethrough, "S")]
                .filter { i.contains($0.0) }.map(\.1).joined()
            return "[\(flags)\(r.link.map { " \($0.absoluteString)" } ?? "")]\(String(a[r.range].characters))"
        }
    }

    @Test func runsDrawAsFoundationsMarkdownDid() throws {
        // The inline Markdown the Mac parsed with Foundation before the core read it: the same text, styles and links.
        let texts = [
            "plain text",
            "**bold** and *italic* and _under_ and __strong__",
            "`code` and ``co`de`` and `unclosed",
            "~~strike~~ and ~single~",
            "[link](https://example.com) and <https://auto.example> and bare https://bare.example.com/x?y=1.",
            "www.example.com and mail me@example.com",
            "**unclosed and [broken](",
            "***both*** and **bold *nested italic* bold**",
            "a\\*not\\* and \\`x\\` and \\\\ backslash",
            "[seed](seedwell://chat/ABC?name=Team)",
            "snake_case_word and 2*3*4 and * spaced *",
            "url (https://example.com/path_(x)) end",
            "Two lines\nstay two",
            "Takk! 🙌 Kan dere sjekke **lørdag** før ~~fredag~~? Se [planen](https://example.com/plan) og `~/Shared/Middag.pdf`.",
            "Ninth with `code`, **bold** and a [link](https://example.com)",
            "*foo**bar**baz* and foo***bar***baz and __init__",
            "&amp; &copy; &#128512;",
        ]
        for t in texts {
            guard case .paragraph(let runs)? = chatBlocks(text: t).first else { Issue.record("no paragraph: \(t)"); continue }
            let foundation = try AttributedString(markdown: t, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace))
            #expect(Self.describe(ChatRuns.attributed(runs)) == Self.describe(foundation), "\(t)")
        }
    }

    @Test func linksThatOpenNothingAreText() {
        // Foundation made links of these; the core does not (a peer's file: or javascript: link never opens).
        guard case .paragraph(let runs)? = chatBlocks(text: "[a](file:///etc/hosts) [b](javascript:x)").first else { Issue.record("no paragraph"); return }
        #expect(runs.allSatisfy { $0.link == nil })
        guard case .paragraph(let seed)? = chatBlocks(text: "[join](seedwell://chat/X)").first else { Issue.record("no paragraph"); return }
        #expect(seed.first?.link?.target == .route)
    }

    @Test func thePageCarriesTheCoresWords() throws {
        let p = try #require(Self.page(Self.channel("Bridge", agents: true), [Self.message("hi", user: "Kari")]))
        #expect(p.toolbar.tag?.text == "agents")
        #expect(p.toolbar.share?.title == "Share…")
        #expect(p.composer.placeholder == "Message Bridge" && p.composer.canPost)
        #expect(p.messages.first?.meta?.name == "Kari" && p.messages.first?.meta?.detail.hasPrefix("Studio iMac · ") == true)
        #expect(p.messages.first?.who == .theirs && !p.bubbles)
        // The Windows app's windows: no Share dialog yet.
        let w = try #require(Self.page(Self.channel("Bridge"), [], host: [.getInfo, .moveTo, .newChannel, .chatDialogs]))
        #expect(w.toolbar.share == nil)
        #expect(w.empty == "No messages yet")
        // The snapshot's sample conversation: every card says who wrote it, the bug mail has its tags.
        let cards = SnapshotRenderer.sampleCards()
        #expect(cards.count == 8 && cards.allSatisfy { $0.meta != nil })
        #expect(cards[6].tags.map(\.text) == ["kglite: Seen, Backlog", "You: Needs triage"])
    }

    @Test func returnSendsAndShiftOrOptionReturnStartsALine() {
        #expect(chatComposerKey(shift: false, alt: false) == .send)
        #expect(chatComposerKey(shift: true, alt: false) == .newLine)
        #expect(chatComposerKey(shift: false, alt: true) == .newLine)
        #expect(!chatComposerCanSend(draft: " \n", attachments: 0) && chatComposerCanSend(draft: " \n", attachments: 1))
        #expect(chatComposerBody(draft: "  hi \n") == "hi")
    }

    @Test func notificationsAndNames() {
        let n = chatNotificationText(placeTitle: chatPlaceTitle(channel: Self.channel(), topic: nil), message: Self.message("Build is ready", agent: "ci"))
        #expect(n.title == "Team" && n.body == "ci on Studio iMac: Build is ready")
        #expect(chatPlaceTitle(channel: Self.channel(), topic: Self.topic()) == "Team › Dinner")
        #expect(chatMessageAuthor(message: Self.message("hi", me: true)) == "You")
        #expect(chatMessagePreview(message: Self.message("## Plan\n\n1. **Build** it"), limit: 120) == "Plan 1. Build it")
        #expect(chatLooksLikeLink(text: " seedwell://chat/ABC?name=Team "))
        #expect(!chatLooksLikeLink(text: "seedwell://sync/ABC"))
    }

    @Test func topicsAndPrivateConversations() {
        // The New Topic and Rename dialogs are the core's (crates/ffi/src/chat_dialogs.rs): what the field becomes.
        let topic = chatNewTopicDialog(channel: Self.channel())
        #expect(chatDialogCheck(dialog: topic, input: "  Dinner on Saturday \n") == .ready(value: "Dinner on Saturday"))
        #expect(chatDialogCheck(dialog: topic, input: " ") == .problem(text: "Give the topic a title."))
        let rename = chatRenameDialog(channel: Self.channel())
        #expect(rename.field?.text == "Team" && rename.confirmTitle == "Rename" && rename.cancelTitle == "Cancel")
        #expect(chatDialogCheck(dialog: rename, input: "Team") == .unchanged)
        #expect(chatDialogCheck(dialog: rename, input: "  Family \n") == .ready(value: "Family"))
        // The channel counts every unread message; General shows what its topics don't.
        let topics = [Self.topic(unread: 2), Self.topic("Trip", unread: 1)]
        #expect(chatGeneralUnread(channel: Self.channel(unread: 5), topics: topics) == 2)
        #expect(chatGeneralUnread(channel: Self.channel(unread: 1), topics: topics) == 0, "never below zero while the counts catch up")
    }

    @Test func tagsAreALineOnTheirMessageByOwnerNotCards() throws {
        let bug = Self.message("---\nmail: 1\ntype: bug\nsummary: Wrong repo\n---\nSteps", agent: "mcp-servers")
        let kglite = Self.message(chatTagNote(messageId: bug.id, tags: ["seen", "backlog"]), by: "D2", name: "Laptop", at: 1_800_000_120, agent: "KGLite")
        let mine = Self.message(chatTagNote(messageId: bug.id, tags: ["needs-triage"]), by: "D0", name: "Mac mini", at: 1_800_000_180, me: true)
        let p = try #require(Self.page(Self.channel(), [bug, kglite, mine]))
        #expect(p.messages.map(\.id) == [bug.id], "tag notes are no cards")
        #expect(p.messages[0].tags.map(\.text) == ["KGLite: Seen, Backlog", "You: Needs triage"])
        guard case .mail(let banner, _) = p.messages[0].body, case .mail(let chip, let tint, _, _, let summary) = banner else {
            Issue.record("no mail: \(p.messages[0].body)"); return
        }
        #expect(chip == "Bug" && tint == .red && summary == "Wrong repo")
    }

    /// Reply opens the reply bar above the composer; Escape and its × cancel it (both call `cancelReply`, which says
    /// whether Escape was used up), and another conversation drops it. The mock's channel has the user's, their agent's,
    /// another person's and other agents' messages.
    @MainActor @Test func replyOpensTheBarAndEscapeCancelsIt() throws {
        let dir = NSTemporaryDirectory() + "seedwell-chat-reply-\(getpid())"
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let core = try Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
        let notes = try #require(core.chatChannels().first { $0.name == "Release notes" })
        let m = LibraryModel(core: core, section: .chat(channelId: notes.id, topicId: nil))
        m.loadChatMessages()
        let cards = try #require(m.chatPage?.messages)
        #expect(Set(cards.map(\.who)) == [.mine, .ourAgent, .theirs, .theirAgent])
        let kari = try #require(cards.first { $0.who == .theirs })
        #expect(!m.cancelReply(), "Escape without a reply is the window's")
        m.reply(to: kari)
        #expect(m.chatReply?.replying.title == "Replying to Kari")
        #expect(m.cancelReply() && m.chatReply == nil)
        m.reply(to: kari)
        m.select(.downloads)
        m.select(.chat(channelId: notes.id, topicId: nil))
        m.loadChatMessages()
        #expect(m.chatReply == nil, "another conversation drops the reply")
    }
}
