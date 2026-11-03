import AppKit
import Foundation
import SwiftUI

/// A text message's inline Markdown as the core reads it (`chatBlocks`, crates/ffi/src/chat_markdown.rs): runs of text
/// with their styles and links. Drawing only.
enum ChatRuns {
    /// Runs as an attributed string with the attributes Foundation's Markdown gives (presentation intents and links),
    /// so SwiftUI's `Text` draws them as it drew parsed Markdown. A link opens as any link in the app does: a web or
    /// mail address in its app, a `seedwell://` link through the app's own link handling (`Core.routeLink`).
    static func attributed(_ runs: [ChatRun]) -> AttributedString {
        var out = AttributedString()
        for r in runs {
            var a = AttributedString(r.text)
            var intent: InlinePresentationIntent = []
            if r.bold { intent.insert(.stronglyEmphasized) }
            if r.italic { intent.insert(.emphasized) }
            if r.strikethrough { intent.insert(.strikethrough) }
            if r.code { intent.insert(.code) }
            if !intent.isEmpty { a.inlinePresentationIntent = intent }
            if let l = r.link, let url = URL(string: l.url) { a.link = url }
            out += a
        }
        return out
    }

    /// The scheme of an image reference's link (`x-seedwell-image:1` for the message's second image): the message
    /// handles it (its gallery shows that image), never the system.
    static let imageScheme = "x-seedwell-image"

    /// Runs as text, an image reference (`ChatRun.image`) as a chip where it stands: a photo symbol and its description
    /// in the accent colour, a link the message handles (`imageScheme`).
    static func text(_ runs: [ChatRun], onAccent: Bool = false) -> Text {
        var out = Text("")
        var plain: [ChatRun] = []
        func flush() {
            if !plain.isEmpty { out = out + Text(attributed(plain)) }
            plain = []
        }
        for r in runs {
            guard let n = r.image, let url = URL(string: "\(imageScheme):\(n)") else { plain.append(r); continue }
            flush()
            var chip = AttributedString("\u{2009}" + r.text)
            chip.link = url
            chip.foregroundColor = onAccent ? .white : .accentColor
            if onAccent { chip.underlineStyle = .single }
            chip.inlinePresentationIntent = r.bold ? .stronglyEmphasized : nil
            out = out + Text(Image(systemName: "photo")).foregroundStyle(onAccent ? Color.white : Color.accentColor) + Text(chip)
        }
        flush()
        return out
    }
}

/// A fenced code block: its language (quiet), the code (monospaced, scrolling sideways), and under the pointer (or
/// with the keyboard's focus) a copy button in its top-right corner, over it: it copies the code exactly, without the
/// language, and shows a check mark for a moment.
struct ChatCodeBlock: View {
    let language: String
    let code: String
    let fill: Bool
    let wash: Color
    let quiet: Color
    @State private var hovered = false
    @State private var copied = false
    @FocusState private var focused: Bool
    @Environment(\.chatCodeHover) private var forceHover

    /// The button's tooltip and name, the core's.
    nonisolated static let copyHelp = chatCodeCopyHelp()

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            if !language.isEmpty {
                Text(language).font(Theme.micro).foregroundStyle(quiet)
            }
            ScrollView(.horizontal, showsIndicators: false) {
                Text(code).font(Theme.mono).lineSpacing(Theme.Line.secondary).fixedSize(horizontal: true, vertical: true)
            }
        }
        .padding(.horizontal, Theme.Space.m).padding(.vertical, Theme.Space.s + 2)
        .frame(maxWidth: fill ? .infinity : nil, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: Theme.Radius.medium, style: .continuous).fill(wash))
        .overlay(alignment: .topTrailing) {
            Button(action: copy) {
                Image(systemName: copied ? "checkmark" : "doc.on.doc")
                    .font(.system(size: Theme.Icon.small + 1))
                    .foregroundStyle(copied ? AnyShapeStyle(Color.accentColor) : AnyShapeStyle(.secondary))
                    .frame(width: 26, height: 24)
                    .background(.bar, in: RoundedRectangle(cornerRadius: Theme.Radius.small, style: .continuous))
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .focused($focused)
            .help(Self.copyHelp)
            .accessibilityLabel(Self.copyHelp)
            .padding(Theme.Space.xs + 2)
            .opacity(hovered || focused || copied || forceHover ? 1 : 0)
        }
        .onHover { hovered = $0 }
    }

    /// Where Copy puts the code: the system's pasteboard; in a QA run its own (`chat copy-code` reads it), never the
    /// user's.
    @MainActor static let pasteboard: NSPasteboard = QuietRun.on ? NSPasteboard(name: NSPasteboard.Name("seedwell-qa-code")) : .general

    /// The code exactly as written (no language, nothing added) on `pb`, as plain text.
    @MainActor static func copy(_ code: String, to pb: NSPasteboard = pasteboard) {
        pb.clearContents()
        pb.setString(code, forType: .string)
    }

    private func copy() {
        Self.copy(code)
        copied = true
        Task { @MainActor in
            try? await Task.sleep(for: .seconds(1.5))
            copied = false
        }
    }
}

/// A text message's blocks, laid out like a short document.
struct ChatMarkdownView: View {
    let blocks: [ChatBlock]
    /// As wide as the column (a channel); else as wide as its text (a bubble).
    var fill = true
    /// On the accent colour (the user's bubble): white text, and code and quotes on a lighter wash.
    var onAccent = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(blocks.enumerated()), id: \.offset) { n, b in
                // A list's items sit close; other blocks a paragraph's space apart.
                block(b).padding(.top, n == 0 ? 0 : Self.isItem(b) && Self.isItem(blocks[n - 1]) ? Theme.Space.xs : Theme.Space.s)
            }
        }
        .textSelection(.enabled)
        .frame(maxWidth: fill ? .infinity : nil, alignment: .leading)
    }

    private static func isItem(_ b: ChatBlock) -> Bool {
        if case .item = b { return true }
        return false
    }

    /// The wash under code and tables.
    private var wash: Color { onAccent ? Color.white.opacity(0.16) : Color.primary.opacity(0.05) }
    private var quiet: Color { onAccent ? Color.white.opacity(0.75) : Color.secondary }

    private func text(_ runs: [ChatRun]) -> Text { ChatRuns.text(runs, onAccent: onAccent) }

    @ViewBuilder private func block(_ b: ChatBlock) -> some View {
        switch b {
        case .heading(let level, let runs):
            text(runs)
                .font(level == 1 ? Theme.heading : Theme.headline)
                .padding(.top, Theme.Space.xs)
        case .paragraph(let runs):
            text(runs).font(Theme.body).lineSpacing(Theme.Line.body).fixedSize(horizontal: false, vertical: true)
        case .item(let marker, let numbered, let depth, let runs):
            HStack(alignment: .firstTextBaseline, spacing: Theme.Space.s - 2) {
                Text(marker).monospacedDigit().foregroundStyle(quiet)
                    .frame(minWidth: numbered ? 18 : 10, alignment: .trailing)
                text(runs).lineSpacing(Theme.Line.body).fixedSize(horizontal: false, vertical: true)
            }
            .font(Theme.body)
            .padding(.leading, CGFloat(depth) * 18)
        case .codeBlock(let language, let code):
            ChatCodeBlock(language: language, code: code, fill: fill, wash: wash, quiet: quiet)
        case .quote(let runs):
            HStack(alignment: .top, spacing: Theme.Space.s) {
                RoundedRectangle(cornerRadius: 1.5).fill(quiet.opacity(0.45)).frame(width: 3)
                text(runs).font(Theme.body).lineSpacing(Theme.Line.body).foregroundStyle(quiet)
                    .fixedSize(horizontal: false, vertical: true)
            }
            // The bar is as tall as the text: a shape alone takes all the height it is offered.
            .fixedSize(horizontal: false, vertical: true)
        case .rule:
            Divider().padding(.vertical, Theme.Space.hair)
        case .table(let header, let rows):
            ScrollView(.horizontal, showsIndicators: false) {
                Grid(alignment: .leading, horizontalSpacing: Theme.Space.l, verticalSpacing: Theme.Space.xs + 1) {
                    GridRow { ForEach(Array(header.enumerated()), id: \.offset) { text($1.runs).fontWeight(.semibold) } }
                    Divider().gridCellUnsizedAxes(.horizontal)
                    ForEach(Array(rows.enumerated()), id: \.offset) { _, r in
                        GridRow { ForEach(Array(r.cells.enumerated()), id: \.offset) { text($1.runs) } }
                    }
                }
                .font(Theme.secondary)
                .padding(.horizontal, Theme.Space.m).padding(.vertical, Theme.Space.s + 2)
            }
            .background(RoundedRectangle(cornerRadius: Theme.Radius.medium, style: .continuous).fill(wash))
        }
    }
}
