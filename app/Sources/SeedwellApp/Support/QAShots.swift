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
        QuietRun.drawActive(w)
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
/// raw colours (black). Here the view is drawn with its vibrant layers hidden, then each of them over it in drawing
/// order the way Core Animation composites them: the matrix works on what is under the layer (its backdrop), and the
/// layer's own drawing gives only where and how much (its coverage), never a colour (`SidebarDrawingTests` checks
/// that it draws every vibrant layer, from what is under it). Secondary text "+0.3" is 0.3 lighter than whatever it
/// is on, so a word on a grey pill in a grey selected row stays readable; put through the matrix on its own (black +
/// 0.3, one fixed grey) it took the grey of the pill, and the sidebar's "agents" pill came out empty in the shots.
///
/// The opacity of the layers a vibrant layer is in (SwiftUI's `.compositingGroup().opacity()`) scales its coverage as
/// its own does. Each macOS builds these layers its own way (macOS 27 gives the sidebar "+0.3" at full alpha, macOS 26
/// "white at 0.28/0.6/0.95 alpha"), so the pass follows the layers and their matrices, not one macOS.
@MainActor enum VibrancyPass {
    private struct Vibrant {
        let layer: CALayer
        /// CAColorMatrix: rows for red, green, blue and alpha, each 4 factors (r, g, b, a) and a bias.
        let matrix: [Float]
        /// The opacity of the layers it is in, below the root (its own is in its drawing).
        let group: Float
        /// The matrix is the layer's compositing filter (icons, the selection), not one of its filters (text).
        let compositing: Bool
        /// Where it is drawn, in the root layer's points, and the part of it its clipping ancestors leave.
        let frame: CGRect
        let visible: CGRect
        /// Its y axis runs the other way than the root's.
        let flipped: Bool
    }

    static func image(of view: NSView) -> NSBitmapImageRep? {
        guard let root = view.layer else { return SnapshotWindow.image(of: view) }
        var found: [Vibrant] = []
        collect(root, root: root, clip: root.bounds, group: 1, into: &found)
        guard !found.isEmpty else { return SnapshotWindow.image(of: view) }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        let hidden = found.map(\.layer.isHidden)
        for v in found { v.layer.isHidden = true }
        let base = SnapshotWindow.image(of: view)
        for (v, h) in zip(found, hidden) { v.layer.isHidden = h }
        CATransaction.commit()
        guard let base, let data = base.bitmapData, base.bitsPerPixel == 32, base.samplesPerPixel == 4, !base.isPlanar,
              base.size.width > 0 else { return base }
        let width = base.pixelsWide, height = base.pixelsHigh, scale = CGFloat(width) / base.size.width
        // Each layer's coverage, on a canvas of the picture's size with the same places (y up, in points).
        guard let space = CGColorSpace(name: CGColorSpace.sRGB),
              let coverage = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: width * 4, space: space,
                                       bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue),
              let mask = coverage.data?.bindMemory(to: UInt8.self, capacity: width * height * 4) else { return base }
        coverage.scaleBy(x: scale, y: scale)
        for v in found {
            // A compositing filter other than a plain one inside a layer with an opacity: Core Animation draws nothing.
            if v.compositing, v.group < 1, !plain(v.matrix) { continue }
            let area = v.visible.intersection(v.frame)
            guard !area.isEmpty, let image = render(v) else { continue }
            coverage.saveGState()
            coverage.clip(to: area)
            coverage.clear(area)
            coverage.draw(image, in: v.frame)
            coverage.restoreGState()
            // The pixels of `area`, rows counted from the top as in memory.
            let x0 = max(0, Int((area.minX * scale).rounded(.down))), x1 = min(width, Int((area.maxX * scale).rounded(.up)))
            let r0 = max(0, height - Int((area.maxY * scale).rounded(.up))), r1 = min(height, height - Int((area.minY * scale).rounded(.down)))
            guard x0 < x1, r0 < r1 else { continue }
            for row in r0..<r1 {
                for x in x0..<x1 {
                    let a = mask[(row * width + x) * 4 + 3]
                    guard a > 0 else { continue }
                    let i = row * base.bytesPerRow + x * 4
                    var px = (data[i], data[i + 1], data[i + 2], data[i + 3])
                    blend(&px, coverage: Float(a) / 255 * v.group, matrix: v.matrix, held: v.compositing && v.group < 1)
                    (data[i], data[i + 1], data[i + 2], data[i + 3]) = px
                }
            }
        }
        return base
    }

    /// A pixel of the picture (premultiplied RGBA) under a vibrant layer that covers `coverage` of it: the pixel moved
    /// towards the layer's matrix applied to it, by the alpha the matrix gives times the coverage. As Core Animation
    /// does it, a plain matrix (each colour itself plus a bias) has its colour held at 1 before the mix, so "white at
    /// 0.6" over a mid grey is 0.6 of the way to white, not grey + 0.6, and "+0.3" at a light letter's soft edge is
    /// not white; it is not held at 0, so half a pixel of "-0.3" over a dark grey is black, not half way to it. Other
    /// matrices mix their colour as it comes. `held`: a compositing filter inside a layer with an opacity, whose
    /// colour Core Animation holds between 0 and 1 (half of "-0.3" over a dark grey is then half way to black). Only
    /// the result is clamped.
    static func blend(_ px: inout (UInt8, UInt8, UInt8, UInt8), coverage: Float, matrix m: [Float], held: Bool = false) {
        let da = Float(px.3) / 255
        let unpremultiply: Float = da > 0 ? 1 / (da * 255) : 0
        let d = (Float(px.0) * unpremultiply, Float(px.1) * unpremultiply, Float(px.2) * unpremultiply)
        func row(_ k: Int) -> Float { m[k] * d.0 + m[k + 1] * d.1 + m[k + 2] * d.2 + m[k + 3] * da + m[k + 4] }
        let k = min(1, max(0, row(15))) * coverage
        guard k > 0 else { return }
        let low: Float = held ? 0 : -.infinity, high: Float = held || plain(m) ? 1 : .infinity
        // Source over, premultiplied: the matrix's colour with alpha k over the pixel.
        func over(_ c: Float, _ under: UInt8) -> UInt8 {
            UInt8(min(255, max(0, min(high, max(low, c)) * k * 255 + Float(under) * (1 - k))).rounded())
        }
        px = (over(row(0), px.0), over(row(5), px.1), over(row(10), px.2), UInt8(min(255, (k + da * (1 - k)) * 255).rounded()))
    }

    /// Each colour is itself plus a bias (and the alpha any factor).
    private static func plain(_ m: [Float]) -> Bool {
        m[0] == 1 && m[6] == 1 && m[12] == 1 && [1, 2, 3, 5, 7, 8, 10, 11, 13].allSatisfy { m[$0] == 0 }
    }

    /// `group`: the opacity of the layers `layer` is in, below the root (the view's drawing leaves out the root's own).
    private static func collect(_ layer: CALayer, root: CALayer, clip: CGRect, group: Float, into found: inout [Vibrant]) {
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
            found.append(Vibrant(layer: layer, matrix: m, group: group, compositing: filter === layer.compositingFilter as? NSObject,
                                 frame: frame, visible: clip, flipped: b.y < a.y))
            return
        }
        let inner = layer === root ? 1 : group * layer.opacity
        for s in layer.sublayers ?? [] { collect(s, root: root, clip: clip, group: inner, into: &found) }
    }

    /// The layer alone at 2x: its coverage (its colours are not used).
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
        return ctx.makeImage()
    }
}
