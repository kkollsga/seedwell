import AppKit
import Testing
@testable import SeedwellApp

/// Files in chat (SPEC 7G.7 Files): the pictures cards draw and the previews of a message of files alone. What a file
/// is, says and offers is the core's (crates/ffi/src/chat_files.rs).
@MainActor @Suite(.timeLimit(.minutes(1))) struct ChatFilesTests {
    @Test func everyIconIsASymbolTheSystemHas() {
        let all: [ChatFileIcon] = [.document, .text, .pdf, .image, .audio, .video, .archive, .code, .app, .diskImage, .spreadsheet, .presentation, .other]
        for icon in all {
            #expect(NSImage(systemSymbolName: ChatFileLayout.symbol(icon), accessibilityDescription: nil) != nil, "\(icon)")
        }
    }

    @Test func aMessageOfFilesAloneIsPreviewedByTheirNamesAndSizes() {
        let film = ChatFileCard(hash: "cd", name: "Film.mov", sizeText: "1.2 GB", icon: .video, state: .available,
                                status: "1.2 GB · Not downloaded", progress: nil, path: nil,
                                actions: [ChatFileAction(kind: .download, label: "Download")])
        var m = ChatTests.message("")
        m.files = [film]
        #expect(chatMessagePreview(message: m, limit: 120) == "Film.mov · 1.2 GB")
        m.body = "the cut"
        #expect(chatMessagePreview(message: m, limit: 120) == "the cut", "a caption is the preview")
    }
}
