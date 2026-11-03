import AppKit
import SwiftUI
import Testing
@testable import SeedwellApp

/// Images in chat (SPEC 7G.7 Attachments): how they fit the column and what a paste or a drop hands the core. The
/// composer's field: ChatComposerTests. What an image is and says is the core's (crates/ffi/src/chat_images.rs).
@MainActor @Suite struct ChatImagesTests {
    @Test func imagesFitTheColumnWithTheirAspectRatioKept() {
        // On a Retina display (2 pixels a point): a picture is never drawn larger than its own pixels.
        let fit = { (w: UInt32, h: UInt32, available: CGFloat) in ChatImageLayout.fit(width: w, height: h, available: available, scale: 2) }
        #expect(fit(2560, 1600, 640) == CGSize(width: 640, height: 400), "the content area's width limits a screenshot")
        #expect(fit(1000, 4000, 640) == CGSize(width: 105, height: 420), "the height (420) limits a tall one, never cropped")
        #expect(fit(120, 80, 640) == CGSize(width: 60, height: 40), "a small one at its own size, sharp")
        #expect(fit(10_000, 1, 640) == CGSize(width: 640, height: 16), "never thinner than 16 points")
        #expect(fit(2560, 1600, 300).width == 300, "a narrow area")
        #expect(ChatImageLayout.fit(width: 120, height: 80, available: 640, scale: 1) == CGSize(width: 120, height: 80), "1 pixel a point elsewhere")
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

    @Test func aMessageOfImagesAloneIsPreviewedByTheirCaptions() {
        let shot = ChatImage(hash: "ab", kind: .png, name: "shot.png", size: 240_000, width: 1280, height: 800, state: .available,
                             caption: "shot.png · 1280 × 800 · 240 KB", status: "", hasPreview: true)
        var m = ChatTests.message("")
        m.images = [shot]
        #expect(chatMessagePreview(message: m, limit: 120) == "shot.png · 1280 × 800 · 240 KB", "the core's words for it")
        m.body = "look"
        #expect(chatMessagePreview(message: m, limit: 120) == "look", "a caption is the preview")
    }

    /// The same picture twice in one message (pasted twice, or two copies of a file) is two thumbnails, and only the
    /// one shown is marked: they were once one identity (keyed by the picture's hash), and all or none were lit.
    @Test func theSamePictureTwiceIsTwoThumbnailsAndOneIsActive() async throws {
        let (core, dir) = try await ChatComposerTests.coreOffMain("thumbs")
        defer { try? FileManager.default.removeItem(atPath: dir) }
        let samples = await ChatComposerTests.samples()
        let a = try #require(samples.first { $0.label == "gallery-1" }?.image)
        let b = try #require(samples.first { $0.label == "gallery-2" }?.image)
        _ = NSApplication.shared
        NSApp.perform(NSSelectorFromString("accessibilitySetValue:forAttribute:"), with: true, with: "AXEnhancedUserInterface")
        for shown in 0..<3 {
            let gallery = ChatGalleryView(core: core, channelId: "C", images: [a, a, b], available: 420, selection: .constant(shown))
            let host = NSHostingView(rootView: gallery.frame(width: 420, height: 600))
            let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 420, height: 600), styleMask: [.titled], backing: .buffered, defer: false)
            window.isReleasedWhenClosed = false
            window.contentView = host
            for _ in 0..<5 {
                spinRunLoop(0.005)
                try? await Task.sleep(for: .milliseconds(2))
            }
            host.layoutSubtreeIfNeeded()
            // The filmstrip's thumbnails as the accessibility tree has them: their captions, and which is selected.
            var thumbs: [(String, Bool)] = []
            func walk(_ e: Any, _ depth: Int) {
                guard depth < 40, let o = e as? NSObject else { return }
                let label = (o.responds(to: #selector(NSAccessibilityElement.accessibilityLabel)) ? o.value(forKey: "accessibilityLabel") as? String : nil) ?? ""
                if [a.caption, b.caption].contains(label), o.responds(to: #selector(NSAccessibilityElement.isAccessibilitySelected)) {
                    thumbs.append((label, (o.value(forKey: "accessibilitySelected") as? Bool) ?? false))
                }
                let children = o.responds(to: #selector(NSAccessibilityElement.accessibilityChildren)) ? o.value(forKey: "accessibilityChildren") as? [Any] ?? [] : []
                for c in children { walk(c, depth + 1) }
            }
            walk(host, 0)
            window.close()
            // The stage's picture comes first (never selected), then the filmstrip.
            #expect(thumbs.count == 4, "the stage and three thumbnails: \(thumbs)")
            #expect(thumbs.suffix(3).map(\.1) == (0..<3).map { $0 == shown }, "only the one shown is lit (\(shown)): \(thumbs)")
        }
    }
}
