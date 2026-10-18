import AppKit

/// The menu's last row: "Open Seedwell…" on the left and "Quit" on the right, each with its own
/// menu-style highlight. A view item cannot carry key equivalents, so ⌘L and ⌘Q live on hidden items
/// next to it (`allowsKeyEquivalentWhenHidden`).
@MainActor final class FooterRow: NSView {
    private let open: FooterButton
    private let quit: FooterButton

    init(openTitle: String, onOpen: @escaping @MainActor () -> Void, onQuit: @escaping @MainActor () -> Void) {
        open = FooterButton(title: openTitle, run: onOpen)
        quit = FooterButton(title: "Quit", run: onQuit)
        super.init(frame: NSRect(x: 0, y: 0, width: 220, height: Self.height))
        autoresizingMask = .width
        open.toolTip = "Open the library window"
        quit.toolTip = "Quit \(AppInfo.name) (trackers are told it stopped)"
        addSubview(open)
        addSubview(quit)
        setAccessibilityRole(.group)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    /// Height of a plain menu row.
    static var height: CGFloat { ceil(NSFont.menuFont(ofSize: 0).boundingRectForFont.height) + 8 }
    /// Inset of the highlight from the menu's edge, and of the text inside it (matches the plain rows).
    private static let edge: CGFloat = 5
    static let pad: CGFloat = 11

    override func layout() {
        super.layout()
        let w = bounds.width - 2 * Self.edge
        let qw = min(quit.fitting + 2 * Self.pad, w / 2)
        quit.frame = NSRect(x: bounds.maxX - Self.edge - qw, y: 0, width: qw, height: bounds.height)
        open.frame = NSRect(x: Self.edge, y: 0, width: w - qw - 4, height: bounds.height)
    }

    override var intrinsicContentSize: NSSize {
        NSSize(width: Self.edge * 2 + open.fitting + quit.fitting + 4 * Self.pad + 24, height: Self.height)
    }

    /// QA: draws one side highlighted, as a hover would ("open", "quit" or nil).
    func qaHighlight(_ side: String?) {
        open.hovered = side == "open"
        quit.hovered = side == "quit"
    }

    /// QA: presses one side as a click would (through the accessibility press, the same path).
    func qaPress(_ side: String) {
        _ = (side == "quit" ? quit : open).accessibilityPerformPress()
    }
}

@MainActor final class FooterButton: NSView {
    private let title: String
    private let run: @MainActor () -> Void
    var hovered = false { didSet { if hovered != oldValue { needsDisplay = true } } }

    init(title: String, run: @escaping @MainActor () -> Void) {
        self.title = title
        self.run = run
        super.init(frame: .zero)
        setAccessibilityElement(true)
        setAccessibilityRole(.menuItem)
        setAccessibilityLabel(title)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    var fitting: CGFloat { ceil((title as NSString).size(withAttributes: attributes(highlighted: false)).width) }

    private func attributes(highlighted: Bool) -> [NSAttributedString.Key: Any] {
        [.font: NSFont.menuFont(ofSize: 0),
         .foregroundColor: highlighted ? NSColor.selectedMenuItemTextColor : NSColor.labelColor]
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        for a in trackingAreas { removeTrackingArea(a) }
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .mouseMoved, .activeAlways, .inVisibleRect], owner: self))
    }

    /// The highlight follows the pointer's real position, never an event that was missed: it is off at
    /// rest, on only while the pointer is inside, and off again the moment it leaves or the menu closes.
    private var pointerInside: Bool {
        guard let w = window else { return false }
        return bounds.contains(convert(w.mouseLocationOutsideOfEventStream, from: nil))
    }
    override func mouseEntered(with event: NSEvent) { hovered = pointerInside }
    override func mouseMoved(with event: NSEvent) { hovered = pointerInside }
    override func mouseExited(with event: NSEvent) { hovered = false }
    override func viewDidMoveToWindow() { hovered = false }
    override func viewWillMove(toWindow newWindow: NSWindow?) { if newWindow == nil { hovered = false } }

    override func mouseUp(with event: NSEvent) {
        guard bounds.contains(convert(event.locationInWindow, from: nil)) else { return }
        fire()
    }

    override func accessibilityPerformPress() -> Bool { fire(); return true }

    private func fire() {
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
        (title as NSString).draw(at: NSPoint(x: FooterRow.pad, y: (bounds.height - size.height) / 2), withAttributes: attrs)
    }
}
