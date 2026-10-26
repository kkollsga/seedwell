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

    var isStatus: Bool { type?.lowercased() == "status" }
    /// Bookkeeping, shown as a small line: "seen" (a recipient saw a mail), "hook" (a project's mail hook changed).
    var isNote: Bool { ["seen", "hook"].contains(type?.lowercased() ?? "") }

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
                        body: String(after).trimmingCharacters(in: .whitespacesAndNewlines))
    }

    /// "Bug", "Heads-up", "Request" for the chip.
    var typeLabel: String? {
        guard let t = type?.trimmingCharacters(in: .whitespaces), !t.isEmpty else { return nil }
        return t.prefix(1).uppercased() + t.dropFirst()
    }

    /// One line for previews, banners and reply quotes: "Bug: Recipe tool returns the wrong catalogue".
    var line: String {
        if isNote { return type?.lowercased() == "seen" ? "Seen" : (summary ?? "Mail hook") }
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
