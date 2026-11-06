import AppKit
import ObjectiveC

/// QA-control runs only (`--qa-control`, see QAControl): the app stays out of the user's way. It never becomes the
/// active or frontmost app, so it never takes the focus or a key the user types; no window of it is ever on a screen;
/// it has no menu bar item, no Dock icon and posts no notification. The QA harness draws what it shows in-process
/// (`shot`, QAShots), which needs neither an awake display nor the Screen Recording permission.
///
/// The rules hold for every window, also the ones AppKit and SwiftUI make (alerts, sheets, popovers), because they
/// are put into NSWindow and NSApplication themselves, once, before the first window exists (main.swift):
///  - a window that would touch a screen is moved far off every screen, by one fixed offset, so windows keep their
///    places relative to each other (a sheet over its window, a popover at its row); none is held back to a screen;
///  - every window draws as the key and main window (accent colours, not the grey inactive look) without ever being
///    key, as the snapshot renderer's windows do (SnapshotWindow);
///  - activating the app does nothing.
/// A normal run never calls `start()`, and nothing here changes it.
enum QuietRun {
    /// Set once at launch, before the first window (main.swift), and read on the main thread only.
    nonisolated(unsafe) private(set) static var on = false

    /// How far a window that would touch a screen is moved: past every screen of any arrangement of displays.
    static let shift = CGSize(width: -40_000, height: -40_000)

    @MainActor static func start() {
        guard !on else { return }
        on = true
        QuietHooks.install()
    }

    /// `frame` moved off every screen when it touches one of `screens`; else as it is.
    static func offscreen(_ frame: NSRect, screens: [NSRect]) -> NSRect {
        guard frame.width > 0, frame.height > 0, screens.contains(where: { $0.intersects(frame) }) else { return frame }
        return frame.offsetBy(dx: shift.width, dy: shift.height)
    }

    /// Where a window moved off the screens would be on them (Get Info measures the room around a row there), when the
    /// run is quiet; else `frame` itself.
    static func onscreen(_ frame: NSRect, screens: [NSRect]) -> NSRect {
        guard on else { return frame }
        let back = frame.offsetBy(dx: -shift.width, dy: -shift.height)
        return screens.contains(where: { $0.intersects(back) }) ? back : frame
    }

    /// The windows told they are key (`drawActive`).
    @MainActor private static let toldKey = NSHashTable<NSWindow>.weakObjects()

    /// `w` drawn in its key look from the first shot on. Its frame (the window buttons) and the vibrancy of its sidebar
    /// follow the key changes they are told of, not `isKeyWindow`: a window that never became key drew grey buttons
    /// and a dim, inactive sidebar until something else (a field taking the keyboard) made them look again, so a
    /// script's first shot showed another state than its later ones. It is told once that it became key and main;
    /// nothing reaches the window server, so no window takes the keys.
    @MainActor static func drawActive(_ w: NSWindow) {
        guard on, !toldKey.contains(w) else { return }
        toldKey.add(w)
        w.becomeMain()
        w.becomeKey()
    }

    /// The Dock icon the app would have (it shows none in a quiet run): QA's `policy` reports it.
    nonisolated(unsafe) static var policy: NSApplication.ActivationPolicy = .accessory

    /// Where a popover was shown from: the view, the rect in it and the preferred edge.
    final class Anchor: NSObject {
        weak var view: NSView?
        let rect: NSRect
        /// NSRectEdge's value: 0 minX, 1 minY, 2 maxX, 3 maxY.
        let edge: UInt
        init(view: NSView, rect: NSRect, edge: UInt) { self.view = view; self.rect = rect; self.edge = edge & 0xF }
    }

    /// The popovers shown in this run and their anchors (weak: a closed popover drops out).
    @MainActor static let anchors = NSMapTable<NSPopover, Anchor>.weakToStrongObjects()

    /// Where the popover window `w` belongs, beside its anchor as AppKit places it on a screen (the content `size`
    /// centred on the anchor, past the arrow), in screen points; nil when it was not shown from a view of a window.
    @MainActor static func placeBesideAnchor(_ w: NSWindow, size: NSSize) -> NSRect? {
        guard let e = anchors.keyEnumerator().allObjects as? [NSPopover],
              let p = e.first(where: { $0.contentViewController?.view.window === w }),
              let a = anchors.object(forKey: p), let v = a.view, let win = v.window else { return nil }
        let r = win.convertToScreen(v.convert(a.rect, to: nil))
        let arrow: CGFloat = 13
        // The edge is in the view's own coordinates: in a flipped view minY is its top.
        var edge = a.edge
        if v.isFlipped, edge == 1 || edge == 3 { edge = 4 - edge }
        switch edge {
        case 3: return NSRect(x: r.midX - size.width / 2, y: r.maxY + arrow, width: size.width, height: size.height)
        case 0: return NSRect(x: r.minX - arrow - size.width, y: r.midY - size.height / 2, width: size.width, height: size.height)
        case 2: return NSRect(x: r.maxX + arrow, y: r.midY - size.height / 2, width: size.width, height: size.height)
        default: return NSRect(x: r.midX - size.width / 2, y: r.minY - arrow - size.height, width: size.width, height: size.height)
        }
    }
}

/// The NSWindow and NSApplication methods a quiet run replaces (see QuietRun). Each replacement calls the original
/// with a frame kept off the screens, or answers on its own.
@MainActor private enum QuietHooks {
    nonisolated(unsafe) static var originals: [Selector: IMP] = [:]

    static func install() {
        let w: AnyClass = NSWindow.self
        let active: @convention(block) (NSWindow) -> Bool = { _ in true }
        replace(w, #selector(getter: NSWindow.isKeyWindow), active)
        replace(w, #selector(getter: NSWindow.isMainWindow), active)
        let unconstrained: @convention(block) (NSWindow, NSRect, NSScreen?) -> NSRect = { _, r, _ in r }
        replace(w, #selector(NSWindow.constrainFrameRect(_:to:)), unconstrained)

        let setFrame = #selector(NSWindow.setFrame(_:display:))
        let setFrameBlock: @convention(block) (NSWindow, NSRect, Bool) -> Void = { win, r, display in
            typealias F = @convention(c) (NSWindow, Selector, NSRect, Bool) -> Void
            unsafeBitCast(original(setFrame), to: F.self)(win, setFrame, keptOff(r), display)
        }
        replace(w, setFrame, setFrameBlock)

        let animate = #selector(NSWindow.setFrame(_:display:animate:))
        let animateBlock: @convention(block) (NSWindow, NSRect, Bool, Bool) -> Void = { win, r, display, anim in
            typealias F = @convention(c) (NSWindow, Selector, NSRect, Bool, Bool) -> Void
            unsafeBitCast(original(animate), to: F.self)(win, animate, keptOff(r), display, anim)
        }
        replace(w, animate, animateBlock)

        let origin = #selector(NSWindow.setFrameOrigin(_:))
        let originBlock: @convention(block) (NSWindow, NSPoint) -> Void = { win, p in
            typealias F = @convention(c) (NSWindow, Selector, NSPoint) -> Void
            unsafeBitCast(original(origin), to: F.self)(win, origin, keptOff(NSRect(origin: p, size: win.frame.size)).origin)
        }
        replace(w, origin, originBlock)

        let topLeft = #selector(NSWindow.setFrameTopLeftPoint(_:))
        let topLeftBlock: @convention(block) (NSWindow, NSPoint) -> Void = { win, p in
            typealias F = @convention(c) (NSWindow, Selector, NSPoint) -> Void
            let size = win.frame.size
            let r = keptOff(NSRect(x: p.x, y: p.y - size.height, width: size.width, height: size.height))
            unsafeBitCast(original(topLeft), to: F.self)(win, topLeft, NSPoint(x: r.minX, y: r.maxY))
        }
        replace(w, topLeft, topLeftBlock)

        // Ordering in: whatever placed the window before (center(), a saved frame, NSAlert) is checked once more.
        let order = #selector(NSWindow.order(_:relativeTo:))
        let orderBlock: @convention(block) (NSWindow, Int, Int) -> Void = { win, place, other in
            typealias F = @convention(c) (NSWindow, Selector, Int, Int) -> Void
            if place != NSWindow.OrderingMode.out.rawValue { keepOff(win) }
            unsafeBitCast(original(order), to: F.self)(win, order, place, other)
        }
        replace(w, order, orderBlock)

        let regardless = #selector(NSWindow.orderFrontRegardless)
        let regardlessBlock: @convention(block) (NSWindow) -> Void = { win in
            typealias F = @convention(c) (NSWindow, Selector) -> Void
            keepOff(win)
            unsafeBitCast(original(regardless), to: F.self)(win, regardless)
        }
        replace(w, regardless, regardlessBlock)

        // Never the active app: nothing comes forward, nothing takes the keys. It draws as the active one all the same
        // (the window buttons in colour, controls in the accent colour).
        let a: AnyClass = NSApplication.self
        let isActive: @convention(block) (NSApplication) -> Bool = { _ in true }
        replace(a, #selector(getter: NSApplication.isActive), isActive)
        // Materials (the sidebar's glass, menus, popovers) are composited by the window server from what is behind a
        // window on screen, which a window off every screen does not have: they draw as with "Reduce transparency",
        // a solid colour drawn in the app, and their content without vibrancy.
        let reduce: @convention(block) (NSWorkspace) -> Bool = { _ in true }
        replace(NSWorkspace.self, #selector(getter: NSWorkspace.accessibilityDisplayShouldReduceTransparency), reduce)
        let ignoring: @convention(block) (NSApplication, Bool) -> Void = { _, _ in }
        replace(a, NSSelectorFromString("activateIgnoringOtherApps:"), ignoring)
        let plain: @convention(block) (NSApplication) -> Void = { _ in }
        replace(a, NSSelectorFromString("activate"), plain)

        // Popovers: where each was anchored. AppKit keeps a popover on a screen, so one anchored to a view off every
        // screen comes up at a screen's edge and is then moved off by the fixed offset, far from its anchor; a shot
        // puts it back beside the anchor (QAShots, `QuietRun.anchor(of:)`).
        let show = #selector(NSPopover.show(relativeTo:of:preferredEdge:))
        let showBlock: @convention(block) (NSPopover, NSRect, NSView, UInt) -> Void = { p, r, v, e in
            MainActor.assumeIsolated { QuietRun.anchors.setObject(QuietRun.Anchor(view: v, rect: r, edge: e), forKey: p) }
            typealias F = @convention(c) (NSPopover, Selector, NSRect, NSView, UInt) -> Void
            unsafeBitCast(original(show), to: F.self)(p, show, r, v, e)
        }
        replace(NSPopover.self, show, showBlock)
    }

    private static func replace(_ cls: AnyClass, _ sel: Selector, _ block: Any) {
        guard let m = class_getInstanceMethod(cls, sel) else { return }
        originals[sel] = method_setImplementation(m, imp_implementationWithBlock(block))
    }

    private static func original(_ sel: Selector) -> IMP { originals[sel]! }

    private nonisolated static func screens() -> [NSRect] {
        MainActor.assumeIsolated { NSScreen.screens.map(\.frame) }
    }

    private nonisolated static func keptOff(_ r: NSRect) -> NSRect { QuietRun.offscreen(r, screens: screens()) }

    private nonisolated static func keepOff(_ win: NSWindow) {
        let f = MainActor.assumeIsolated { win.frame }
        let r = keptOff(f)
        if r != f { MainActor.assumeIsolated { win.setFrameOrigin(r.origin) } }
    }
}
