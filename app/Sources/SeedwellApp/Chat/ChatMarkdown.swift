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
}

/// A text message's blocks, laid out like a short document.
struct ChatMarkdownView: View {
    let blocks: [ChatBlock]

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(Array(blocks.enumerated()), id: \.offset) { _, b in block(b) }
        }
        .textSelection(.enabled)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func text(_ runs: [ChatRun]) -> Text { Text(ChatRuns.attributed(runs)) }

    @ViewBuilder private func block(_ b: ChatBlock) -> some View {
        switch b {
        case .heading(let level, let runs):
            text(runs)
                .font(.system(size: level == 1 ? 16 : level == 2 ? 14.5 : 13.5, weight: .semibold))
                .padding(.top, 4)
        case .paragraph(let runs):
            text(runs).font(.system(size: 13)).fixedSize(horizontal: false, vertical: true)
        case .item(let marker, let numbered, let depth, let runs):
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(marker).monospacedDigit().foregroundStyle(.secondary)
                    .frame(minWidth: numbered ? 18 : 10, alignment: .trailing)
                text(runs).fixedSize(horizontal: false, vertical: true)
            }
            .font(.system(size: 13))
            .padding(.leading, CGFloat(depth) * 18)
        case .codeBlock(let language, let code):
            VStack(alignment: .leading, spacing: 4) {
                if !language.isEmpty {
                    Text(language).font(.system(size: 10.5, weight: .medium)).foregroundStyle(.secondary)
                }
                ScrollView(.horizontal, showsIndicators: false) {
                    Text(code).font(.system(size: 12, design: .monospaced)).fixedSize(horizontal: true, vertical: true)
                }
            }
            .padding(.horizontal, 10).padding(.vertical, 8)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(RoundedRectangle(cornerRadius: 6).fill(Color.secondary.opacity(0.14)))
        case .quote(let runs):
            HStack(alignment: .top, spacing: 8) {
                RoundedRectangle(cornerRadius: 1.5).fill(Color.secondary.opacity(0.35)).frame(width: 3)
                text(runs).font(.system(size: 13)).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            // The bar is as tall as the text: a shape alone takes all the height it is offered.
            .fixedSize(horizontal: false, vertical: true)
        case .rule:
            Divider().padding(.vertical, 2)
        case .table(let header, let rows):
            ScrollView(.horizontal, showsIndicators: false) {
                Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 4) {
                    GridRow { ForEach(Array(header.enumerated()), id: \.offset) { text($1.runs).fontWeight(.semibold) } }
                    Divider().gridCellUnsizedAxes(.horizontal)
                    ForEach(Array(rows.enumerated()), id: \.offset) { _, r in
                        GridRow { ForEach(Array(r.cells.enumerated()), id: \.offset) { text($1.runs) } }
                    }
                }
                .font(.system(size: 12.5))
                .padding(.horizontal, 10).padding(.vertical, 8)
            }
            .background(RoundedRectangle(cornerRadius: 6).strokeBorder(Color.secondary.opacity(0.25)))
        }
    }
}
