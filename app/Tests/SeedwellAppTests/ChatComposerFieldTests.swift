import AppKit
import SwiftUI
import Testing
@testable import SeedwellApp

/// The composer's field as the user meets it: the real conversation view (`ChatConversationView`, the mock core's
/// "Release notes" channel) in a window that is never shown, its AppKit text view driven in-process. Keys are `NSEvent`s
/// handed to the view's `keyDown` (nothing is posted to the system), typing and input-method composition are the text
/// input calls an input method makes (`insertText`, `setMarkedText`), pastes read a private pasteboard, and drops call
/// the drag-destination methods with a drag whose location comes from the layout (`QADrag`).
/// The main run loop for `seconds` (SwiftUI's updates, AppKit's layout), called between awaits: a test holds the main
/// actor only that long at a time.
@MainActor func spinRunLoop(_ seconds: Double) {
    RunLoop.main.run(until: Date().addingTimeInterval(seconds))
}

@MainActor @Suite(.serialized, .timeLimit(.minutes(1))) struct ChatComposerFieldTests {
    /// A conversation page in a window, with its model, the core and the field.
    @MainActor final class Page {
        let core: Core
        let dir: String
        let channel: String
        let model: LibraryModel
        let window: NSWindow
        let host: NSView

        init(core: Core, dir: String, height: CGFloat = 640) throws {
            _ = NSApplication.shared
            (self.core, self.dir) = (core, dir)
            channel = try #require(core.chatChannels().first { $0.name == "Release notes" }).id
            model = LibraryModel(core: core, section: .chat(channelId: channel, topicId: nil))
            model.loadChatMessages()
            let actions = ChatActions(core: core, sync: SyncActions(core: core))
            host = NSHostingView(rootView: Host(model: model, actions: actions).frame(width: 720, height: height))
            window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 720, height: height), styleMask: [.titled], backing: .buffered, defer: false)
            window.isReleasedWhenClosed = false
            window.contentView = host
            host.layoutSubtreeIfNeeded()
        }

        deinit { try? FileManager.default.removeItem(atPath: dir) }

        /// A page laid out and settled.
        static func open(_ name: String) async throws -> Page {
            let (core, dir) = try await ChatComposerTests.coreOffMain("field-\(name)")
            let p = try Page(core: core, dir: dir)
            await p.settle()
            return p
        }

        /// The page as the main window shows it: the model's page, laid out again when it changes.
        struct Host: View {
            @Bindable var model: LibraryModel
            let actions: ChatActions
            var body: some View {
                if let page = model.chatPage { ChatConversationView(model: model, page: page, actions: actions) }
            }
        }

        /// The composer's text view.
        var field: ChatComposerTextView {
            func find(_ v: NSView) -> ChatComposerTextView? { (v as? ChatComposerTextView) ?? v.subviews.lazy.compactMap(find).first }
            return find(host)!
        }

        var scroll: NSScrollView { field.enclosingScrollView! }

        /// Lets SwiftUI lay the page out again (the run loop in slices of a few milliseconds, the main actor free in
        /// between: other suites' main-actor work runs meanwhile, a long spin would starve their timing).
        func settle(_ timeout: Double = 0.15) async {
            let end = Date().addingTimeInterval(timeout)
            repeat {
                spinRunLoop(0.005)
                host.layoutSubtreeIfNeeded()
                try? await Task.sleep(for: .milliseconds(2))
            } while Date() < end
        }

        /// Waits until `done` holds, at most `timeout` s, while the main actor runs the core's answers (a send's) and
        /// SwiftUI its updates.
        func until(_ timeout: Double = 5, _ done: () -> Bool) async -> Bool {
            let end = Date().addingTimeInterval(timeout)
            while Date() < end {
                if done() { return true }
                await settle(0.02)
            }
            return done()
        }

        /// A key as the keyboard sends it, handed to the field (`keyDown`, which interprets it as the text system does).
        func key(_ chars: String, code: UInt16, _ flags: NSEvent.ModifierFlags = []) async {
            let e = NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: flags, timestamp: ProcessInfo.processInfo.systemUptime,
                                     windowNumber: window.windowNumber, context: nil, characters: chars, charactersIgnoringModifiers: chars,
                                     isARepeat: false, keyCode: code)!
            // Keys reach the first responder through its window's input context, which is active while it has the keyboard.
            if window.firstResponder !== field { window.makeFirstResponder(field) }
            field.inputContext?.activate()
            field.keyDown(with: e)
            await settle()
        }

        func enter(_ flags: NSEvent.ModifierFlags = []) async { await key("\r", code: 36, flags) }
        func escape() async { await key("\u{1b}", code: 53) }

        /// Typed text, as the keyboard's input method inserts it at the selection.
        func type(_ text: String) async {
            field.insertText(text, replacementRange: NSRange(location: NSNotFound, length: 0))
            await settle()
        }

        // Plain values of the field for the expectations (an expectation never holds a view: printing one, should it
        // fail, would describe the view off the main thread).
        var text: String { field.string }
        var caret: NSRange { field.selectedRange() }
        var composing: Bool { field.hasMarkedText() }
        var marked: NSRange { field.markedRange() }
        var placeholder: Bool { field.showsPlaceholder }
        var focused: Bool { window.firstResponder === field }
        var dropping: Bool { field.images?.dropping == true }
        var attachments: [String] { field.images?.items.map(\.input.name) ?? [] }

        /// The channel's messages as the core has them (the last one is the newest).
        var sent: [SyncMessage] { core.chatMessages(id: channel, topic: nil, before: nil, limit: 200) }

        /// The first message of another person (the mock's Kari).
        var kari: ChatMessageCard { model.chatPage!.messages.first { $0.who == .theirs }! }
    }

    // MARK: Keys

    @Test func enterSendsAndClearsTheField() async throws {
        let p = try await Page.open("enter")
        let before = p.sent.count
        await p.type("Ship it on Friday")
        #expect(p.text == "Ship it on Friday")
        await p.enter()
        #expect(await p.until { p.sent.count == before + 1 }, "the model sent it")
        #expect(p.sent.last?.body == "Ship it on Friday")
        #expect(p.text == "", "the field is empty after Send")
        #expect(p.placeholder)
    }

    @Test func shiftOrOptionEnterStartsALine() async throws {
        let p = try await Page.open("newline")
        let before = p.sent.count
        await p.type("one")
        await p.enter(.shift)
        await p.type("two")
        await p.enter(.option)
        await p.type("three")
        #expect(p.text == "one\ntwo\nthree")
        #expect(p.caret == NSRange(location: 13, length: 0))
        await p.settle(0.3)
        #expect(p.sent.count == before, "nothing was sent")
        // Return with only Caps Lock on still sends, the whole text with its lines.
        await p.enter(.capsLock)
        #expect(await p.until { p.sent.count == before + 1 })
        #expect(p.sent.last?.body == "one\ntwo\nthree")
    }

    /// Return while an input method composes (Japanese kana before conversion, a Chinese pinyin buffer, a dead key)
    /// commits the composition; it never sends half-typed text and never puts a line in its place.
    @Test func enterDuringACompositionCommitsItAndDoesNotSend() async throws {
        let p = try await Page.open("ime")
        let before = p.sent.count
        await p.type("Hei ")
        p.field.setMarkedText("にほん", selectedRange: NSRange(location: 3, length: 0), replacementRange: NSRange(location: NSNotFound, length: 0))
        await p.settle()
        #expect(p.composing && p.marked == NSRange(location: 4, length: 3))
        await p.enter()
        #expect(!p.composing, "committed")
        #expect(p.text == "Hei にほん", "no line, nothing lost")
        await p.settle(0.3)
        #expect(p.sent.count == before, "not sent")
        // Escape during a composition is the input method's, never the reply's.
        p.model.reply(to: p.kari)
        await p.settle()
        p.field.setMarkedText("ni", selectedRange: NSRange(location: 2, length: 0), replacementRange: NSRange(location: NSNotFound, length: 0))
        #expect(p.placeholder == false)
        await p.escape()
        #expect(p.model.chatReply != nil, "the reply stays")
        p.field.unmarkText()
        // The committed text, Return again: sent, as the reply.
        await p.enter()
        #expect(await p.until { p.sent.count == before + 1 })
        #expect(p.sent.last?.body == "Hei にほんni")
        #expect(p.sent.last?.replyTo == p.kari.id)
        #expect(p.model.chatReply == nil)
        // An empty field composing: the placeholder is hidden (the composition is text), and back when it is undone.
        p.field.setMarkedText("か", selectedRange: NSRange(location: 1, length: 0), replacementRange: NSRange(location: NSNotFound, length: 0))
        #expect(!p.placeholder)
        p.field.setMarkedText("", selectedRange: NSRange(location: 0, length: 0), replacementRange: NSRange(location: NSNotFound, length: 0))
        p.field.unmarkText()
        #expect(p.placeholder)
    }

    @Test func escapeWithAReplyCancelsIt() async throws {
        let p = try await Page.open("escape")
        await p.type("draft")
        p.model.reply(to: p.kari)
        await p.settle()
        #expect(p.model.chatReply?.id == p.kari.id)
        await p.escape()
        #expect(p.model.chatReply == nil, "Escape cancelled the reply")
        #expect(p.text == "draft", "and left the text alone")
        // Without a reply, Escape changes nothing in the field and opens no completion list. (Only that window is
        // looked for: other suites open and close windows meanwhile, and the input system has its own.)
        await p.escape()
        let completions = NSApp.windows.filter { String(describing: type(of: $0)).localizedCaseInsensitiveContains("completion") }
        #expect(p.text == "draft" && completions.isEmpty, "\(completions)")
    }

    @Test func afterReplyTheFieldHasTheKeyboard() async throws {
        let p = try await Page.open("focus")
        p.window.makeFirstResponder(nil)
        _ = await p.until(0.2) { false }   // the binding hears that the field gave the keyboard away
        #expect(!p.focused)
        p.model.reply(to: p.kari)
        #expect(await p.until { p.focused }, "Reply puts the cursor in the field")
        // Elsewhere and Reply again (another message): the field takes it again.
        p.window.makeFirstResponder(nil)
        _ = await p.until(0.2) { false }   // the binding hears that the field gave the keyboard away
        #expect(p.model.cancelReply())
        await p.settle()
        let other = try #require(p.model.chatPage?.messages.last { $0.who != .mine && $0.id != p.kari.id })
        p.model.reply(to: other)
        #expect(await p.until { p.focused })
    }

    // MARK: Paste

    @Test func pasteTakesTextImagesAndFilesFromThePasteboard() async throws {
        let p = try await Page.open("paste")
        let pb = QADrag.pasteboard()
        defer { pb.releaseGlobally() }
        p.field.pasteboard = pb
        await p.type("hello world")
        // Text: pasted at the selection, as typed.
        p.field.setSelectedRange(NSRange(location: 6, length: 5))
        pb.clearContents()
        pb.setString("there", forType: .string)
        p.field.paste(nil)
        await p.settle()
        #expect(p.text == "hello there")
        // A picture's bytes (a screenshot): attached, its reference at the cursor.
        let png = try Data(contentsOf: ChatComposerTests.png("shot.png", in: p.dir))
        pb.clearContents()
        pb.setData(png, forType: .png)
        p.field.setSelectedRange(NSRange(location: 5, length: 0))
        p.field.paste(nil)
        await p.settle()
        #expect(p.text == "hello ![Image 1](att:1) there")
        // A file (Finder's Copy): attached by name, its reference at the cursor.
        let file = try ChatComposerTests.png("photo.png", in: p.dir)
        pb.clearContents()
        pb.writeObjects([file as NSURL])
        p.field.setSelectedRange(NSRange(location: (p.text as NSString).length, length: 0))
        p.field.paste(nil)
        await p.settle()
        #expect(p.text == "hello ![Image 1](att:1) there ![Image 2](att:2)")
        await p.field.images?.settled()
        #expect(p.attachments.count == 2 && p.attachments.last == "photo.png", "\(p.attachments)")
    }

    // MARK: Drops

    @Test func aDropInsideTheTextPutsTheReferenceAtThatCharacter() async throws {
        let p = try await Page.open("drop")
        let a = try ChatComposerTests.png("a.png", in: p.dir)
        let pb = QADrag.pasteboard()
        defer { pb.releaseGlobally() }
        pb.writeObjects([a as NSURL])
        // UTF-16: the emoji is two units, so "after" starts at 10.
        await p.type("😀 before after")
        let at = try #require(p.field.dropPoint(before: 10))
        let index = p.field.dropIndex(QADrag(pb, at: at, in: p.window))
        #expect(index == 10)
        #expect(!p.dropping)
        let drag = QADrag(pb, at: at, in: p.window)
        let entered = p.field.draggingEntered(drag)
        #expect(entered == .copy)
        await p.settle()
        #expect(p.dropping, "the drop outline shows while it is over the text")
        p.field.draggingExited(drag)
        await p.settle()
        #expect(!p.dropping, "and goes when it leaves")
        let (op, ok) = p.field.drop(pb, at: at)
        await p.settle()
        #expect(op == .copy && ok && !p.dropping)
        #expect(p.text == "😀 before ![Image 1](att:1) after")
        #expect(p.caret == NSRange(location: 28, length: 0), "the cursor after the reference")
        await p.field.images?.settled()
        #expect(p.attachments.count == 1)
    }

    /// Scrolled to its end, a drop on a visible line lands at that line's character: the drag's window location is
    /// converted through the scroll view's offset.
    @Test func aDropOnAScrolledFieldLandsOnTheLineUnderIt() async throws {
        let p = try await Page.open("scrolled")
        let pb = QADrag.pasteboard()
        defer { pb.releaseGlobally() }
        pb.writeObjects([try ChatComposerTests.png("a.png", in: p.dir) as NSURL])
        await p.type((1...14).map { "line \($0)" }.joined(separator: "\n"))
        p.field.scrollRangeToVisible(NSRange(location: (p.text as NSString).length, length: 0))
        await p.settle()
        #expect(p.scroll.contentView.bounds.origin.y > 0, "scrolled")
        let text = p.text as NSString
        let line13 = text.range(of: "line 13").location
        let at = try #require(p.field.dropPoint(before: line13 + 5))
        let took = p.field.drop(pb, at: at).1
        #expect(took)
        await p.settle()
        #expect((p.text as NSString).substring(from: line13).hasPrefix("line ![Image 1](att:1) 13"), "\(p.text)")
        // The check of the file ends before its folder goes (else the core refuses it and says so in an alert).
        await p.field.images?.settled()
        #expect(p.attachments == ["a.png"])
    }

    @Test func aDropOnAnEmptyFieldOrOnTheChipsLeavesNoReference() async throws {
        let p = try await Page.open("chips")
        let pb = QADrag.pasteboard()
        defer { pb.releaseGlobally() }
        pb.writeObjects([try ChatComposerTests.png("a.png", in: p.dir) as NSURL])
        let empty = try #require(p.field.dropPoint(before: 0))
        let took = p.field.drop(pb, at: empty).1
        #expect(took, "attached")
        await p.settle()
        #expect(p.text == "", "an image alone is no reference")
        let images = try #require(p.field.images)
        await images.settled()
        #expect(p.attachments.count == 1)
        // Onto the chips (the conversation's own drop, outside the text): attached, still no reference in the text.
        await p.type("caption")
        let b = try ChatComposerTests.png("b.png", in: p.dir)
        let dropped = images.drop([try #require(NSItemProvider(contentsOf: b))], core: p.core)
        #expect(dropped)
        #expect(await p.until { p.attachments.count == 2 })
        #expect(p.text == "caption")
    }

    // MARK: Height and placeholder

    /// One line high empty, a line more for each line to eight, then it keeps its height and scrolls.
    @Test func theFieldGrowsFromOneToEightLinesThenScrolls() async throws {
        let p = try await Page.open("height")
        var heights: [CGFloat] = []
        for n in 1...11 {
            p.field.selectAll(nil)
            await p.type((1...n).map { "line \($0)" }.joined(separator: "\n"))
            heights.append(p.scroll.frame.height)
        }
        let line = heights[2] - heights[1]
        #expect(line > 12 && line < 30, "\(heights)")
        #expect(heights[0] >= 28 && heights[0] <= 34, "one line and the insets, as the field always was: \(heights)")
        #expect(heights[1] > heights[0] && heights[1] - heights[0] <= line, "the one-line field holds its spacing: \(heights)")
        for n in 2..<8 { #expect(abs(heights[n] - heights[n - 1] - line) <= 1, "line \(n + 1): \(heights)") }
        // The ninth line scrolls; it does not add its spacing to the field (it once grew 2 points more there).
        #expect(heights[8...].allSatisfy { $0 == heights[7] }, "eight lines at most: \(heights)")
        let (textHeight, visible) = (p.field.frame.height, p.scroll.contentView.bounds.height)
        #expect(textHeight > visible + line, "the text is taller than the field: it scrolls")
        // Emptied: one line again.
        p.field.selectAll(nil)
        p.field.deleteBackward(nil)
        await p.settle()
        #expect(p.scroll.frame.height == heights[0])
    }

    @Test func thePlaceholderShowsOnlyWhenEmpty() async throws {
        let p = try await Page.open("placeholder")
        let words = try #require(p.model.chatPage?.composer.placeholder)
        let shown = p.field.placeholder
        #expect(!words.isEmpty && shown == words)
        let empty = Self.ink(p.field)
        #expect(p.placeholder && empty > 0, "drawn while empty")
        await p.type("x")
        let typed = Self.ink(p.field)
        #expect(!p.placeholder && typed > 0 && typed < empty, "one letter, no words")
        p.field.deleteBackward(nil)
        await p.settle()
        let again = Self.ink(p.field)
        #expect(p.placeholder && again == empty)
        // With no words, an empty field draws nothing.
        p.field.placeholder = ""
        let none = Self.ink(p.field)
        #expect(none == 0)
    }

    /// How many of the view's pixels are drawn on (not clear), read from the bitmap's bytes.
    static func ink(_ view: NSView) -> Int {
        view.display()
        guard let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { return 0 }
        view.cacheDisplay(in: view.bounds, to: rep)
        guard let data = rep.bitmapData, rep.samplesPerPixel == 4, rep.bitsPerSample == 8, !rep.isPlanar else { return -1 }
        let alpha = rep.bitmapFormat.contains(.alphaFirst) ? 0 : 3
        var n = 0
        for y in 0..<rep.pixelsHigh {
            let row = data + y * rep.bytesPerRow
            for x in 0..<rep.pixelsWide where row[x * 4 + alpha] > 25 { n += 1 }
        }
        return n
    }
}
