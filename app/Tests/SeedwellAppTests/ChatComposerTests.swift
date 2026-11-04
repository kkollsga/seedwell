import AppKit
import SwiftUI
import Testing
@testable import SeedwellApp

/// The composer's field (`ChatComposerField`, an AppKit text view): images or files dropped onto its text leave a
/// reference at the character the drop lands on, a paste at the selection, none in an empty field; taking an attachment
/// out renumbers them; Return sends and Escape cancels a reply. The drops are made by calling the text view's drag
/// destination methods with a drag that stands for AppKit's (no synthetic events); the core says where references go
/// (`chatComposerWithImages`, crates/ffi/src/chat_markdown.rs).
@MainActor @Suite(.timeLimit(.minutes(1))) struct ChatComposerTests {
    /// The conversation's draft and selection, as its `@State` holds them.
    @MainActor final class Draft {
        var text: String
        var selection = NSRange(location: 0, length: 0)
        init(_ text: String) { self.text = text }
        var binding: Binding<String> { Binding(get: { self.text }, set: { self.text = $0 }) }
        var selectionBinding: Binding<NSRange> { Binding(get: { self.selection }, set: { self.selection = $0 }) }
    }

    /// A composer as the conversation wires it: the field's text view in a window (never shown), the attachments, and
    /// the draft their references go into.
    @MainActor struct Composer {
        let view: ChatComposerTextView
        let window: NSWindow
        let images: ChatComposerImages
        let draft: Draft

        /// The window point over the left part of the character at `index`: a drop there lands just before it.
        func point(before index: Int) -> NSPoint {
            let layout = view.layoutManager!
            let glyph = layout.glyphIndexForCharacter(at: index)
            let r = layout.boundingRect(forGlyphRange: NSRange(location: glyph, length: 1), in: view.textContainer!)
            let o = view.textContainerOrigin
            return view.convert(NSPoint(x: r.minX + o.x + 1, y: r.midY + o.y), to: nil)
        }

        /// The point after the text's last character.
        var end: NSPoint {
            let layout = view.layoutManager!
            let r = layout.usedRect(for: view.textContainer!)
            let o = view.textContainerOrigin
            return view.convert(NSPoint(x: r.maxX + o.x + 4, y: r.maxY + o.y - 4), to: nil)
        }

        /// Drops the files at `point`, as AppKit does: entered, then perform. What the drop destination said.
        func drop(_ files: [URL], at point: NSPoint) -> (NSDragOperation, Bool) {
            let pb = NSPasteboard(name: NSPasteboard.Name("seedwell-test-\(UUID().uuidString)"))
            defer { pb.releaseGlobally() }
            pb.clearContents()
            pb.writeObjects(files.map { $0 as NSURL })
            let drag = QADrag(pb, at: point, in: window)
            let op = view.draggingEntered(drag)
            let ok = view.prepareForDragOperation(drag) && view.performDragOperation(drag)
            // SwiftUI hands the draft back to the field (`updateNSView`).
            view.replace(with: draft.text)
            view.setSelectedRange(draft.selection)
            return (op, ok)
        }
    }

    static func core(_ name: String) throws -> (Core, String) {
        let dir = NSTemporaryDirectory() + "seedwell-composer-\(name)-\(getpid())"
        try? FileManager.default.removeItem(atPath: dir)
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        return (try Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil)), dir)
    }

    /// The same, made off the main actor: a test that lays out windows keeps the main actor free for the suites that
    /// run beside it (their timing waits on it).
    nonisolated static func coreOffMain(_ name: String) async throws -> (Core, String) {
        try await Task.detached {
            let dir = NSTemporaryDirectory() + "seedwell-composer-\(name)-\(getpid())"
            try? FileManager.default.removeItem(atPath: dir)
            try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
            return (try Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil)), dir)
        }.value
    }

    /// The image samples of the layout tests (pictures drawn by the core), made off the main actor.
    nonisolated static func samples() async -> [ChatImageSample] {
        await Task.detached { chatImageLayoutSamples(maxSide: ChatImageLayout.side) }.value
    }

    /// A small PNG file in `dir`.
    static func png(_ name: String, in dir: String) throws -> URL {
        let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 8, pixelsHigh: 6, bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true,
                                   isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        let url = URL(fileURLWithPath: dir).appendingPathComponent(name)
        try rep.representation(using: .png, properties: [:])!.write(to: url)
        return url
    }

    static func composer(_ text: String, core: Core) -> Composer {
        let draft = Draft(text)
        let images = ChatComposerImages()
        images.placed = { first, count, at in
            ChatConversationView.place(images: first..<(first + count), at: at, in: draft.binding, selection: draft.selectionBinding)
        }
        images.removed = { n in draft.text = chatComposerWithoutImage(draft: draft.text, index: UInt32(n)) }
        let view = ChatComposerTextView.make()
        view.frame = NSRect(x: 0, y: 0, width: 420, height: 60)
        let window = NSWindow(contentRect: view.frame, styleMask: .borderless, backing: .buffered, defer: true)
        window.contentView = view
        view.replace(with: text)
        view.addImages = { pb, at in images.paste(from: pb, core: core, at: at) }
        return Composer(view: view, window: window, images: images, draft: draft)
    }

    @Test func aDropOnTheTextLeavesItsReferenceWhereItLandsAndRemovingRenumbers() async throws {
        let (core, dir) = try Self.core("drop")
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let (a, b) = (try Self.png("a.png", in: dir), try Self.png("b.png", in: dir))
        let c = Self.composer("before after", core: core)
        // Onto the "a" of "after": the reference goes just before it, kept apart by a space; the cursor after it.
        let (op, ok) = c.drop([a], at: c.point(before: 7))
        #expect(op == .copy && ok)
        #expect(c.draft.text == "before ![Image 1](att:1) after")
        #expect(c.draft.selection == NSRange(location: 25, length: 0))
        #expect(c.view.string == c.draft.text)
        // A second drop, at the end: the next number, wherever it lands.
        #expect(c.drop([b], at: c.end).1)
        #expect(c.draft.text == "before ![Image 1](att:1) after ![Image 2](att:2)")
        await c.images.settled()
        #expect(c.images.items.map(\.input.name) == ["a.png", "b.png"])
        // The first attachment taken out: its reference goes and the second's is renumbered where it stands.
        c.images.remove(c.images.items[0].id)
        #expect(c.draft.text == "before  after ![Image 1](att:1)")
    }

    @Test func aDropOnAnEmptyFieldOrAPasteOfTextLeavesNoReference() async throws {
        let (core, dir) = try Self.core("empty")
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let a = try Self.png("a.png", in: dir)
        let empty = Self.composer("", core: core)
        #expect(empty.drop([a], at: empty.view.convert(NSPoint(x: 10, y: 15), to: nil)).1, "attached")
        #expect(empty.draft.text == "", "an image alone is no reference")
        await empty.images.settled()
        #expect(empty.images.items.count == 1)
        // Where images cannot be added, a drop of files is refused (never their paths as text).
        let off = Self.composer("text", core: core)
        off.view.addImages = nil
        #expect(off.drop([a], at: off.point(before: 1)) == ([], false))
        #expect(off.view.string == "text")
    }

    @Test func aPasteOfImagesReplacesTheSelectionWithTheirReferences() async throws {
        let (core, dir) = try Self.core("paste")
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let c = Self.composer("hello world", core: core)
        let pb = NSPasteboard(name: NSPasteboard.Name("seedwell-test-\(UUID().uuidString)"))
        defer { pb.releaseGlobally() }
        pb.clearContents()
        pb.writeObjects([try Self.png("a.png", in: dir) as NSURL, try Self.png("b.png", in: dir) as NSURL])
        c.view.pasteboard = pb
        c.view.setSelectedRange(NSRange(location: 6, length: 5))
        c.view.paste(nil)
        #expect(c.draft.text == "hello ![Image 1](att:1) ![Image 2](att:2)")
        await c.images.settled()
        #expect(c.images.items.count == 2)
        // Text on the pasteboard is the field's own paste, from the same pasteboard.
        pb.clearContents()
        pb.setString("!", forType: .string)
        #expect(c.view.addImages?(pb, NSRange(location: 0, length: 0)) == false)
        c.view.setSelectedRange(NSRange(location: 0, length: 0))
        c.view.paste(nil)
        #expect(c.view.string.hasPrefix("!hello "))
        #expect(c.images.items.count == 2)
    }

    @Test func returnSendsShiftReturnStartsALineAndEscapeIsTheReplys() {
        let newline = #selector(NSResponder.insertNewline(_:))
        #expect(ChatComposerTextView.command(newline, flags: []) == .send)
        #expect(ChatComposerTextView.command(newline, flags: .capsLock) == .send)
        #expect(ChatComposerTextView.command(newline, flags: .shift) == .newLine)
        #expect(ChatComposerTextView.command(#selector(NSResponder.insertNewlineIgnoringFieldEditor(_:)), flags: .option) == .newLine)
        #expect(ChatComposerTextView.command(#selector(NSResponder.cancelOperation(_:)), flags: []) == .escape)
        #expect(ChatComposerTextView.command(#selector(NSResponder.insertTab(_:)), flags: []) == nil)
    }

    /// The field grows with its text from one line to eight, then scrolls; the placeholder shows while it is empty.
    @Test func theFieldIsOneToEightLinesHigh() {
        let view = ChatComposerTextView.make()
        let one = view.height(forWidth: 300, lines: ChatComposerField.lines)
        view.replace(with: "a\nb\nc")
        let three = view.height(forWidth: 300, lines: ChatComposerField.lines)
        view.replace(with: Array(repeating: "line", count: 20).joined(separator: "\n"))
        let many = view.height(forWidth: 300, lines: ChatComposerField.lines)
        #expect(one >= 28 && one <= 34, "one line and the insets, as the field was: \(one)")
        #expect(three > one * 2 - 14 && many > three && many < one * 8, "\(one) \(three) \(many)")
    }
}
