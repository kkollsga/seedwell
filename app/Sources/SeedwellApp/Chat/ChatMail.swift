import Foundation
import SwiftUI

/// A mail between projects (SPEC 7G.8): `ChatMail` comes from the core, read by the rules `seedwell mail` uses
/// (`chatMail(text:)`). The app shows the header as a line, never as text.
extension ChatMail {
    /// One line for previews, banners and reply quotes: "Bug: Recipe tool returns the wrong catalogue".
    var line: String {
        if isTag { return "Tagged " + tags.map { chatTagLabel(tag: $0) }.joined(separator: ", ") }
        if isNote { return summary ?? "Mail hook" }
        if isStatus { return state == "open" ? "Progress: \(ChatMarkdown.plain(body))" : "Done: \(ChatMarkdown.plain(body))" }
        let what = summary ?? ChatMarkdown.plain(body)
        return typeLabel.map { "\($0): \(what)" } ?? what
    }

    /// The messages that are cards (a tag note is not one) and the tags on each by its short id (`chatCards`).
    static func cards(_ ms: [SyncMessage]) -> (cards: [SyncMessage], tags: [String: [ChatTagOwner]]) {
        let c = chatCards(messages: ms)
        return (c.cards, Dictionary(uniqueKeysWithValues: c.tags.map { ($0.message, $0.owners) }))
    }
}

/// The header line of a mail on its card: the type, whom it is for, and its summary.
struct ChatMailHeader: View {
    let mail: ChatMail

    var body: some View {
        if mail.isNote {
            Label(mail.line, systemImage: mail.kind == "seen" ? "eye" : "bell")
                .font(.system(size: 11.5)).foregroundStyle(.secondary)
        } else if mail.isStatus {
            Label(mail.state == "open" ? "Progress" : "Done",
                  systemImage: mail.state == "open" ? "clock" : "checkmark.circle.fill")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(mail.state == "open" ? Color.orange : Color.green)
        } else {
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    if let t = mail.typeLabel {
                        Text(t).font(.system(size: 10.5, weight: .semibold))
                            .padding(.horizontal, 6).padding(.vertical, 1.5)
                            .background(Capsule().fill(Self.tint(mail.kind).opacity(0.18)))
                            .foregroundStyle(Self.tint(mail.kind))
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

    static func tint(_ kind: String?) -> Color {
        switch kind {
        case "bug": .red
        case "request", "question": .blue
        case "heads-up": .orange
        case "reply": .teal
        default: .secondary
        }
    }
}
