import Foundation
import SwiftUI

/// A text message as Markdown (SPEC 7G.7): the blocks people and agents write in chat (headings, paragraphs, lists,
/// fenced code, quotes, rules), each with its inline Markdown (bold, italic, `code`, links, strikethrough). Lines
/// inside a paragraph keep their breaks, as in a chat. Anything else is plain text.
enum ChatMarkdown {
    enum Block: Equatable {
        case heading(level: Int, text: String)
        case paragraph(String)
        /// `language` is the fence's info word ("swift"), empty when there is none.
        case code(language: String, text: String)
        /// `marker` is "•", a task's "☐" or "☑", or the item's number with its dot ("3."); `depth` 0 for the outer list.
        case item(marker: String, depth: Int, text: String)
        case quote(String)
        case rule
        /// A pipe table: its header cells, then its rows (each padded or cut to the header's width).
        case table(header: [String], rows: [[String]])
    }

    static func blocks(_ text: String) -> [Block] {
        var out: [Block] = []
        var paragraph: [String] = []
        var quote: [String] = []
        func flush() {
            if !paragraph.isEmpty { out.append(.paragraph(paragraph.joined(separator: "\n"))); paragraph = [] }
            if !quote.isEmpty { out.append(.quote(quote.joined(separator: "\n"))); quote = [] }
        }
        let lines = text.replacingOccurrences(of: "\r\n", with: "\n").components(separatedBy: "\n")
        var i = 0
        while i < lines.count {
            let line = lines[i]
            let t = line.trimmingCharacters(in: .whitespaces)
            // A fenced code block runs to its closing fence (or the end of the message).
            if let fence = fence(t) {
                flush()
                var body: [String] = []
                i += 1
                while i < lines.count, !lines[i].trimmingCharacters(in: .whitespaces).hasPrefix(fence.mark) {
                    body.append(lines[i])
                    i += 1
                }
                out.append(.code(language: fence.language, text: body.joined(separator: "\n")))
                i += 1
                continue
            }
            // A table: a row of cells, a line of dashes under it, then rows while lines start with a pipe.
            if t.hasPrefix("|"), i + 1 < lines.count, isTableRule(lines[i + 1]) {
                flush()
                let header = cells(t)
                var rows: [[String]] = []
                i += 2
                while i < lines.count, lines[i].trimmingCharacters(in: .whitespaces).hasPrefix("|") {
                    let r = cells(lines[i].trimmingCharacters(in: .whitespaces))
                    rows.append(Array((r + Array(repeating: "", count: max(0, header.count - r.count))).prefix(header.count)))
                    i += 1
                }
                out.append(.table(header: header, rows: rows))
                continue
            }
            if t.isEmpty {
                flush()
            } else if let h = heading(t) {
                flush()
                out.append(h)
            } else if isRule(t) {
                flush()
                out.append(.rule)
            } else if t.hasPrefix(">") {
                if !paragraph.isEmpty { out.append(.paragraph(paragraph.joined(separator: "\n"))); paragraph = [] }
                quote.append(String(t.dropFirst()).trimmingCharacters(in: .whitespaces))
            } else if let item = item(line) {
                flush()
                out.append(item)
            } else if case .item(let marker, let depth, let text)? = out.last, paragraph.isEmpty, quote.isEmpty,
                      line.hasPrefix("  ") || line.hasPrefix("\t") {
                // An indented line continues the list item above it.
                out[out.count - 1] = .item(marker: marker, depth: depth, text: text + "\n" + t)
            } else {
                if !quote.isEmpty { out.append(.quote(quote.joined(separator: "\n"))); quote = [] }
                paragraph.append(t)
            }
            i += 1
        }
        flush()
        return out
    }

    private static func fence(_ t: String) -> (mark: String, language: String)? {
        for mark in ["```", "~~~"] where t.hasPrefix(mark) {
            let info = t.dropFirst(3).trimmingCharacters(in: .whitespaces)
            return (mark, String(info.split(separator: " ").first ?? ""))
        }
        return nil
    }

    private static func heading(_ t: String) -> Block? {
        let hashes = t.prefix { $0 == "#" }.count
        guard (1...6).contains(hashes) else { return nil }
        let rest = t.dropFirst(hashes)
        guard rest.first == " " else { return nil }
        var text = rest.trimmingCharacters(in: .whitespaces)
        // A closing run of #s ("## Plan ##") goes; one that is part of a word ("C#") stays.
        let closing = text.reversed().prefix { $0 == "#" }.count
        if closing > 0, closing == text.count || text.dropLast(closing).last == " " { text = text.dropLast(closing).trimmingCharacters(in: .whitespaces) }
        return .heading(level: hashes, text: text)
    }

    /// `|---|:--:|`: dashes (and alignment colons) between pipes.
    private static func isTableRule(_ line: String) -> Bool {
        let t = line.trimmingCharacters(in: .whitespaces)
        guard t.contains("-"), t.contains("|") else { return false }
        return t.allSatisfy { "|-: ".contains($0) }
    }

    /// The cells of a table row: split at pipes (not `\|`), the outer pipes dropped, each trimmed.
    private static func cells(_ row: String) -> [String] {
        var out: [String] = [], cell = "", escaped = false
        for c in row {
            // Only `\|` is the table's: other escapes stay for the inline Markdown.
            if escaped { if c != "|" { cell.append("\\") }; cell.append(c); escaped = false } else if c == "\\" { escaped = true } else if c == "|" { out.append(cell); cell = "" } else { cell.append(c) }
        }
        out.append(escaped ? cell + "\\" : cell)
        if out.first?.trimmingCharacters(in: .whitespaces).isEmpty == true { out.removeFirst() }
        if out.last?.trimmingCharacters(in: .whitespaces).isEmpty == true { out.removeLast() }
        return out.map { $0.trimmingCharacters(in: .whitespaces) }
    }

    private static func isRule(_ t: String) -> Bool {
        let s = t.filter { $0 != " " }
        guard s.count >= 3, let c = s.first, "-*_".contains(c) else { return false }
        return s.allSatisfy { $0 == c }
    }

    /// `- x`, `* x`, `+ x`, `1. x`, `1) x`; two spaces (or a tab) of indent per level.
    private static func item(_ line: String) -> Block? {
        let indent = line.prefix { $0 == " " || $0 == "\t" }.reduce(0) { $0 + ($1 == "\t" ? 4 : 1) }
        let t = line.dropFirst(line.prefix { $0 == " " || $0 == "\t" }.count)
        let depth = min(indent / 2, 3)
        if let c = t.first, "-*+".contains(c), t.dropFirst().first == " " {
            let text = t.dropFirst(2).trimmingCharacters(in: .whitespaces)
            guard !text.isEmpty else { return nil }
            // A task: `- [ ] todo`, `- [x] done`.
            for (box, marker) in [("[ ] ", "☐"), ("[x] ", "☑"), ("[X] ", "☑")] where text.hasPrefix(box) {
                return .item(marker: marker, depth: depth, text: String(text.dropFirst(box.count)))
            }
            return .item(marker: "•", depth: depth, text: text)
        }
        let digits = t.prefix { $0.isASCII && $0.isNumber }
        guard (1...9).contains(digits.count) else { return nil }
        let after = t.dropFirst(digits.count)
        let text = after.dropFirst(2).trimmingCharacters(in: .whitespaces)
        guard let d = after.first, d == "." || d == ")", after.dropFirst().first == " ", !text.isEmpty else { return nil }
        return .item(marker: "\(Int(digits) ?? 1).", depth: depth, text: text)
    }

    /// Inline Markdown of one block; the text as it is when it does not parse.
    static func inline(_ text: String) -> AttributedString {
        (try? AttributedString(markdown: text, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace))) ?? AttributedString(text)
    }

    /// The message without its Markdown, one block per line (for previews, notifications and replies).
    static func plain(_ text: String) -> String {
        blocks(text).map { b in
            switch b {
            case .heading(_, let t), .paragraph(let t), .quote(let t): String(inline(t).characters)
            case .item(let marker, _, let t): "\(marker) \(String(inline(t).characters))"
            case .code(_, let t): t
            case .table(let header, let rows): ([header] + rows).map { $0.map { String(inline($0).characters) }.joined(separator: " · ") }.joined(separator: "\n")
            case .rule: ""
            }
        }
        .filter { !$0.isEmpty }
        .joined(separator: "\n")
    }
}

/// A text message's blocks, laid out like a short document.
struct ChatMarkdownView: View {
    let text: String

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(Array(ChatMarkdown.blocks(text).enumerated()), id: \.offset) { _, b in block(b) }
        }
        .textSelection(.enabled)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    @ViewBuilder private func block(_ b: ChatMarkdown.Block) -> some View {
        switch b {
        case .heading(let level, let t):
            Text(ChatMarkdown.inline(t))
                .font(.system(size: level == 1 ? 16 : level == 2 ? 14.5 : 13.5, weight: .semibold))
                .padding(.top, 4)
        case .paragraph(let t):
            Text(ChatMarkdown.inline(t)).font(.system(size: 13)).fixedSize(horizontal: false, vertical: true)
        case .item(let marker, let depth, let t):
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(marker).monospacedDigit().foregroundStyle(.secondary)
                    .frame(minWidth: marker.hasSuffix(".") ? 18 : 10, alignment: .trailing)
                Text(ChatMarkdown.inline(t)).fixedSize(horizontal: false, vertical: true)
            }
            .font(.system(size: 13))
            .padding(.leading, CGFloat(depth) * 18)
        case .code(let language, let t):
            VStack(alignment: .leading, spacing: 4) {
                if !language.isEmpty {
                    Text(language).font(.system(size: 10.5, weight: .medium)).foregroundStyle(.secondary)
                }
                ScrollView(.horizontal, showsIndicators: false) {
                    Text(t).font(.system(size: 12, design: .monospaced)).fixedSize(horizontal: true, vertical: true)
                }
            }
            .padding(.horizontal, 10).padding(.vertical, 8)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(RoundedRectangle(cornerRadius: 6).fill(Color.secondary.opacity(0.14)))
        case .quote(let t):
            HStack(alignment: .top, spacing: 8) {
                RoundedRectangle(cornerRadius: 1.5).fill(Color.secondary.opacity(0.35)).frame(width: 3)
                Text(ChatMarkdown.inline(t)).font(.system(size: 13)).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            // The bar is as tall as the text: a shape alone takes all the height it is offered.
            .fixedSize(horizontal: false, vertical: true)
        case .rule:
            Divider().padding(.vertical, 2)
        case .table(let header, let rows):
            ScrollView(.horizontal, showsIndicators: false) {
                Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 4) {
                    GridRow { ForEach(Array(header.enumerated()), id: \.offset) { Text(ChatMarkdown.inline($1)).fontWeight(.semibold) } }
                    Divider().gridCellUnsizedAxes(.horizontal)
                    ForEach(Array(rows.enumerated()), id: \.offset) { _, r in
                        GridRow { ForEach(Array(r.enumerated()), id: \.offset) { Text(ChatMarkdown.inline($1)) } }
                    }
                }
                .font(.system(size: 12.5))
                .padding(.horizontal, 10).padding(.vertical, 8)
            }
            .background(RoundedRectangle(cornerRadius: 6).strokeBorder(Color.secondary.opacity(0.25)))
        }
    }
}
