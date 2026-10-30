import AppKit

/// QA's `shot <name> [what]`: a picture of the running app's own windows, drawn in-process at 2x (QuietRun keeps them
/// off every screen and drawn active), so no capture of the screen is needed. Each is drawn every 50 ms until two
/// drawings match (SnapshotWindow.settledImage).
///
///     main | settings      the main window (Settings is a section of it) with its sheet and Get Info's popover
///     info                 Get Info's popover alone
///     chooser              the first chooser window
///     popover              the "Starting in …" notifier (its content, on a stand-in of the popover's background)
///     menu                 the open context menu, else the open menu bar menu: real item views and stand-ins of what
///                          NSMenu draws itself (MenuPanel), with the submenus of a context menu beside it
///     alert                the open alert
///     window <id|title>    any other window of the app (chat-new, sync-add, finder-setup, …)
@MainActor enum QAShots {
    struct Scene {
        let app: AppDelegate
        /// The context menu QA has "open" (a quiet run pops none up).
        let menu: NSMenu?
        /// The menu bar menu's footer side drawn hovered (`menu-hover`).
        let menuHover: String?
    }

    /// The picture, or why there is none.
    static func image(_ what: String, in scene: Scene) async -> Result<NSBitmapImageRep, ShotError> {
        let words = what.split(separator: " ", maxSplits: 1).map(String.init)
        let app = scene.app
        switch words.first ?? "main" {
        case "main", "settings":
            guard let w = app.mainWindowController?.nsWindow, w.isVisible else { return .failure("the main window is closed") }
            var extras: [NSWindow] = w.attachedSheet.map { [$0] } ?? []
            extras += (w.childWindows ?? []).filter { $0.isVisible && $0 !== w.attachedSheet }
            return await windows(w, with: extras)
        case "info":
            guard let w = app.mainWindowController?.infoWindow else { return .failure("Get Info is not open") }
            return await windows(w, with: [])
        case "chooser":
            guard let w = app.qaChooserWindow, w.isVisible else { return .failure("no chooser window") }
            return await windows(w, with: w.attachedSheet.map { [$0] } ?? [])
        case "popover":
            guard let w = app.startingNotifier?.popoverWindow, w.isVisible else { return .failure("the notifier is not shown") }
            app.startingNotifier?.qaFit()
            return await windows(w, with: [])
        case "alert":
            guard let w = NSApp.modalWindow ?? NSApp.windows.first(where: { $0.isVisible && $0 is NSPanel && $0.styleMask.contains(.titled) })
            else { return .failure("no alert") }
            return await windows(w, with: [])
        case "menu":
            return await menu(scene)
        case "window":
            let key = words.count > 1 ? words[1] : ""
            guard let w = NSApp.windows.first(where: { $0.isVisible && !key.isEmpty
                && (($0.identifier?.rawValue ?? "").localizedCaseInsensitiveContains(key) || $0.title.localizedCaseInsensitiveContains(key)) })
            else { return .failure("no window \(key)") }
            return await windows(w, with: w.attachedSheet.map { [$0] } ?? [])
        default:
            return .failure("what is \(what)? main, settings, info, chooser, popover, menu, alert, window <id>")
        }
    }

    struct ShotError: Error, ExpressibleByStringInterpolation, CustomStringConvertible {
        let description: String
        init(stringLiteral value: String) { description = value }
        init(_ value: String) { description = value }
    }

    /// The open windows, for `windows` (and `wait windows ~ …`): "main 1180x820 sheet info | chooser | popover | menu".
    static func listing(_ scene: Scene) -> String {
        let app = scene.app
        var parts: [String] = []
        func at(_ w: NSWindow) -> String { "@\(Int(w.frame.minX)),\(Int(w.frame.minY))" }
        if let w = app.mainWindowController?.nsWindow, w.isVisible {
            var main = "main \(Int(w.frame.width))x\(Int(w.frame.height))\(at(w))"
            if let s = w.attachedSheet { main += " sheet\(at(s))" }
            if let i = app.mainWindowController?.infoWindow { main += " info \(Int(i.frame.width))x\(Int(i.frame.height))\(at(i))" }
            parts.append(main)
        }
        if app.qaChooserWindow?.isVisible == true { parts.append("chooser") }
        if app.startingNotifier?.isShown == true { parts.append("popover") }
        if scene.menu != nil || app.statusMenuController?.qaMenuOpen == true { parts.append("menu") }
        if NSApp.modalWindow != nil { parts.append("alert") }
        for w in NSApp.windows where w.isVisible {
            if let id = w.identifier?.rawValue, ["chat-new", "sync-", "finder-setup"].contains(where: id.hasPrefix) { parts.append(id) }
        }
        return "windows: " + (parts.isEmpty ? "none" : parts.joined(separator: " | "))
    }

    // MARK: Windows

    /// The window's frame view with `extras` (its sheet, its popovers) where they are over it, on one canvas. A part
    /// that sits far from the window (moved off the screens on its own) is left out.
    private static func windows(_ w: NSWindow, with extras: [NSWindow]) async -> Result<NSBitmapImageRep, ShotError> {
        guard w.contentView?.superview != nil else { return .failure("the window has no content") }
        let rep = await SnapshotWindow.settled {
            var parts: [(NSView, NSRect, Bool)] = []
            if let v = w.contentView?.superview { parts.append((v, w.frame, isPopover(w))) }
            for x in extras {
                guard let v = x.contentView?.superview else { continue }
                var f = x.frame
                if x === w.attachedSheet, !w.frame.insetBy(dx: -40, dy: -40).contains(f) {
                    // A sheet hangs from the top of its window's content.
                    f.origin = NSPoint(x: w.frame.midX - f.width / 2, y: w.contentLayoutRect.maxY + w.frame.minY - f.height)
                }
                guard w.frame.insetBy(dx: -1200, dy: -1200).intersects(f) else { continue }
                parts.append((v, f, isPopover(x)))
            }
            return compose(parts)
        }
        return rep.map { .success($0) } ?? .failure("drawing failed")
    }

    private static func isPopover(_ w: NSWindow) -> Bool {
        String(describing: type(of: w)).contains("Popover") || w.identifier?.rawValue == "starting-popover"
    }

    /// The parts drawn at 2x on one canvas, at their places (screen points, y up). A popover gets a stand-in of its
    /// background (its material is the window server's) and its rounded outline.
    private static func compose(_ parts: [(NSView, NSRect, Bool)]) -> NSBitmapImageRep? {
        guard var canvas = parts.first?.1 else { return nil }
        for part in parts.dropFirst() { canvas = canvas.union(part.1) }
        let images = parts.map { part -> (NSBitmapImageRep?, NSRect, Bool, NSAppearance?) in
            part.0.layoutSubtreeIfNeeded()
            return (VibrancyPass.image(of: part.0), part.1, part.2, part.0.window?.effectiveAppearance)
        }
        if parts.count == 1, !parts[0].2 { return images[0].0 }
        guard let out = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(canvas.width * 2), pixelsHigh: Int(canvas.height * 2),
                                         bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                                         colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0) else { return nil }
        // In points before the context is made: its drawing is then at 2x.
        out.size = canvas.size
        guard let ctx = NSGraphicsContext(bitmapImageRep: out) else { return nil }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = ctx
        for (rep, frame, popover, appearance) in images {
            guard let rep else { continue }
            let at = NSRect(x: frame.minX - canvas.minX, y: frame.minY - canvas.minY, width: frame.width, height: frame.height)
            if popover {
                let shape = NSBezierPath(roundedRect: at.insetBy(dx: 0.5, dy: 0.5), xRadius: 10, yRadius: 10)
                NSGraphicsContext.saveGraphicsState()
                shape.addClip()
                (appearance ?? NSApp.effectiveAppearance).performAsCurrentDrawingAppearance {
                    NSColor.windowBackgroundColor.setFill()
                    at.fill()
                    rep.draw(in: at, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: false, hints: nil)
                    NSColor.separatorColor.setStroke()
                    shape.lineWidth = 1
                    shape.stroke()
                }
                NSGraphicsContext.restoreGraphicsState()
            } else {
                rep.draw(in: at, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: false, hints: nil)
            }
        }
        NSGraphicsContext.restoreGraphicsState()
        return out
    }

    // MARK: Menus

    private static func menu(_ scene: Scene) async -> Result<NSBitmapImageRep, ShotError> {
        let panels: [(MenuPanel, NSPoint)]
        var prepare: () -> Void = {}
        if let menu = scene.menu {
            panels = MenuPanel.withSubmenus(menu.items, minWidth: 200)
        } else if let status = scene.app.statusMenuController, status.qaMenuOpen {
            panels = [(MenuPanel(items: status.qaItems, minWidth: 220), .zero)]
            let hover = scene.menuHover
            prepare = { status.footerRow?.qaHighlight(hover) }
        } else {
            return .failure("no menu is open")
        }
        let rep = await MenuPicture.image(panels: panels, caption: nil, appearance: NSApp.effectiveAppearance, prepare: prepare)
        return rep.map { .success($0) } ?? .failure("drawing failed")
    }
}

/// Vibrancy drawn in the app. A vibrant layer (sidebar labels, icons, the selected row) carries a colour matrix the
/// window server applies as it composites the window; an in-process drawing (`cacheDisplay`) skips it and gets the
/// raw colours (black). Here the view is drawn with its vibrant layers hidden, then each of them on its own, through
/// its colour matrix, over it in drawing order.
@MainActor enum VibrancyPass {
    private struct Vibrant {
        let layer: CALayer
        /// CAColorMatrix: rows for red, green, blue and alpha, each 4 factors (r, g, b, a) and a bias.
        let matrix: [Float]
        /// Where it is drawn, in the root layer's points, and the part of it its clipping ancestors leave.
        let frame: CGRect
        let visible: CGRect
        /// Its y axis runs the other way than the root's.
        let flipped: Bool
    }

    static func image(of view: NSView) -> NSBitmapImageRep? {
        guard let root = view.layer else { return SnapshotWindow.image(of: view) }
        var found: [Vibrant] = []
        collect(root, root: root, clip: root.bounds, into: &found)
        guard !found.isEmpty else { return SnapshotWindow.image(of: view) }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        let hidden = found.map(\.layer.isHidden)
        for v in found { v.layer.isHidden = true }
        let base = SnapshotWindow.image(of: view)
        for (v, h) in zip(found, hidden) { v.layer.isHidden = h }
        CATransaction.commit()
        guard let base, let ctx = NSGraphicsContext(bitmapImageRep: base) else { return base }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = ctx
        let cg = ctx.cgContext
        for v in found {
            guard let image = render(v) else { continue }
            cg.saveGState()
            cg.clip(to: v.visible)
            cg.draw(image, in: v.frame)
            cg.restoreGState()
        }
        NSGraphicsContext.restoreGraphicsState()
        return base
    }

    private static func collect(_ layer: CALayer, root: CALayer, clip: CGRect, into found: inout [Vibrant]) {
        guard !layer.isHidden, layer.opacity > 0 else { return }
        let frame = root.convert(layer.bounds, from: layer)
        var clip = clip
        if layer.masksToBounds { clip = clip.intersection(frame) }
        guard !clip.isEmpty else { return }
        // As its compositing filter (icons, the selection) or among its filters (text).
        let filters = ([layer.compositingFilter].compactMap { $0 } + (layer.filters ?? [])).compactMap { $0 as? NSObject }
        if let filter = filters.first(where: { "\($0)".contains("ColorMatrix") }),
           let value = filter.value(forKey: "inputColorMatrix") as? NSValue {
            var m = [Float](repeating: 0, count: 20)
            m.withUnsafeMutableBytes { value.getValue($0.baseAddress!, size: 80) }
            let a = root.convert(CGPoint(x: layer.bounds.minX, y: layer.bounds.minY), from: layer)
            let b = root.convert(CGPoint(x: layer.bounds.minX, y: layer.bounds.minY + 1), from: layer)
            found.append(Vibrant(layer: layer, matrix: m, frame: frame, visible: clip, flipped: b.y < a.y))
            return
        }
        for s in layer.sublayers ?? [] { collect(s, root: root, clip: clip, into: &found) }
    }

    /// The layer alone at 2x, its colours put through its matrix (on unpremultiplied colours).
    private static func render(_ v: Vibrant) -> CGImage? {
        let b = v.layer.bounds
        let w = Int((b.width * 2).rounded(.up)), h = Int((b.height * 2).rounded(.up))
        guard w > 0, h > 0, w * h < 16_000_000, let space = CGColorSpace(name: CGColorSpace.sRGB),
              let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4, space: space,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
        ctx.scaleBy(x: 2, y: 2)
        if v.flipped {
            ctx.translateBy(x: 0, y: b.height)
            ctx.scaleBy(x: 1, y: -1)
        }
        ctx.translateBy(x: -b.minX, y: -b.minY)
        v.layer.render(in: ctx)
        guard let data = ctx.data else { return nil }
        let px = data.bindMemory(to: UInt8.self, capacity: w * h * 4)
        let m = v.matrix
        for i in stride(from: 0, to: w * h * 4, by: 4) {
            let a = Float(px[i + 3]) / 255
            guard a > 0 else { continue }
            let r = Float(px[i]) / 255 / a, g = Float(px[i + 1]) / 255 / a, bl = Float(px[i + 2]) / 255 / a
            func row(_ k: Int) -> Float { min(1, max(0, m[k] * r + m[k + 1] * g + m[k + 2] * bl + m[k + 3] * a + m[k + 4])) }
            let na = row(15)
            px[i] = UInt8(row(0) * na * 255)
            px[i + 1] = UInt8(row(5) * na * 255)
            px[i + 2] = UInt8(row(10) * na * 255)
            px[i + 3] = UInt8(na * 255)
        }
        return ctx.makeImage()
    }
}
