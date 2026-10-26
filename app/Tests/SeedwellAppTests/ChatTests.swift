import Foundation
import Testing
@testable import SeedwellApp

/// Channels (SPEC 7G.7): the words of the Chats page, its runs of messages, and its notifications.
@Suite struct ChatTests {
    static func channel(_ name: String = "Team", unread: UInt32 = 0, members: UInt32 = 3, online: UInt32 = 2, muted: Bool = false) -> SyncChannel {
        SyncChannel(id: "C-\(name)", name: name, role: .owner, shareUntilS: nil, members: members, online: online, unread: unread,
                    lastMessageS: nil, muted: muted, agents: .off, wakeCommand: nil)
    }

    static func message(_ body: String, by device: String = "D1", name: String = "Studio iMac", at: UInt64 = 1_800_000_000,
                        me: Bool = false, agent: String? = nil, reply: String? = nil, kind: SyncMessageKind = .text) -> SyncMessage {
        SyncMessage(id: "M-\(at)-\(body.count)", channelId: "C-Team", authorDevice: device, authorName: name, isMe: me, agent: agent,
                    atS: at, replyTo: reply, topic: nil, kind: kind, body: body)
    }

    static func topic(_ title: String = "Dinner", unread: UInt32 = 0, messages: UInt32 = 12) -> SyncTopic {
        SyncTopic(id: "T-\(title)", channelId: "C-Team", title: title, authorName: "Kari's iPhone", createdS: 1_800_000_000,
                  lastMessageS: nil, unread: unread, messages: messages)
    }

    @Test func theListAndPageSayWhatIsGoingOn() {
        #expect(ChatText.header(Self.channel()) == "Owner · 2 of 3 online")
        var c = Self.channel(muted: true)
        c.agents = .read
        #expect(ChatText.header(c) == "Owner · 2 of 3 online · Muted · agents: read")
        #expect(ChatText.subtitle(Self.channel()) == "2 of 3 online")
        #expect(ChatText.subtitle(Self.channel(members: 1, online: 0, muted: true)) == "Only this Mac · Muted")
        #expect(ChatText.agents(.readPost) == "Read and post")
    }

    @Test func authorsAndAgentsAreNamed() {
        #expect(ChatText.author(Self.message("hi")) == "Studio iMac")
        #expect(ChatText.author(Self.message("hi", me: true)) == "You")
        #expect(ChatText.author(Self.message("hi", agent: "Seedwell")) == "Seedwell on Studio iMac")
        #expect(ChatText.author(Self.message("hi", me: true, agent: "Seedwell")) == "Seedwell on Studio iMac", "an agent on this Mac names the Mac too")
        #expect(ChatText.author(Self.message("hi", name: "")) == "Unnamed device")
    }

    @Test func previewsAreOneShortLine() {
        #expect(ChatText.preview(Self.message("line one\nline two")) == "line one line two")
        #expect(ChatText.preview(Self.message("## Plan\n\n1. **Build** it\n- `ship`")) == "Plan 1. Build it • ship", "without its Markdown")
        #expect(ChatText.preview(Self.message(String(repeating: "x", count: 300)), limit: 10) == "xxxxxxxxx…")
        #expect(ChatText.preview(Self.message("{}", kind: .json)) == "JSON message")
        #expect(ChatText.preview(Self.message("?", kind: .other)) == "A message this version cannot show")
    }

    @Test func aRunOfMessagesShowsItsAuthorOnce() {
        let a = Self.message("one", at: 1_000)
        #expect(ChatText.startsRun(a, after: nil))
        #expect(!ChatText.startsRun(Self.message("two", at: 1_100), after: a), "same author, within five minutes")
        #expect(ChatText.startsRun(Self.message("later", at: 1_400), after: a))
        #expect(ChatText.startsRun(Self.message("other", by: "D2", at: 1_050), after: a))
        #expect(ChatText.startsRun(Self.message("agent", at: 1_050, agent: "bot"), after: a), "an agent's message is its own run")
        #expect(ChatText.startsRun(Self.message("re", at: 1_050, reply: a.id), after: a), "a reply shows what it answers")
    }

    @Test func notificationsAndNames() {
        let n = ChatText.notification(ChatText.place(Self.channel(), nil), Self.message("Build is ready", agent: "ci"))
        #expect(n.title == "Team" && n.body == "ci on Studio iMac: Build is ready")
        #expect(ChatText.place(Self.channel(), Self.topic()) == "Team › Dinner")
        #expect(ChatText.channelName("  Family \n") == "Family")
        #expect(ChatText.channelName("   ") == nil)
        #expect(ChatText.looksLikeLink(" seedwell://chat/ABC?name=Team "))
        #expect(!ChatText.looksLikeLink("seedwell://sync/ABC"))
    }

    @Test func topicsAndPrivateConversations() {
        #expect(ChatText.topicLine(Self.topic()) == "Topic by Kari's iPhone · 12 messages")
        #expect(ChatText.directLine(online: false) == "Private · offline · end-to-end encrypted")
        #expect(ChatText.topicTitle("  Dinner on Saturday \n") == "Dinner on Saturday")
        #expect(ChatText.topicTitle(" ") == nil)
        #expect(ChatText.topicTitle(String(repeating: "x", count: 200))?.count == 100)
        // The channel counts every unread message; General shows what its topics don't.
        let topics = [Self.topic(unread: 2), Self.topic("Trip", unread: 1)]
        #expect(ChatText.generalUnread(Self.channel(unread: 5), topics) == 2)
        #expect(ChatText.generalUnread(Self.channel(unread: 1), topics) == 0, "never below zero while the counts catch up")
    }

    @Test func messagesAreMarkdownBlocks() {
        typealias B = ChatMarkdown.Block
        let text = """
        # Status ##
        Built and tested.
        Two lines stay two.

        1. Install it
        2) Check Finder
          with a second line
        - one
          - nested
        > a quote
        ---
        ```sh
        seedwell chat send Team -
        # not a heading

        ```
        ## C#
        """
        #expect(ChatMarkdown.blocks(text) == [
            B.heading(level: 1, text: "Status"),
            .paragraph("Built and tested.\nTwo lines stay two."),
            .item(marker: "1.", depth: 0, text: "Install it"),
            .item(marker: "2.", depth: 0, text: "Check Finder\nwith a second line"),
            .item(marker: "•", depth: 0, text: "one"),
            .item(marker: "•", depth: 1, text: "nested"),
            .quote("a quote"),
            .rule,
            .code(language: "sh", text: "seedwell chat send Team -\n# not a heading\n"),
            .heading(level: 2, text: "C#"),
        ])
        #expect(ChatMarkdown.blocks("#hashtag and 3.5 apples") == [.paragraph("#hashtag and 3.5 apples")], "not a heading, not a list")
        #expect(ChatMarkdown.blocks("```\nunclosed") == [.code(language: "", text: "unclosed")])
        #expect(ChatMarkdown.blocks("- [ ] screenshot\n- [x] changelog") == [.item(marker: "☐", depth: 0, text: "screenshot"), .item(marker: "☑", depth: 0, text: "changelog")])
        #expect(ChatMarkdown.plain("Use **bold** and `code`.\n\n```\nx = 1\n```") == "Use bold and code.\nx = 1")
    }

    @Test func complexMessagesParseIntoTheirBlocks() {
        typealias B = ChatMarkdown.Block
        let report = ChatMarkdown.blocks(ChatSamples.report)
        #expect(report.first == .heading(level: 1, text: "Build report for 0.1.7"))
        #expect(report.contains(.paragraph("All **283** Rust tests and *292* Swift tests passed.\nDetails are in `target/nextest/report.xml`.")))
        #expect(report.contains(.item(marker: "2.", depth: 0, text: "Ran the checks:")))
        #expect(report.contains(.item(marker: "•", depth: 1, text: "the Xcode build with Swift 6.3.3")))
        #expect(report.contains(.item(marker: "☑", depth: 0, text: "CI green")) && report.contains(.item(marker: "☐", depth: 0, text: "Tag `v0.1.7`")))
        #expect(report.contains(.quote("The daylight saving change today moved the times by one hour.\n**Check** the stamps in older logs.")))
        #expect(report.contains(.rule))
        #expect(report.last == .code(language: "rust", text: "fn time(at: u64) -> String {\n    clock(at, offset_at(at)) // local, with the offset of that moment\n}"))

        let table = ChatMarkdown.blocks(ChatSamples.table)
        #expect(table == [
            B.heading(level: 2, text: "Download page"),
            .table(header: ["Build", "Size", "SHA-256", "Notes"], rows: [
                ["0.1.6", "13.4 MB", "`113c97b5…`", "published"],
                ["0.1.7", "13.6 MB", "`pending`", "**draft**"],
                ["0.1.5", "12.9 MB", "", "old | kept"],
            ]),
            .paragraph("The table is in `site/download.md`."),
        ])
        // A short row is padded, a long one cut, to the header's width; a pipe line without its rule is a paragraph.
        #expect(ChatMarkdown.blocks("| a | b |\n|---|---|\n| 1 |\n| 1 | 2 | 3 |") == [.table(header: ["a", "b"], rows: [["1", ""], ["1", "2"]])])
        #expect(ChatMarkdown.blocks("| not | a table |") == [.paragraph("| not | a table |")])
        #expect(ChatMarkdown.blocks("| a \\* b |\n|-|\n| x |") == [.table(header: ["a \\* b"], rows: [["x"]])], "other escapes stay for the inline Markdown")

        let code = ChatMarkdown.blocks(ChatSamples.code)
        #expect(code.count == 4)
        if case .code(let language, let text) = code[1] {
            #expect(language == "sh")
            #expect(text.hasSuffix("```not a fence end```"), "a ``` line inside a ~~~ fence is code")
            #expect(text.contains("compact \\\n  --predicate"))
        } else { Issue.record("no code block: \(code)") }
        if case .code(let language, let text) = code[3] { #expect(language.isEmpty && !text.contains("\n")) } else { Issue.record("no second code block") }

        let nested = ChatMarkdown.blocks(ChatSamples.nested)
        #expect(nested == [
            B.item(marker: "7.", depth: 0, text: "Seventh, numbered as written"),
            .item(marker: "8.", depth: 0, text: "Eighth\ncontinued on an indented line"),
            .item(marker: "•", depth: 1, text: "a point under it"),
            .item(marker: "•", depth: 2, text: "and one deeper"),
            .item(marker: "•", depth: 3, text: "and the deepest"),
            .item(marker: "9.", depth: 0, text: "Ninth with `code`, **bold** and a [link](https://example.com)"),
            .paragraph("#hashtag, 3.5 apples and a lone 1. are plain text."),
        ])
    }

    @Test func everySampleHasAPlainPreviewWithoutMarkdown() {
        for s in ChatSamples.messages {
            let plain = ChatMarkdown.plain(s.body)
            #expect(!plain.isEmpty)
            for mark in ["**", "](", "~~", "|---"] { #expect(!plain.contains(mark), "\(mark) in \(plain)") }
        }
        #expect(ChatMarkdown.plain(ChatSamples.table).contains("0.1.7 · 13.6 MB · pending · draft"))
        #expect(ChatMarkdown.plain(ChatSamples.messages[2].body).hasPrefix("Takk! 🙌 Kan dere sjekke lørdag før fredag? Se planen og ~/Shared/Middag.pdf."))
    }

    @Test func oddMessagesStillRender() {
        #expect(ChatMarkdown.blocks("").isEmpty)
        #expect(ChatMarkdown.blocks(" \n\n \t ").isEmpty)
        #expect(ChatMarkdown.blocks("a\r\nb") == [.paragraph("a\nb")], "Windows line ends")
        #expect(ChatMarkdown.blocks("```") == [.code(language: "", text: "")])
        #expect(ChatMarkdown.blocks("####### seven") == [.paragraph("####### seven")])
        #expect(ChatMarkdown.blocks("#") == [.paragraph("#")])
        #expect(ChatMarkdown.blocks("- ") == [.paragraph("-")], "a dash alone is text")
        #expect(ChatMarkdown.blocks("1234567890. too long a number") == [.paragraph("1234567890. too long a number")])
        #expect(ChatMarkdown.inline("**unclosed and [broken](").characters.count > 0)
        // The largest text message (16 KiB) of every kind of block parses quickly.
        let big = String(repeating: ChatSamples.report + "\n\n", count: 16 * 1024 / ChatSamples.report.utf8.count + 1).prefix(16 * 1024)
        let start = Date()
        let blocks = ChatMarkdown.blocks(String(big))
        #expect(blocks.count > 100)
        #expect(Date().timeIntervalSince(start) < 0.5)
    }

    @Test func mailsShowTheirHeaderAsALine() {
        let bug = ChatMail.parse(ChatSamples.bugMail)
        #expect(bug?.to == "kglite" && bug?.type == "bug" && bug?.typeLabel == "Bug" && bug?.isStatus == false)
        #expect(bug?.summary == "The open-source graph serves the wrong repo after a switch")
        #expect(bug?.body.hasPrefix("After `repo_management") == true, "the header is not text")
        #expect(bug?.line == "Bug: The open-source graph serves the wrong repo after a switch")
        let status = ChatMail.parse(ChatSamples.statusMail)
        #expect(status?.isStatus == true && status?.re == "7f1b588c" && status?.state == "done")
        #expect(status?.line.hasPrefix("Done: Fixed in 0.17.13") == true)
        #expect(ChatMail.parse("---\nnot a mail\n---\nhi") == nil, "a rule, not a mail header")
        #expect(ChatMail.parse("plain text") == nil)
        #expect(ChatMail.parse("---\nmail: 1\nsummary: Only a header\n---")?.body == "")
        #expect(ChatMail.parse("---\r\nmail: 1\r\ntype: request\r\n---\r\nPlease")?.body == "Please")
        // Previews and banners use the line.
        #expect(ChatText.preview(Self.message(ChatSamples.bugMail)) == "Bug: The open-source graph serves the wrong repo after a switch")
    }
}
