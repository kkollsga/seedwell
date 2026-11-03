import AppKit

/// QA only: what a message of the conversation shown can be asked to do, as its row does it (its pointer state, its
/// gallery, its image references). The rows register here while they are shown, in a QA-control run only (`enabled`);
/// `qa chat …` calls these, which call what the row's pointer, click or key calls.
@MainActor enum ChatQAHooks {
    struct Row {
        /// The pointer came onto the message (true) or left it: its time, Tag and Reply show or hide.
        let hover: (Bool) -> Void
        let hovered: () -> Bool
        /// The gallery's image shown (nil: the first, never chosen) and how many it has.
        let gallery: () -> (selection: Int?, count: Int)
        /// A filmstrip thumbnail clicked: that image is shown.
        let show: (Int) -> Void
        /// A click on the n-th image reference (a chip) in the text: the link the chip opens.
        let reference: (Int) -> Void
    }

    /// Rows register (a QA-control run sets it at start; tests that drive a row set it too).
    static var enabled = false
    static var rows: [String: Row] = [:]
}

/// QA's `chat …` commands (QAControl): the composer and the conversation shown, through the calls the keys and
/// clicks make. No event is posted: a key is the command the text system makes of it (`perform`), typing and an input
/// method's composition are the text input calls an input method makes, a paste reads a pasteboard of QA's own, a
/// drop is the drag-destination calls with a drag at the point the layout gives (`QADrag`).
@MainActor enum QAChat {
    static let usage = "chat open <channel> | messages | type <text> | compose <text>|end | select <start> [length] | key enter|shift-enter|option-enter|escape | "
        + "reply <id>|cancel | paste text <text>|png <file>|file <file> | drop <file> at <index>|chips | state | hover <id> [off] | "
        + "gallery <id> next|prev|<n>|state | chip <id> <n> | copy-code <id> [n]"

    static func run(_ arg: String, app: AppDelegate, commands: Commands) async -> String {
        let words = arg.split(separator: " ", maxSplits: 1).map(String.init)
        let rest = words.count > 1 ? words[1] : ""
        let model = app.mainWindowController?.libraryModel
        switch words.first ?? "" {
        case "open":
            guard let c = commands.core.chatChannels().first(where: { $0.id == rest || $0.name.localizedCaseInsensitiveCompare(rest) == .orderedSame })
            else { return "chat: no channel \(rest)" }
            commands.showMain(.chat(channelId: c.id, topicId: nil), nil)
            app.mainWindowController?.libraryModel?.loadChatMessages()
            return "chat: open \(c.name)"
        case "messages":
            // The conversation's messages as shown, oldest first: "<id> <who> <first words>", one per line.
            guard let page = model?.chatPage else { return "chat: no conversation" }
            return (["chat: messages \(page.messages.count)"] + page.messages.map { "\($0.id) \($0.who) \($0.replying.text.prefix(40))" })
                .joined(separator: "\n")
        case "reply":
            guard let m = model else { return "chat: no window" }
            if rest == "cancel" { return "chat: reply \(m.cancelReply() ? "cancelled" : "none")" }
            guard let card = m.chatPage?.messages.first(where: { $0.id == rest }) else { return "chat: no message \(rest)" }
            m.reply(to: card)
            return "chat: reply \(card.replying.title) · \(card.replying.text)"
        case "hover", "gallery", "chip", "copy-code":
            return message(words[0], rest, model: model)
        default:
            break
        }
        guard let field = composer(app) else { return "chat: no composer (open a conversation that can post)" }
        switch words.first ?? "" {
        case "type":
            field.insertText(rest, replacementRange: NSRange(location: NSNotFound, length: 0))
        case "compose":
            // An input method's composition (marked text), as Japanese or Chinese input makes it; `end` commits it.
            if rest == "end" { field.unmarkText() } else {
                field.setMarkedText(rest, selectedRange: NSRange(location: (rest as NSString).length, length: 0),
                                    replacementRange: NSRange(location: NSNotFound, length: 0))
            }
        case "select":
            let n = rest.split(separator: " ").compactMap { Int($0) }
            let length = (field.string as NSString).length
            guard let start = n.first, start >= 0, start + (n.count > 1 ? n[1] : 0) <= length else { return "chat: select <start> [length] within \(length)" }
            field.setSelectedRange(NSRange(location: start, length: n.count > 1 ? n[1] : 0))
        case "key":
            let keys: [String: (Selector, NSEvent.ModifierFlags)] = [
                "enter": (#selector(NSResponder.insertNewline(_:)), []),
                "shift-enter": (#selector(NSResponder.insertNewline(_:)), .shift),
                "option-enter": (#selector(NSResponder.insertNewlineIgnoringFieldEditor(_:)), .option),
                "escape": (#selector(NSResponder.cancelOperation(_:)), []),
            ]
            guard let (selector, flags) = keys[rest] else { return "chat: key enter|shift-enter|option-enter|escape" }
            field.perform(selector, flags: flags)
        case "paste":
            let p = rest.split(separator: " ", maxSplits: 1).map(String.init)
            guard p.count == 2 else { return "chat: paste text <text>|png <file>|file <file>" }
            let pb = QADrag.pasteboard()
            defer { pb.releaseGlobally() }
            switch p[0] {
            case "text": pb.setString(p[1], forType: .string)
            case "png", "file":
                guard qaFile(p[1]) else { return "chat: paste: only files under /private/tmp" }
                if p[0] == "png" {
                    guard let data = FileManager.default.contents(atPath: p[1]) else { return "chat: paste: cannot read \(p[1])" }
                    pb.setData(data, forType: .png)
                } else {
                    pb.writeObjects([URL(fileURLWithPath: p[1]) as NSURL])
                }
            default: return "chat: paste text|png|file"
            }
            let system = field.pasteboard
            field.pasteboard = pb
            field.paste(nil)
            field.pasteboard = system
        case "drop":
            // `drop <file> at <index>`: onto the text, before the character at that UTF-16 index (its end: the length);
            // `drop <file> chips`: onto the conversation outside the text (the chips), which attaches without a reference.
            guard let r = rest.range(of: " at ", options: .backwards) ?? rest.range(of: " chips", options: .backwards) else {
                return "chat: drop <file> at <index> | drop <file> chips"
            }
            let path = String(rest[..<r.lowerBound])
            guard qaFile(path), FileManager.default.fileExists(atPath: path) else { return "chat: drop: no file under /private/tmp at \(path)" }
            if rest[r] == " chips" {
                guard let images = field.images, let item = NSItemProvider(contentsOf: URL(fileURLWithPath: path)) else { return "chat: drop: nothing takes it" }
                return "chat: drop chips \(images.drop([item], core: commands.core) ? "taken" : "refused")"
            }
            guard let index = Int(rest[r.upperBound...]), let point = field.dropPoint(before: index) else {
                return "chat: drop: no character \(rest[r.upperBound...]) (length \((field.string as NSString).length))"
            }
            let pb = QADrag.pasteboard()
            defer { pb.releaseGlobally() }
            pb.writeObjects([URL(fileURLWithPath: path) as NSURL])
            let lands = field.dropIndex(QADrag(pb, at: point, in: field.window))
            let (op, ok) = field.drop(pb, at: point)
            await settle(field)
            return "chat: drop at \(lands) \(ok ? "taken" : "refused") (\(op == .copy ? "copy" : "none"))"
        case "state":
            break
        default:
            return "chat: \(usage)"
        }
        await settle(field)
        return state(field, model: model)
    }

    /// Lets the conversation hand its draft back to the field (a reference a paste or a drop put in goes through the
    /// draft, and the field shows it at SwiftUI's next update) and the field's height follow.
    private static func settle(_ field: NSView) async {
        for _ in 0..<3 {
            try? await Task.sleep(for: .milliseconds(20))
            field.window?.contentView?.layoutSubtreeIfNeeded()
        }
    }

    /// `chat: text "…" · caret 4+0 · markers 1@6+17 · attachments a.png · reply <id>|none · focus yes · placeholder no ·
    /// composing no · dropping no · height 2 lines`.
    static func state(_ field: ChatComposerTextView, model: LibraryModel?) -> String {
        let text = field.string.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\n", with: "\\n")
        let sel = field.selectedRange()
        let markers = chatComposerImageRefs(draft: field.string).enumerated().map { "\($0 + 1)@\($1.start)+\($1.length)" }
        let names = field.images?.items.map(\.input.name) ?? []
        let line = (field.layoutManager?.defaultLineHeight(for: field.font ?? Theme.NS.body()) ?? 16) + Theme.Line.secondary
        let lines = Int(((field.enclosingScrollView?.frame.height ?? 0) - ChatComposerField.inset.height * 2) / line + 0.5)
        return "chat: text \"\(text)\" · caret \(sel.location)+\(sel.length) · markers \(markers.isEmpty ? "none" : markers.joined(separator: ","))"
            + " · attachments \(names.isEmpty ? "none" : names.joined(separator: ","))"
            + " · reply \(model?.chatReply?.id ?? "none") · focus \(field.window?.firstResponder === field ? "yes" : "no")"
            + " · placeholder \(field.showsPlaceholder ? "yes" : "no") · composing \(field.hasMarkedText() ? "yes" : "no")"
            + " · dropping \(field.images?.dropping == true ? "yes" : "no") · height \(lines) lines"
    }

    /// A message's own controls: its pointer state, its gallery, its image references, its code's copy button.
    private static func message(_ what: String, _ arg: String, model: LibraryModel?) -> String {
        let p = arg.split(separator: " ").map(String.init)
        guard let id = p.first, let card = model?.chatPage?.messages.first(where: { $0.id == id }) else { return "chat: no message \(arg)" }
        let row = ChatQAHooks.rows[id]
        switch what {
        case "hover":
            guard let row else { return "chat: message \(id) is not shown" }
            row.hover(p.count < 2 || p[1] != "off")
            return "chat: hover \(id) \(row.hovered() ? "on" : "off") · reply \(card.reply?.title ?? "none") · tag \(card.tagMenu == nil ? "no" : "yes")"
        case "gallery":
            guard let row else { return "chat: message \(id) is not shown" }
            let (selection, count) = row.gallery()
            guard card.gallery, count > 1 else { return "chat: message \(id) has no gallery" }
            let active = ChatGalleryView.active(selection: selection, count: count)
            switch p.count > 1 ? p[1] : "state" {
            case "next", "right": if let n = ChatGalleryView.step(active, by: 1, count: count) { row.show(n) }
            case "prev", "left": if let n = ChatGalleryView.step(active, by: -1, count: count) { row.show(n) }
            case "state": break
            case let n: guard let i = Int(n), (1...count).contains(i) else { return "chat: gallery <id> next|prev|<1…\(count)>|state" }
                row.show(i - 1)
            }
            let now = ChatGalleryView.active(selection: row.gallery().selection, count: count)
            return "chat: gallery \(id) \(chatGalleryCounter(active: UInt32(now + 1), count: UInt32(count))) · \(card.images[now].caption)"
        case "chip":
            guard let row else { return "chat: message \(id) is not shown" }
            guard p.count > 1, let n = Int(p[1]), n >= 1, n <= card.images.count else { return "chat: chip <id> <1…\(card.images.count)>" }
            row.reference(n - 1)
            let shown = ChatGalleryView.active(selection: row.gallery().selection, count: card.images.count)
            return "chat: chip \(id) shows \(shown + 1) / \(card.images.count)"
        default:
            // `copy-code <id> [n]`: the n-th code block's copy button (1 first), into QA's own pasteboard.
            let blocks: [ChatBlock] = switch card.body {
            case .markdown(let b), .mail(_, let b): b
            default: []
            }
            let code = blocks.compactMap { b -> String? in if case .codeBlock(_, let text) = b { text } else { nil } }
            let n = p.count > 1 ? Int(p[1]) ?? 1 : 1
            guard n >= 1, n <= code.count else { return "chat: message \(id) has \(code.count) code block(s)" }
            ChatCodeBlock.copy(code[n - 1])
            let copied = ChatCodeBlock.pasteboard.string(forType: .string) ?? ""
            return "chat: copied \(copied == code[n - 1] ? "exactly" : "differently") \((copied as NSString).length) characters: "
                + copied.replacingOccurrences(of: "\n", with: "\\n")
        }
    }

    /// The composer's text view in the main window.
    static func composer(_ app: AppDelegate) -> ChatComposerTextView? {
        func find(_ v: NSView) -> ChatComposerTextView? { (v as? ChatComposerTextView) ?? v.subviews.lazy.compactMap(find).first }
        return app.mainWindowController?.nsWindow?.contentView.flatMap(find)
    }

    /// QA reads and drops only files of its own root.
    private static func qaFile(_ path: String) -> Bool { ["/private/tmp/", "/tmp/"].contains(where: path.hasPrefix) && !path.contains("/../") }
}
