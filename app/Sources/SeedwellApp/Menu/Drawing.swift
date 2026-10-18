import AppKit

/// Small vector images for the menu bar: the status icon and the progress ring.
enum Drawing {
    /// How strongly an arrow is coloured for a transfer rate: 0 below 5 KB/s, then logarithmic up to
    /// 1 at 10 MB/s, in 8 steps (the icon is only redrawn when a step changes).
    static func intensity(_ bytesPerSec: UInt64) -> Double {
        guard bytesPerSec >= 5_000 else { return 0 }
        let t = log10(Double(bytesPerSec) / 5_000) / log10(10_000_000 / 5_000)
        return (min(1, max(0, t)) * 8).rounded(.up) / 8
    }

    /// Menu bar icon: a down and an up arrow, no text. Each arrow goes from the menu bar's dimmed
    /// grey to green (download) or red (upload) with its transfer rate (`down`/`up` from
    /// [`intensity`]); a faint hairline shows total download progress (`progress`, nil = none); a
    /// small orange dot in the lower right corner asks for attention. While files are being moved
    /// (`moving`, 0...1, aggregate over all moves) the hairline is a green bar instead and shows that. Drawn with dynamic
    /// colours at draw time, so it follows the light/dark menu bar.
    static func statusIcon(down: Double, up: Double, progress: Double?, moving: Double? = nil, attention: Bool) -> NSImage {
        let image = NSImage(size: NSSize(width: 20, height: 18), flipped: false) { _ in
            let grey = NSColor.labelColor.withAlphaComponent(0.5)
            func tint(_ c: NSColor, _ t: Double) -> NSColor {
                guard t > 0 else { return grey }
                let from = grey.usingColorSpace(.sRGB) ?? grey, to = c.usingColorSpace(.sRGB) ?? c
                // Even the faintest step is visibly tinted; full speed is the plain system colour.
                return from.blended(withFraction: 0.3 + 0.7 * t, of: to) ?? to
            }
            func arrow(x: CGFloat, up isUp: Bool, color: NSColor) {
                let path = NSBezierPath()
                let (a, b): (CGFloat, CGFloat) = isUp ? (4, 15) : (15, 4)
                let d: CGFloat = isUp ? -3.2 : 3.2
                path.move(to: NSPoint(x: x, y: a))
                path.line(to: NSPoint(x: x, y: b))
                path.move(to: NSPoint(x: x - 3.4, y: b + d))
                path.line(to: NSPoint(x: x, y: b))
                path.line(to: NSPoint(x: x + 3.4, y: b + d))
                path.lineWidth = 1.6
                path.lineCapStyle = .round
                path.lineJoinStyle = .round
                color.setStroke()
                path.stroke()
            }
            arrow(x: 6.5, up: false, color: tint(.systemGreen, down))
            arrow(x: 13.5, up: true, color: tint(.systemRed, up))
            if let moving {
                NSColor.systemGreen.withAlphaComponent(0.28).setFill()
                NSRect(x: 3, y: 1, width: 14, height: 1).fill()
                NSColor.systemGreen.setFill()
                NSRect(x: 3, y: 1, width: 14 * max(0.05, min(1, moving)), height: 1).fill()
            } else if let progress {
                NSColor.labelColor.withAlphaComponent(0.18).setFill()
                NSRect(x: 3, y: 1, width: 14, height: 1).fill()
                NSColor.labelColor.withAlphaComponent(0.55).setFill()
                NSRect(x: 3, y: 1, width: 14 * max(0.05, min(1, progress)), height: 1).fill()
            }
            if attention {
                // Lower right, clear of the arrows; a ring of clear pixels ends the progress hairline
                // short of it.
                let dot = NSRect(x: 16, y: 0, width: 4, height: 4)
                NSGraphicsContext.current?.compositingOperation = .clear
                NSBezierPath(ovalIn: dot.insetBy(dx: -1.2, dy: -1.2)).fill()
                NSGraphicsContext.current?.compositingOperation = .sourceOver
                NSColor.systemOrange.setFill()
                NSBezierPath(ovalIn: dot).fill()
            }
            return true
        }
        image.isTemplate = false
        return image
    }

    /// Progress ring used as the menu item image (template, so it follows the menu's text colour).
    static func ring(_ fraction: Double) -> NSImage {
        let image = NSImage(size: NSSize(width: 16, height: 16), flipped: false) { rect in
            let r = rect.insetBy(dx: 2, dy: 2)
            let track = NSBezierPath(ovalIn: r)
            track.lineWidth = 2
            NSColor.black.withAlphaComponent(0.3).setStroke()
            track.stroke()
            let arc = NSBezierPath()
            let c = NSPoint(x: rect.midX, y: rect.midY)
            arc.appendArc(withCenter: c, radius: r.width / 2, startAngle: 90, endAngle: 90 - 360 * max(0.04, fraction), clockwise: true)
            arc.lineWidth = 2
            arc.lineCapStyle = .round
            NSColor.black.setStroke()
            arc.stroke()
            return true
        }
        image.isTemplate = true
        return image
    }

    /// A small solid dot (not a template: it keeps its colour in the menu).
    static func dot(_ color: NSColor) -> NSImage {
        let image = NSImage(size: NSSize(width: 16, height: 16), flipped: false) { rect in
            color.setFill()
            NSBezierPath(ovalIn: rect.insetBy(dx: 4.5, dy: 4.5)).fill()
            return true
        }
        image.isTemplate = false
        return image
    }

    /// Green circle with a white tick (the "finished" mark of concept 1), drawn here so it never depends on SF Symbol rendering modes.
    static func check() -> NSImage? {
        let image = NSImage(size: NSSize(width: 16, height: 16), flipped: false) { rect in
            NSColor.systemGreen.setFill()
            NSBezierPath(ovalIn: rect.insetBy(dx: 1.5, dy: 1.5)).fill()
            let tick = NSBezierPath()
            tick.move(to: NSPoint(x: 5.0, y: 8.2))
            tick.line(to: NSPoint(x: 7.2, y: 5.9))
            tick.line(to: NSPoint(x: 11.2, y: 10.6))
            tick.lineWidth = 1.7
            tick.lineCapStyle = .round
            tick.lineJoinStyle = .round
            NSColor.white.setStroke()
            tick.stroke()
            return true
        }
        image.isTemplate = false
        return image
    }
}
