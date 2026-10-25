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
        #expect(ChatText.author(Self.message("hi", agent: "claude-code")) == "Studio iMac · agent claude-code")
        #expect(ChatText.author(Self.message("hi", name: "")) == "Unnamed device")
    }

    @Test func previewsAreOneShortLine() {
        #expect(ChatText.preview(Self.message("line one\nline two")) == "line one line two")
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
        #expect(n.title == "Team" && n.body == "Studio iMac · agent ci: Build is ready")
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
}
