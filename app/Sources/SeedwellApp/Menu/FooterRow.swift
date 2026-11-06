import AppKit

/// The menu's last row: the torrent proxy's shield (SPEC §7H), "Open Seedwell…" beside it and "Quit" on the right,
/// each with its own menu-style highlight. A view item cannot carry key equivalents, so ⌘L and ⌘Q live on hidden items
/// next to it (`allowsKeyEquivalentWhenHidden`).
@MainActor final class FooterRow: NSView {
    private let open: FooterButton
    private let quit: FooterButton
    /// The proxy's shield, where the other rows have their images (nil: a picture of the menu without the proxy).
    let shield: MenuProxyShield?

    init(openTitle: String, proxy: ProxyCenter? = nil, onOpen: @escaping @MainActor () -> Void, onQuit: @escaping @MainActor () -> Void) {
        shield = proxy.map { MenuProxyShield(center: $0) }
        // Beside the shield the title keeps a smaller inset: the shield's square already sets it off from the edge.
        open = FooterButton(title: openTitle, pad: proxy == nil ? Self.pad : 6, run: onOpen)
        quit = FooterButton(title: "Quit", run: onQuit)
        super.init(frame: NSRect(x: 0, y: 0, width: 220, height: Self.height))
        autoresizingMask = .width
        open.toolTip = "Open the library window"
        quit.toolTip = "Quit \(AppInfo.name) (trackers are told it stopped)"
        if let shield { addSubview(shield) }
        addSubview(open)
        addSubview(quit)
        setAccessibilityRole(.group)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    /// Height of a plain menu row.
    static var height: CGFloat { ceil(Theme.NS.menu.boundingRectForFont.height) + 8 }
    /// Inset of the highlight from the menu's edge, and of the text inside it (matches the plain rows).
    private static let edge: CGFloat = 5
    static let pad: CGFloat = 11

    /// The shield's square: a little wider than the row is high, around the menu's 16-point image size.
    private static var shieldWidth: CGFloat { height + 4 }

    override func layout() {
        super.layout()
        let w = bounds.width - 2 * Self.edge
        let qw = min(quit.fitting + 2 * Self.pad, w / 2)
        quit.frame = NSRect(x: bounds.maxX - Self.edge - qw, y: 0, width: qw, height: bounds.height)
        let sw = shield == nil ? 0 : Self.shieldWidth
        shield?.frame = NSRect(x: Self.edge, y: 0, width: sw, height: bounds.height)
        let ox = Self.edge + (sw > 0 ? sw + 2 : 0)
        open.frame = NSRect(x: ox, y: 0, width: bounds.maxX - Self.edge - qw - 4 - ox, height: bounds.height)
    }

    override var intrinsicContentSize: NSSize {
        NSSize(width: Self.edge * 2 + (shield == nil ? 0 : Self.shieldWidth + 2) + open.fitting + quit.fitting + 4 * Self.pad + 24, height: Self.height)
    }

    /// The parts that light up under the pointer: the shield, Open and Quit.
    var hoverRows: [any HoverRow] { (shield.map { [$0] } ?? []) + [open, quit] }

    /// QA: draws one side highlighted, as a hover would ("shield", "open", "quit" or nil).
    func qaHighlight(_ side: String?) {
        shield?.animates = false
        shield?.hovered = side == "shield"
        shield?.animates = true
        open.hovered = side == "open"
        quit.hovered = side == "quit"
    }

    /// QA: presses one side: the call a click (and an accessibility press) on it makes.
    func qaPress(_ side: String) {
        (side == "quit" ? quit : open).fire()
    }
}

@MainActor final class FooterButton: NSView, HoverRow {
    private let title: String
    /// The text's inset from the highlight's edge.
    private let pad: CGFloat
    private let run: @MainActor () -> Void
    var hovered = false { didSet { if hovered != oldValue { needsDisplay = true } } }

    init(title: String, pad: CGFloat = FooterRow.pad, run: @escaping @MainActor () -> Void) {
        self.title = title
        self.pad = pad
        self.run = run
        super.init(frame: .zero)
        setAccessibilityElement(true)
        setAccessibilityRole(.menuItem)
        setAccessibilityLabel(title)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    var fitting: CGFloat { ceil((title as NSString).size(withAttributes: attributes(highlighted: false)).width) }

    private func attributes(highlighted: Bool) -> [NSAttributedString.Key: Any] {
        [.font: Theme.NS.menu,
         .foregroundColor: highlighted ? NSColor.selectedMenuItemTextColor : NSColor.labelColor]
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        for a in trackingAreas { removeTrackingArea(a) }
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .mouseMoved, .activeAlways, .inVisibleRect], owner: self))
        hovered = menuPointerInside
    }

    /// The highlight follows the pointer's real position, never an event that was missed: it is off at
    /// rest, on only while the pointer is inside, and off again the moment it leaves or the menu closes.
    override func mouseEntered(with event: NSEvent) { hovered = menuPointerInside }
    override func mouseMoved(with event: NSEvent) { hovered = menuPointerInside }
    override func mouseExited(with event: NSEvent) { hovered = false }
    override func viewDidMoveToWindow() { hovered = false }
    override func viewWillMove(toWindow newWindow: NSWindow?) { if newWindow == nil { hovered = false } }

    override func mouseUp(with event: NSEvent) {
        guard bounds.contains(convert(event.locationInWindow, from: nil)) else { return }
        fire()
    }

    override func accessibilityPerformPress() -> Bool { fire(); return true }

    /// What a click on this side does (also QA's `menu-press`).
    func fire() {
        hovered = false
        enclosingMenuItem?.menu?.cancelTracking()
        // After the menu has closed, as a plain item's action would run.
        DispatchQueue.main.async { [run] in MainActor.assumeIsolated { run() } }
    }

    override func draw(_ dirtyRect: NSRect) {
        if hovered {
            NSColor.selectedContentBackgroundColor.setFill()
            NSBezierPath(roundedRect: bounds.insetBy(dx: 0, dy: 1), xRadius: 5, yRadius: 5).fill()
        }
        let attrs = attributes(highlighted: hovered)
        let size = (title as NSString).size(withAttributes: attrs)
        (title as NSString).draw(at: NSPoint(x: pad, y: (bounds.height - size.height) / 2), withAttributes: attrs)
    }
}
