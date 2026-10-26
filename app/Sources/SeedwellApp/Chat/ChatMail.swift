import Foundation
import SwiftUI

/// A mail between projects (SPEC 7G.8): a text message that starts with a `---` header (`mail: 1`, `to`, `type`,
/// `re`, `summary`, and for a status `state`), then Markdown. The app shows the header as a line, never as text.
struct ChatMail: Equatable {
    var to: String?
    var type: String?
    var re: String?
    var summary: String?
    /// A status: `done`, or `open` (progress on an item that stays open).
    var state: String?
    /// The Markdown after the header.
    var body: String
    /// A tag note's whole set, comma-separated (SPEC 7G.9).
    var tags: String? = nil

    var isStatus: Bool { type?.lowercased() == "status" }
    /// Bookkeeping, shown as a small line: "seen" (a recipient saw a mail), "hook" (a project's mail hook changed).
    var isNote: Bool { ["seen", "hook", "tag"].contains(type?.lowercased() ?? "") }
    /// Tags on the message `re` names (a seen receipt of mail before tags counts as the tag `seen`): shown on that
    /// message's card, not as a card of its own.
    var isTag: Bool { ["seen", "tag"].contains(type?.lowercased() ?? "") }
    var tagList: [String] {
        if type?.lowercased() == "seen" { return ["seen"] }
        return (tags ?? "").split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces).lowercased() }.filter { !$0.isEmpty }
    }

    /// The mail in a message's text; `nil` for a plain message.
    static func parse(_ text: String) -> ChatMail? {
        let t = text.replacingOccurrences(of: "\r\n", with: "\n")
        guard t.hasPrefix("---\n") else { return nil }
        let rest = t.dropFirst(4)
        guard let end = rest.range(of: "\n---") else { return nil }
        var fields: [String: String] = [:]
        for line in rest[..<end.lowerBound].split(separator: "\n", omittingEmptySubsequences: true) {
            guard let colon = line.firstIndex(of: ":") else { continue }
            let key = line[..<colon].trimmingCharacters(in: .whitespaces).lowercased()
            let value = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
            if !key.isEmpty, !value.isEmpty { fields[key] = value }
        }
        guard fields["mail"] != nil else { return nil }
        // The body starts after the closing line.
        var after = rest[end.upperBound...]
        if let nl = after.firstIndex(of: "\n") { after = after[after.index(after: nl)...] } else { after = "" }
        return ChatMail(to: fields["to"], type: fields["type"], re: fields["re"], summary: fields["summary"], state: fields["state"],
                        body: String(after).trimmingCharacters(in: .whitespacesAndNewlines), tags: fields["tags"])
    }

    /// "Bug", "Heads-up", "Request" for the chip.
    var typeLabel: String? {
        guard let t = type?.trimmingCharacters(in: .whitespaces), !t.isEmpty else { return nil }
        return t.prefix(1).uppercased() + t.dropFirst()
    }

    /// One line for previews, banners and reply quotes: "Bug: Recipe tool returns the wrong catalogue".
    var line: String {
        if isTag { return "Tagged " + tagList.map(ChatTags.label).joined(separator: ", ") }
        if isNote { return summary ?? "Mail hook" }
        if isStatus { return state?.lowercased() == "open" ? "Progress: \(ChatMarkdown.plain(body))" : "Done: \(ChatMarkdown.plain(body))" }
        let what = summary ?? ChatMarkdown.plain(body)
        return typeLabel.map { "\($0): \(what)" } ?? what
    }
}

/// The header line of a mail on its card: the type, whom it is for, and its summary.
struct ChatMailHeader: View {
    let mail: ChatMail

    var body: some View {
        if mail.isNote {
            Label(mail.line, systemImage: mail.type?.lowercased() == "seen" ? "eye" : "bell")
                .font(.system(size: 11.5)).foregroundStyle(.secondary)
        } else if mail.isStatus {
            Label(mail.state?.lowercased() == "open" ? "Progress" : "Done",
                  systemImage: mail.state?.lowercased() == "open" ? "clock" : "checkmark.circle.fill")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(mail.state?.lowercased() == "open" ? Color.orange : Color.green)
        } else {
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    if let t = mail.typeLabel {
                        Text(t).font(.system(size: 10.5, weight: .semibold))
                            .padding(.horizontal, 6).padding(.vertical, 1.5)
                            .background(Capsule().fill(Self.tint(mail.type).opacity(0.18)))
                            .foregroundStyle(Self.tint(mail.type))
                    }
                    if let to = mail.to {
                        Label("to \(to)", systemImage: "tray.and.arrow.down").font(.system(size: 11.5)).foregroundStyle(.secondary)
                    }
                    if let re = mail.re {
                        Text("re \(re.prefix(8))").font(.system(size: 11, design: .monospaced)).foregroundStyle(.secondary)
                    }
                }
                if let s = mail.summary {
                    Text(s).font(.system(size: 13.5, weight: .semibold)).fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }

    static func tint(_ type: String?) -> Color {
        switch type?.lowercased() {
        case "bug": .red
        case "request", "question": .blue
        case "heads-up": .orange
        case "reply": .teal
        default: .secondary
        }
    }
}

/// One owner's tags on a message (SPEC 7G.9): an agent by its name, or the user of a device ("You" on this Mac).
struct ChatTags: Equatable {
    var owner: String
    var isYou: Bool
    var tags: [String]

    /// The tags the menu offers.
    static let known = ["seen", "actioned", "backlog", "needs-triage"]

    /// "Needs triage" for `needs-triage`.
    static func label(_ tag: String) -> String {
        let words = tag.replacingOccurrences(of: "-", with: " ")
        return words.prefix(1).uppercased() + words.dropFirst()
    }

    /// The text of a tag note: the whole set `tags` of whoever posts it, on the message `id`.
    static func note(on id: String, _ tags: [String]) -> String {
        "---\nmail: 1\ntype: tag\nre: \(id.prefix(8).lowercased())\ntags: \(tags.joined(separator: ", "))\n---\n"
    }
}

extension ChatMail {
    /// The messages that are cards (a tag note is not one), and the tags on each message by its short id (the first
    /// 8 characters, as `re` gives it): each owner's newest note is its set, owners without tags left out.
    static func cards(_ ms: [SyncMessage]) -> (cards: [SyncMessage], tags: [String: [ChatTags]]) {
        var cards: [SyncMessage] = []
        var tags: [String: [ChatTags]] = [:]
        for m in ms {
            guard m.kind == .text, let mail = parse(m.body), mail.isTag else { cards.append(m); continue }
            guard let re = mail.re?.lowercased(), re.count >= 8 else { continue }
            let key = String(re.prefix(8))
            let owner = m.agent ?? (m.isMe ? "You" : ChatText.computer(m))
            let isYou = m.agent == nil && m.isMe
            var list = tags[key] ?? []
            let i = list.firstIndex { $0.isYou == isYou && $0.owner.lowercased() == owner.lowercased() }
            // A receipt adds `seen`; a tag note is the owner's whole set.
            let set = mail.type?.lowercased() == "seen" ? Array(Set((i.map { list[$0].tags } ?? []) + ["seen"])).sorted() : mail.tagList
            if let i { list[i].tags = set } else { list.append(ChatTags(owner: owner, isYou: isYou, tags: set)) }
            tags[key] = list
        }
        return (cards, tags.mapValues { $0.filter { !$0.tags.isEmpty } })
    }
}
