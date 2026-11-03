import AppKit
import Testing
import UserNotifications
@testable import SeedwellApp

/// The menu bar icon's one dot (something needs the user: a problem, or an agent's request waiting for an answer): small,
/// orange, lower right, the same for both; the icon keeps its width. And a request's notice is time-sensitive.
@MainActor @Suite struct StatusIconTests {
    /// The icon drawn at 2x in `dark` or light.
    static func pixels(dot: Bool, dark: Bool) -> NSBitmapImageRep {
        let image = Drawing.statusIcon(down: 0, up: 0, progress: nil, attention: dot)
        let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(image.size.width) * 2, pixelsHigh: Int(image.size.height) * 2,
                                   bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                                   colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        rep.size = image.size
        let ctx = NSGraphicsContext(bitmapImageRep: rep)!
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = ctx
        NSAppearance(named: dark ? .darkAqua : .aqua)!.performAsCurrentDrawingAppearance {
            image.draw(in: NSRect(origin: .zero, size: image.size))
        }
        ctx.flushGraphics()
        NSGraphicsContext.restoreGraphicsState()
        return rep
    }

    /// The colour at point (x, y) of the icon (y up, as the icon is drawn).
    static func color(_ rep: NSBitmapImageRep, _ x: CGFloat, _ y: CGFloat) -> NSColor? {
        rep.colorAt(x: Int(x * 2), y: rep.pixelsHigh - 1 - Int(y * 2))?.usingColorSpace(.sRGB)
    }

    @Test func theIconKeepsItsWidthWithAndWithoutTheDot() {
        let plain = Drawing.statusIcon(down: 0, up: 0, progress: nil, attention: false)
        let dot = Drawing.statusIcon(down: 0, up: 0, progress: nil, attention: true)
        #expect(plain.size == dot.size)
        #expect(plain.size == NSSize(width: 20, height: 18))
    }

    @Test func theOneDotIsOrangeLowerRightInLightAndDark() {
        for dark in [false, true] {
            let c = Self.color(Self.pixels(dot: true, dark: dark), 18, 2)
            #expect(c != nil)
            if let c {
                #expect(c.alphaComponent > 0.5)
                #expect(c.redComponent > 0.7 && c.blueComponent < 0.4, "orange: \(c)")
            }
            let none = Self.color(Self.pixels(dot: false, dark: dark), 18, 2)
            #expect((none?.alphaComponent ?? 0) < 0.05, "nothing there without the dot")
        }
    }

    @Test func aRequestsNoticeIsTimeSensitive() {
        #expect(Notifier.level(.persistent) == .timeSensitive)
        #expect(Notifier.level(.timeSensitive) == .timeSensitive)
        #expect(Notifier.level(.normal) == .active)
    }
}
