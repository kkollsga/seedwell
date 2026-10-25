import Foundation

// Channels (SPEC 7G.7): the words and small rules the Chats page, its notifications and the menu share, as plain data
// with no AppKit, so they can be tested.

enum ChatText {
    /// What agents on this Mac may do in a channel.
    static func agents(_ a: SyncAgentAccess) -> String {
        switch a {
        case .off: "Off"
        case .read: "Read"
        case .readPost: "Read and post"
        }
    }

    /// A channel page's line under its name: "Owner · 2 of 3 online · shared until 1 Nov · agents: read".
    static func header(_ c: SyncChannel) -> String {
        [SyncText.role(c.role), subtitle(c), c.shareUntilS.map { "shared until \(SyncText.day($0))" },
         c.agents == .off ? nil : "agents: \(agents(c.agents).lowercased())"].compactMap { $0 }.joined(separator: " · ")
    }

    /// A topic page's line: "Topic by Kari's iPhone · 12 messages".
    static func topicLine(_ t: SyncTopic) -> String {
        "Topic by \(t.authorName.isEmpty ? "an unnamed device" : t.authorName) · \(Fmt.count(t.messages, "message"))"
    }

    /// A private conversation's line: "Private · online · end-to-end encrypted".
    static func directLine(online: Bool) -> String {
        "Private · \(online ? "online" : "offline") · end-to-end encrypted"
    }

    /// A channel's unread messages under General: its total less its topics' (the core counts the total).
    static func generalUnread(_ c: SyncChannel, _ topics: [SyncTopic]) -> UInt32 {
        c.unread - min(c.unread, topics.reduce(0) { $0 + $1.unread })
    }

    /// A topic name as typed: trimmed, at most 100 characters; `nil` when empty.
    static func topicTitle(_ text: String) -> String? {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return t.isEmpty ? nil : String(t.prefix(100))
    }

    /// A channel's grey line in the list: "2 of 3 online", or "Muted".
    static func subtitle(_ c: SyncChannel) -> String {
        var parts = [c.members > 1 ? "\(c.online) of \(c.members) online" : "Only this Mac"]
        if c.muted { parts.append("Muted") }
        return parts.joined(separator: " · ")
    }

    /// A message's time: "14.32" today, "Yesterday 14.32", "1 Nov 14.32" before.
    static func time(_ at: UInt64, now: Date = .now) -> String {
        let d = Date(timeIntervalSince1970: TimeInterval(at))
        let cal = Calendar.current
        let clock = d.formatted(.dateTime.hour().minute())
        if cal.isDate(d, inSameDayAs: now) { return clock }
        if let y = cal.date(byAdding: .day, value: -1, to: now), cal.isDate(d, inSameDayAs: y) { return "Yesterday \(clock)" }
        return "\(SyncText.day(at)) \(clock)"
    }

    /// The author as shown: "Studio iMac", "Studio iMac · agent claude-code", "You".
    static func author(_ m: SyncMessage) -> String {
        let who = m.isMe ? "You" : (m.authorName.isEmpty ? "Unnamed device" : m.authorName)
        return m.agent.map { "\(who) · agent \($0)" } ?? who
    }

    /// One line of a message, for notifications, replies and the list: at most `limit` characters.
    static func preview(_ m: SyncMessage, limit: Int = 120) -> String {
        let text: String = switch m.kind {
        case .text: m.body
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
