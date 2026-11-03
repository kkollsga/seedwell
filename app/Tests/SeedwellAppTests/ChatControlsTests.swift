import AppKit
import SwiftUI
import Testing
import UserNotifications
@testable import SeedwellApp

/// What once needed a person to check: a message's pointer controls, its gallery (arrow keys, buttons, thumbnails),
/// a click on an image reference, a code block's copy button, a notification's Approve and Deny, and the menu bar
/// menu's request line. Each is checked through the call its click or key makes (the same calls QA's `chat`,
/// `notice-action` and `menu-requests` make in a running app); no event is sent.
@MainActor @Suite(.serialized) struct ChatControlsTests {
    // MARK: Gallery

    @Test func theGalleryStepsWithinItsImages() {
        #expect(ChatGalleryView.active(selection: nil, count: 5) == 0, "the first until one is chosen")
        #expect(ChatGalleryView.active(selection: nil, start: 1, count: 5) == 1)
        #expect(ChatGalleryView.active(selection: 9, count: 5) == 4 && ChatGalleryView.active(selection: -1, count: 5) == 0)
        #expect(ChatGalleryView.step(0, by: 1, count: 5) == 1)
        #expect(ChatGalleryView.step(4, by: 1, count: 5) == nil, "→ at the last is not used (the key goes on)")
        #expect(ChatGalleryView.step(0, by: -1, count: 5) == nil, "← at the first neither")
        #expect(ChatGalleryView.step(3, by: -1, count: 5) == 2)
    }

    /// A message with a gallery of five and references to its second and fourth image, as the core lays it out.
    static func galleryCard() async throws -> ChatMessageCard {
        let samples = await ChatComposerTests.samples()
        let images = (1...5).compactMap { n in samples.first { $0.label == "gallery-\(n)" }?.image }
        try #require(images.count == 5)
        let body = "Fra turen: toppen ![Image 2](att:2) og hytta ![Image 4](att:4)."
        let m = SyncMessage(id: "img5-c0ffee", channelId: "C-Team", authorDevice: "D1", authorName: "Kari's iPhone", authorUser: "Kari", isMe: false,
                            agent: nil, atS: 1_800_000_000, replyTo: nil, topic: nil, kind: .text, body: body, images: images, files: [], place: "C-Team")
        return try #require(ChatTests.page(ChatTests.channel(), [m])?.messages.first)
    }

    /// The row in a window (never shown), registered for QA as a QA run registers it.
    static func host(_ card: ChatMessageCard, core: Core) async -> (NSWindow, ChatQAHooks.Row?) {
        _ = NSApplication.shared
        ChatQAHooks.enabled = true
        let view = ChatMessageRow(card: card, core: core).frame(width: 640)
        let host = NSHostingView(rootView: view)
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 640, height: 900), styleMask: [.titled], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = host
        for _ in 0..<10 {
            await settle()
            host.layoutSubtreeIfNeeded()
        }
        return (window, ChatQAHooks.rows[card.id])
    }

    /// The run loop in short slices, the main actor free in between (other suites' main-actor work runs meanwhile).
    static func settle() async {
        for _ in 0..<5 {
            spinRunLoop(0.005)
            try? await Task.sleep(for: .milliseconds(2))
        }
    }

    @Test func aMessagesPointerGalleryAndReferencesDoWhatTheirClicksDo() async throws {
        let (core, dir) = try await ChatComposerTests.coreOffMain("controls")
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let card = try await Self.galleryCard()
        #expect(card.gallery && card.images.count == 5 && card.reply != nil)
        let (window, hooks) = await Self.host(card, core: core)
        defer { window.close(); ChatQAHooks.enabled = false; ChatQAHooks.rows = [:] }
        let row = try #require(hooks, "the shown row registered")
        // The pointer on the message: its time, Tag and Reply show; off: they go.
        #expect(!row.hovered())
        row.hover(true)
        await Self.settle()
        #expect(row.hovered())
        row.hover(false)
        await Self.settle()
        #expect(!row.hovered())
        // The gallery: the first image until one is chosen; a thumbnail shows its image; → and ← step within it.
        #expect(row.gallery().selection == nil && row.gallery().count == 5)
        row.show(2)
        await Self.settle()
        #expect(row.gallery().selection == 2)
        // A click on the reference to the fourth image (`att:4`, the link's index 3): the gallery shows it.
        row.reference(3)
        await Self.settle()
        #expect(row.gallery().selection == 3)
        let next = ChatGalleryView.step(ChatGalleryView.active(selection: row.gallery().selection, count: 5), by: 1, count: 5)
        #expect(next == 4)
        #expect(chatGalleryCounter(active: 4, count: 5) == "4 / 5")
    }

    // MARK: Code

    @Test func theCodeCopyButtonCopiesTheCodeExactly() throws {
        let pb = QADrag.pasteboard()
        defer { pb.releaseGlobally() }
        let code = "seedwell chat read \"Release notes\" --project Seedwell\n  indented\t tab"
        ChatCodeBlock.copy(code, to: pb)
        #expect(pb.string(forType: .string) == code, "the code as written: no language, nothing added")
        #expect(pb.availableType(from: [.fileURL, .URL, .rtf]) == nil, "plain text only")
        #expect(!ChatCodeBlock.copyHelp.isEmpty)
        // Outside a QA run Copy goes to the system's pasteboard (a QA run's is its own: `chat copy-code`).
        #expect(ChatCodeBlock.pasteboard == NSPasteboard.general)
    }

    // MARK: Agents' requests

    static func pending(_ core: Core) -> [String] { core.agentRequests(nowMs: UInt64(Date().timeIntervalSince1970 * 1000)).rows.map(\.id) }

    @Test func aNotificationsButtonsAnswerTheRequestAndAClickOpensItsRoute() async throws {
        let (core, dir) = try await ChatComposerTests.coreOffMain("notice")
        defer { try? FileManager.default.removeItem(atPath: dir) }
        // The categories the app registers: Approve asks to unlock, Deny is destructive (the core's words).
        let cat = try #require(Notifier.categories().first { $0.identifier == "agent-request" })
        #expect(cat.actions.map(\.identifier) == ["approve", "deny"])
        #expect(cat.actions.map(\.title) == ["Approve", "Deny"])
        #expect(cat.actions[0].options == .authenticationRequired && cat.actions[1].options == .destructive)
        // A click on the notice (no button): its route opens, nothing is answered.
        #expect(Self.pending(core) == ["5eed0a11"], "the mock's waiting request")
        var opened: [NoticeRoute] = []
        Notifier.respond(click: "agent-request\n5eed0a11", action: UNNotificationDefaultActionIdentifier, core: core) { opened.append($0) }
        #expect(opened == [.agentRequests])
        // Deny on another notice's click is no answer: its route opens.
        Notifier.respond(click: "transfer\nabc", action: "deny", core: core) { opened.append($0) }
        #expect(opened.last == .transfer(id: "abc"))
        // Approve: the request is answered (off the main thread), and nothing opens.
        Notifier.respond(click: "agent-request\n5eed0a11", action: "approve", core: core) { opened.append($0) }
        #expect(opened.count == 2)
        for _ in 0..<100 where !Self.pending(core).isEmpty { try await Task.sleep(for: .milliseconds(20)) }
        #expect(Self.pending(core).isEmpty, "approved")
    }

    @Test func theMenusRequestLineHasApproveAndDenyThatAnswer() async throws {
        let (core, dir) = try await ChatComposerTests.coreOffMain("menu-requests")
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let model = core.agentRequests(nowMs: UInt64(Date().timeIntervalSince1970 * 1000))
        let line = try #require(model.line)
        let item = StatusMenuController.requestsItem(model, line, core: core)
        let sub = try #require(item.submenu)
        #expect(item.title == line && item.subtitle == model.rows.first?.detail)
        let titles = sub.items.map(\.title)
        #expect(titles.suffix(2) == [model.approveTitle, model.denyTitle], "\(titles)")
        #expect(sub.items.suffix(2).allSatisfy { $0.representedObject as? String == "5eed0a11" && $0.isEnabled })
        // Deny pressed as a click presses it.
        let deny = try #require(sub.items.firstIndex { $0.title == model.denyTitle })
        sub.performActionForItem(at: deny)
        for _ in 0..<100 where !Self.pending(core).isEmpty { try await Task.sleep(for: .milliseconds(20)) }
        #expect(Self.pending(core).isEmpty, "denied")
    }
}
