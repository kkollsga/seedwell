import AppKit
import Testing
@testable import SeedwellApp

/// Images in chat (SPEC 7G.7 Attachments): how they fit the column, what a paste or a drop hands the core, and which
/// key is a paste. What an image is and says is the core's (crates/ffi/src/chat_images.rs).
@MainActor @Suite struct ChatImagesTests {
    @Test func imagesFitTheColumnWithTheirAspectRatioKept() {
        let wide = ChatImageLayout.fit(width: 2560, height: 1600)
        #expect(wide == CGSize(width: 420, height: 263), "the width limits a screenshot")
        let tall = ChatImageLayout.fit(width: 1000, height: 4000)
        #expect(tall == CGSize(width: 80, height: 320), "the height limits a tall one")
        #expect(ChatImageLayout.fit(width: 100, height: 50) == CGSize(width: 100, height: 50), "never larger than it is")
        #expect(ChatImageLayout.fit(width: 10_000, height: 1) == CGSize(width: 420, height: 40), "never thinner than 40 points")
        #expect(ChatImageLayout.fit(width: 2560, height: 1600, available: 300).width == 300, "a narrow column")
    }

    @Test func aPastedScreenshotGoesToTheCoreAsPngAndTextIsLeftToTheField() {
        let pb = NSPasteboard(name: NSPasteboard.Name("seedwell-test-\(UUID().uuidString)"))
        defer { pb.releaseGlobally() }
        // A screenshot copied with Cmd-Ctrl-Shift-4 is a TIFF (and often a PNG) on the pasteboard.
        let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 4, pixelsHigh: 2, bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true,
                                   isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        pb.clearContents()
        pb.setData(rep.tiffRepresentation!, forType: .tiff)
        #expect(ChatImagePasteboard.hasImage(pb))
        let inputs = ChatImagePasteboard.inputs(from: pb, pastedName: "Pasted Image.png")
        #expect(inputs.count == 1 && inputs[0].name == "Pasted Image.png" && inputs[0].path == nil)
        #expect(inputs[0].bytes?.starts(with: [0x89, 0x50, 0x4E, 0x47]) == true, "made a PNG")
        // Text: the field pastes it.
        pb.clearContents()
        pb.setString("hello", forType: .string)
        #expect(!ChatImagePasteboard.hasImage(pb))
        #expect(ChatImagePasteboard.inputs(from: pb, pastedName: "x.png").isEmpty)
        // A file copied in the Finder: its path, for the core to read and check.
        pb.clearContents()
        pb.writeObjects([URL(fileURLWithPath: "/tmp/Shot 1.png") as NSURL])
        let files = ChatImagePasteboard.inputs(from: pb, pastedName: "x.png")
        #expect(files.map(\.name) == ["Shot 1.png"] && files[0].path == "/tmp/Shot 1.png" && files[0].bytes == nil)
    }

    @Test func onlyCommandVIsAPaste() {
        func key(_ chars: String, _ flags: NSEvent.ModifierFlags) -> NSEvent {
            NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0, windowNumber: 0, context: nil,
                             characters: chars, charactersIgnoringModifiers: chars, isARepeat: false, keyCode: 9)!
        }
        #expect(ChatImageInputModifier.isPaste(key("v", .command)))
        #expect(ChatImageInputModifier.isPaste(key("v", [.command, .capsLock])))
        #expect(!ChatImageInputModifier.isPaste(key("v", [.command, .option, .shift])), "Paste and Match Style is the field's")
        #expect(!ChatImageInputModifier.isPaste(key("v", [])))
        #expect(!ChatImageInputModifier.isPaste(key("c", .command)))
    }

    @Test func aMessageOfImagesAloneIsPreviewedByTheirCaptions() {
        let shot = ChatImage(hash: "ab", kind: .png, name: "shot.png", size: 240_000, width: 1280, height: 800, state: .available,
                             caption: "shot.png · 1280 × 800 · 240 KB", status: "", hasPreview: true)
        var m = ChatTests.message("")
        m.images = [shot]
        #expect(ChatText.preview(m) == "shot.png · 1280 × 800 · 240 KB", "the core's words for it")
        m.body = "look"
        #expect(ChatText.preview(m) == "look", "a caption is the preview")
    }
}
