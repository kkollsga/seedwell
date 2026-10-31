import Foundation

// Channels (SPEC 7G.7): the words and small rules the Chats page, its notifications and the menu share, as plain data
// with no AppKit, so they can be tested.

enum ChatText {
    /// What agents on this Mac may do in a channel (the core's words).
    static func agents(_ a: SyncAgentAccess) -> String { chatAgentsLabel(access: a) }

    /// An agent channel's tag ("agents" and its tooltip; the core's), `nil` for any other channel.
    static func agentTag(_ c: SyncChannel) -> SidebarTag? { chatAgentTag(channel: c) }

    /// A channel's unread messages under General: its total less its topics' (the core's rule).
    static func generalUnread(_ c: SyncChannel, _ topics: [SyncTopic]) -> UInt32 { chatGeneralUnread(channel: c, topics: topics) }

    /// A topic name as typed: trimmed, at most 100 characters; `nil` when empty.
    static func topicTitle(_ text: String) -> String? {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return t.isEmpty ? nil : String(t.prefix(100))
    }

    /// A message's time: "14.32" today, "Yesterday 14.32", "1 Nov 14.32" before.
    static func time(_ at: UInt64, now: Date = .now) -> String {
        let d = Date(timeIntervalSince1970: TimeInterval(at))
        let cal = Calendar.current
        let clock = d.formatted(.dateTime.hour().minute())
        if cal.isDate(d, inSameDayAs: now) { return clock }
        if let y = cal.date(byAdding: .day, value: -1, to: now), cal.isDate(d, inSameDayAs: y) { return "Yesterday \(clock)" }
        return "\(Fmt.day(at)) \(clock)"
    }

    /// "You", "Kari (Studio iMac)", or for an agent its name and the computer it runs on: "Seedwell on Studio iMac".
    static func author(_ m: SyncMessage) -> String {
        if let agent = m.agent { return "\(agent) on \(computer(m))" }
        return m.isMe ? "You" : named(m.authorName, user: m.authorUser)
    }

    /// The device that sent it, by its name.
    static func computer(_ m: SyncMessage) -> String { m.authorName.isEmpty ? "Unnamed device" : m.authorName }

    /// A device with its user, as every device names it (Settings › Folder Sync): "Kari (Studio iMac)" (the core's words).
    static func named(_ device: String, user: String) -> String { chatNamed(device: device, user: user) }

    /// One line of a message, for notifications, replies and the list: at most `limit` characters.
    static func preview(_ m: SyncMessage, limit: Int = 120) -> String {
        let text: String = switch m.kind {
        case .text: chatMail(text: m.body)?.line ?? ChatMarkdown.plain(m.body)
        case .json: "JSON message"
        case .other: "A message this version cannot show"
        }
        let one = text.replacingOccurrences(of: "\n", with: " ").trimmingCharacters(in: .whitespaces)
        return one.count > limit ? String(one.prefix(limit - 1)) + "…" : one
    }

    /// A run of messages from one author within five minutes shows the author once.
    static func startsRun(_ m: SyncMessage, after prev: SyncMessage?) -> Bool {
        guard let prev else { return true }
        return prev.authorDevice != m.authorDevice || prev.agent != m.agent || m.atS > prev.atS + 300 || m.replyTo != nil
    }

    /// A conversation's name in a banner: "Team", "Team › Dinner", or the device of a private one.
    static func place(_ c: SyncChannel, _ t: SyncTopic?) -> String { t.map { "\(c.name) › \($0.title)" } ?? c.name }

    /// The banner for a new message: "Team › Dinner" / "Studio iMac: Hello there".
    static func notification(_ place: String, _ m: SyncMessage) -> (title: String, body: String) {
        (place, "\(author(m)): \(preview(m, limit: 200))")
    }

    /// A channel name as typed: trimmed, at most 64 characters; `nil` when empty.
    static func channelName(_ text: String) -> String? {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return t.isEmpty ? nil : String(t.prefix(64))
    }

    /// A pasteboard text worth prefilling the join field with: a `seedwell://chat/` link.
    static func looksLikeLink(_ text: String) -> Bool {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return t.count < 2_000 && t.lowercased().hasPrefix("seedwell://chat/")
    }
}
