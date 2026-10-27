import AppKit

// The menu bar's inline file list (SPEC §8.2): a multi-file torrent's row has a disclosure chevron; it opens
// a checklist right under the row, and ticking a file keeps the menu open. The list is fetched when it opens
// and dropped when the menu closes.

extension TransferRow {
    /// The row gets the chevron for its inline file list: a multi-file torrent that is not held for the user's choice
    /// (that has the chooser).
    var offersMenuFiles: Bool { kind == .torrent && fileCount > 1 && pending == nil && !awaitingChoice }
}

// MARK: Hover

/// A custom menu row that draws its own hover highlight. The controller keeps at most one of them lit
/// (`StatusMenuController.refreshHover`).
@MainActor protocol HoverRow: NSView {
    var hovered: Bool { get set }
}

/// The checkbox of the menu's rows (torrent rows and file rows alike), drawn rather than an NSButton so it
/// follows the row's own highlight: an accent-filled rounded box with a bold white tick when on (a dash when
/// mixed), a softly filled box with a clear outline when off, so on and off read at a glance.
enum MenuCheckbox {
    static let size: CGFloat = 14

    static func draw(at origin: NSPoint, state: FileCheck, hovered: Bool) {
        let box = NSRect(origin: origin, size: NSSize(width: size, height: size))
        let path = NSBezierPath(roundedRect: box.insetBy(dx: 0.5, dy: 0.5), xRadius: 4, yRadius: 4)
        // On a lit row the accent is the background: the box turns white there.
        if state == .off {
            (hovered ? NSColor.selectedMenuItemTextColor.withAlphaComponent(0.18) : NSColor.quaternaryLabelColor).setFill()
            path.fill()
            (hovered ? NSColor.selectedMenuItemTextColor : NSColor.secondaryLabelColor).setStroke()
            path.lineWidth = 1.2
            path.stroke()
            return
        }
        let mark: NSColor
        if hovered {
            NSColor.selectedMenuItemTextColor.setFill(); mark = .selectedContentBackgroundColor
            path.fill()
        } else {
            let accent = NSColor.controlAccentColor
            let top = accent.blended(withFraction: 0.18, of: .white) ?? accent
            NSGradient(starting: top, ending: accent)?.draw(in: path, angle: -90)
            mark = .white
        }
        mark.setStroke()
        let m = NSBezierPath()
        m.lineWidth = 2; m.lineCapStyle = .round; m.lineJoinStyle = .round
        if state == .on {
            m.move(to: NSPoint(x: box.minX + 3.4, y: box.midY))
            m.line(to: NSPoint(x: box.minX + 5.9, y: box.minY + 3.6))
            m.line(to: NSPoint(x: box.maxX - 3.2, y: box.maxY - 3.6))
        } else {
            m.move(to: NSPoint(x: box.minX + 3.6, y: box.midY))
            m.line(to: NSPoint(x: box.maxX - 3.6, y: box.midY))
        }
        m.stroke()
    }
}

extension NSView {
    /// Whether the pointer is over the visible part of this view. A row scrolled out of its list is clipped, so
    /// its (off-screen) bounds must not count.
    var menuPointerInside: Bool {
        guard let w = window else { return false }
        return visibleRect.contains(convert(w.mouseLocationOutsideOfEventStream, from: nil))
    }
}

// MARK: Rows

/// A row of the checklist: [fold chevron] ✓ / − / empty, the name, the size on the right. A click on the
/// chevron folds a folder, a click elsewhere runs `run`; neither closes the menu.
@MainActor final class MenuCheckRow: NSView, HoverRow {
    struct Content: Equatable {
        var title: String
        var trailing = ""
        /// nil: a plain command ("Select All").
        var check: FileCheck?
        /// A folder: open or closed. nil for a file.
        var disclosure: Bool?
        var indent = 0
        /// A video file: a Play icon at the right edge.
        var playable = false
        /// The list has videos: the Play column is kept free in every row, so the sizes line up.
        var reservesPlay = false
        /// The file type's icon (the folder icon for a folder), as in the window's file list. nil for a command.
        var iconExt: String?
        /// The trailing text is a live rate (the Sharing line): the menu's font, the label colour while data
        /// flows (true), the secondary one when idle (false). nil: a size.
        var rateActive: Bool?
    }

    var content: Content { didSet { if content != oldValue { needsDisplay = true; refreshAccessibility() } } }
    var run: @MainActor () -> Void
    var onDisclosure: @MainActor () -> Void = {}
    var onPlay: @MainActor () -> Void = {}
    var hovered = false { didSet { if hovered != oldValue { needsDisplay = true } } }
    private static let edge: CGFloat = 5
    private static let step: CGFloat = 14

    init(content: Content, width: CGFloat, run: @escaping @MainActor () -> Void) {
        self.content = content
        self.run = run
        super.init(frame: NSRect(x: 0, y: 0, width: width, height: FooterRow.height))
        autoresizingMask = .width
        setAccessibilityElement(true)
        setAccessibilityRole(.menuItem)
        refreshAccessibility()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    private func refreshAccessibility() {
        setAccessibilityLabel(content.trailing.isEmpty ? content.title : "\(content.title), \(content.trailing)")
        switch content.check {
        case .on?: setAccessibilityValue("checked")
        case .mixed?: setAccessibilityValue("mixed")
        case .off?: setAccessibilityValue("unchecked")
        case nil: setAccessibilityValue(nil)
        }
    }

    private static var font: NSFont { .menuFont(ofSize: 0) }
    private static var small: NSFont { .menuFont(ofSize: NSFont.smallSystemFontSize) }
    /// File rows match the window's file list: smaller, quieter text and digits that line up.
    private static var fileFont: NSFont { .systemFont(ofSize: 12) }
    private static var fileSize: NSFont { .monospacedDigitSystemFont(ofSize: 10.5, weight: .regular) }

    /// Left edge of the fold chevron (a file reserves the space).
    private var chevronX: CGFloat { Self.edge + 6 + CGFloat(content.indent) * Self.step }
    /// The chevron's hit area: its column, 24 pt wide.
    private var disclosureZone: NSRect { NSRect(x: chevronX - 4, y: 0, width: 24, height: bounds.height) }
    /// The Play icon's column at the right edge, left of the menu's inset.
    private static let playColumn: CGFloat = 22
    private var playZone: NSRect { NSRect(x: bounds.maxX - Self.edge - 24 - Self.playColumn, y: 0, width: Self.playColumn + 4, height: bounds.height) }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        for a in trackingAreas { removeTrackingArea(a) }
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .mouseMoved, .activeAlways, .inVisibleRect], owner: self))
        // The areas were rebuilt (a scroll, a resize): the pointer may be on the other side of the edge now.
        hovered = menuPointerInside
    }

    override func mouseEntered(with event: NSEvent) { hovered = menuPointerInside }
    override func mouseMoved(with event: NSEvent) { hovered = menuPointerInside }
    override func mouseExited(with event: NSEvent) { hovered = false }
    override func viewDidMoveToWindow() { hovered = false }
    override func viewWillMove(toWindow newWindow: NSWindow?) { if newWindow == nil { hovered = false } }

    override func mouseUp(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        guard bounds.contains(p) else { return }
        if content.disclosure != nil, disclosureZone.contains(p) { onDisclosure() }
        else if content.playable, playZone.contains(p) { onPlay() }
        else { run() }
    }

    override func accessibilityPerformPress() -> Bool { run(); return true }

    override func draw(_ dirtyRect: NSRect) {
        if hovered {
            NSColor.selectedContentBackgroundColor.setFill()
            NSBezierPath(roundedRect: bounds.insetBy(dx: Self.edge, dy: 1), xRadius: 5, yRadius: 5).fill()
        }
        let isFile = content.check != nil
        let skippedIcon = content.check == .off && !hovered
        let ink: NSColor = hovered ? .selectedMenuItemTextColor : (isFile ? .secondaryLabelColor : .labelColor)
        let faint: NSColor = hovered ? .selectedMenuItemTextColor : (isFile ? .tertiaryLabelColor : .secondaryLabelColor)
        func symbol(_ name: String, size: CGFloat, color: NSColor, at x: CGFloat) -> CGFloat {
            let cfg = NSImage.SymbolConfiguration(pointSize: size, weight: .semibold).applying(.init(paletteColors: [color]))
            guard let img = NSImage(systemSymbolName: name, accessibilityDescription: nil)?.withSymbolConfiguration(cfg) else { return 0 }
            img.draw(at: NSPoint(x: x, y: (bounds.height - img.size.height) / 2), from: .zero, operation: .sourceOver, fraction: 1)
            return img.size.width
        }

        var x = Self.edge + 10
        if content.check != nil {
            // The fold chevron in the label colour, not the faint one: it is a control, and must be seen.
            if let open = content.disclosure {
                _ = symbol(open ? "chevron.down" : "chevron.right", size: 10, color: hovered ? .selectedMenuItemTextColor : .secondaryLabelColor, at: chevronX + 1)
            }
            x = chevronX + 18
            if let check = content.check {
                MenuCheckbox.draw(at: NSPoint(x: x, y: ((bounds.height - MenuCheckbox.size) / 2).rounded()), state: check, hovered: hovered)
            }
            x += 19
        }
        if let ext = content.iconExt {
            let s: CGFloat = 14
            FileIcons.icon(ext: ext).draw(in: NSRect(x: x, y: ((bounds.height - s) / 2).rounded(), width: s, height: s),
                                          from: .zero, operation: .sourceOver, fraction: skippedIcon ? 0.5 : 1)
            x += s + 5
        }

        // File rows sit in a scroll view whose overlay scroller and the menu's own inset eat into the
        // right edge: keep the size column clear of both.
        var right = bounds.maxX - Self.edge - (content.check != nil ? 24 : 9)
        if content.reservesPlay {
            if content.playable { _ = symbol(QAFlags.playGlyph, size: 9.5, color: hovered ? .selectedMenuItemTextColor : .secondaryLabelColor, at: right - 11) }
            right -= Self.playColumn
        }
        var textRight = right
        if !content.trailing.isEmpty {
            let attrs: [NSAttributedString.Key: Any] = if let active = content.rateActive {
                [.font: NSFont.monospacedDigitSystemFont(ofSize: Self.font.pointSize, weight: .regular),
                 .foregroundColor: hovered ? NSColor.selectedMenuItemTextColor : active ? NSColor.labelColor : NSColor.secondaryLabelColor]
            } else {
                [.font: isFile ? Self.fileSize : Self.small, .foregroundColor: faint]
            }
            let size = (content.trailing as NSString).size(withAttributes: attrs)
            (content.trailing as NSString).draw(at: NSPoint(x: right - size.width, y: (bounds.height - size.height) / 2), withAttributes: attrs)
            textRight = right - size.width - 12
        }
        let style = NSMutableParagraphStyle()
        style.lineBreakMode = .byTruncatingMiddle
        let skipped = content.check == .off
        let attrs: [NSAttributedString.Key: Any] = [.font: isFile ? Self.fileFont : Self.font, .foregroundColor: skipped && !hovered ? faint : ink, .paragraphStyle: style]
        let h = (content.title as NSString).size(withAttributes: attrs).height
        (content.title as NSString).draw(in: NSRect(x: x, y: (bounds.height - h) / 2, width: max(10, textRight - x), height: h), withAttributes: attrs)
    }
}

/// The torrent's own row in the menu when it has a file list: a native-looking two-line row (ring, name,
/// grey line) with a chevron at the left edge when it has a file list, a check (running / paused) for a
/// download and a Play icon at the right edge for a video. The chevron (24 pt hit area) opens the list, the
/// check pauses or resumes, the Play icon plays; elsewhere a Recent row reveals the file, a held row
/// opens its chooser and a download row does nothing (its commands are on the right-click menu). It takes its texts from the NSMenuItem the controller keeps up to date (`sync`).
@MainActor final class MenuTorrentRow: NSView, HoverRow {
    private var title = ""
    private var subtitle = ""
    private var image: NSImage?
    private var dimmed = false
    /// A chevron at the left edge opens the file list.
    var canExpand = false { didSet { if canExpand != oldValue { needsDisplay = true } } }
    /// The checkbox: ticked = running, empty = paused; nil = the row has none.
    var running: Bool? { didSet { if running != oldValue { needsDisplay = true } } }
    /// A Play icon at the right edge.
    var playable = false { didSet { if playable != oldValue { needsDisplay = true } } }
    var onCheck: (@MainActor () -> Void)?
    var onPlay: (@MainActor () -> Void)?
    /// A right-click: the row's commands.
    var onContext: (@MainActor () -> Void)?
    var expanded = false { didSet { if expanded != oldValue { needsDisplay = true } } }
    var onToggle: @MainActor () -> Void = {}
    /// A click outside the chevron (Recent: Show in Finder). The menu closes first.
    var onActivate: (@MainActor () -> Void)?
    var hovered = false { didSet { if hovered != oldValue { needsDisplay = true } } }
    private static let edge: CGFloat = 5
    static let height: CGFloat = 40

    init() {
        super.init(frame: NSRect(x: 0, y: 0, width: 340, height: Self.height))
        autoresizingMask = .width
        setAccessibilityElement(true)
        setAccessibilityRole(.menuItem)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func sync(from item: NSMenuItem) {
        let t = item.attributedTitle?.string ?? item.title
        let dim = item.attributedTitle != nil && item.state != .on && item.attributedTitle?.attribute(.foregroundColor, at: 0, effectiveRange: nil) as? NSColor == .secondaryLabelColor
        let s = item.subtitle ?? ""
        let changed = (t, s, dim) != (title, subtitle, dimmed)
        title = t; subtitle = s; dimmed = dim; image = item.image
        if changed { setAccessibilityLabel(s.isEmpty ? t : "\(t), \(s)") }
        needsDisplay = true
    }

    private var chevronZone: NSRect { NSRect(x: Self.edge, y: 0, width: 24, height: bounds.height) }
    private static let checkX: CGFloat = edge + 28
    private var checkZone: NSRect { NSRect(x: Self.checkX - 6, y: 0, width: 26, height: bounds.height) }
    private var playZone: NSRect { NSRect(x: bounds.maxX - Self.edge - 32, y: 0, width: 32, height: bounds.height) }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        for a in trackingAreas { removeTrackingArea(a) }
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .mouseMoved, .activeAlways, .inVisibleRect], owner: self))
        // The areas were rebuilt (a scroll, a resize): the pointer may be on the other side of the edge now.
        hovered = menuPointerInside
    }

    override func mouseEntered(with event: NSEvent) { hovered = menuPointerInside }
    override func mouseMoved(with event: NSEvent) { hovered = menuPointerInside }
    override func mouseExited(with event: NSEvent) { hovered = false }
    override func viewDidMoveToWindow() { hovered = false }
    override func viewWillMove(toWindow newWindow: NSWindow?) { if newWindow == nil { hovered = false } }

    override func mouseUp(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        guard bounds.contains(p) else { return }
        if canExpand, chevronZone.contains(p) { onToggle(); return }
        if running != nil, checkZone.contains(p) { onCheck?(); return }
        if playable, playZone.contains(p) { hovered = false; closeThen { self.onPlay?() }; return }
        guard let run = onActivate else { return }
        hovered = false
        closeThen(run)
    }

    /// The menu closes first, then the action runs (as a plain item's action would).
    private func closeThen(_ run: @escaping @MainActor () -> Void) {
        enclosingMenuItem?.menu?.cancelTracking()
        DispatchQueue.main.async { MainActor.assumeIsolated { run() } }
    }

    override func rightMouseUp(with event: NSEvent) { onContext?() }

    override func accessibilityPerformPress() -> Bool {
        if canExpand { onToggle() } else if let onActivate { closeThen(onActivate) } else { onCheck?() }
        return true
    }

    /// QA: a click on the chevron.
    func qaToggle() { onToggle() }

    override func draw(_ dirtyRect: NSRect) {
        if hovered {
            NSColor.selectedContentBackgroundColor.setFill()
            NSBezierPath(roundedRect: bounds.insetBy(dx: Self.edge, dy: 1), xRadius: 5, yRadius: 5).fill()
        }
        let ink: NSColor = hovered ? .selectedMenuItemTextColor : (dimmed ? .secondaryLabelColor : .labelColor)
        let faint: NSColor = hovered ? .selectedMenuItemTextColor : .secondaryLabelColor
        let cfg = NSImage.SymbolConfiguration(pointSize: 10, weight: .semibold).applying(.init(paletteColors: [faint]))
        if canExpand, let img = NSImage(systemSymbolName: expanded ? "chevron.down" : "chevron.right", accessibilityDescription: nil)?.withSymbolConfiguration(cfg) {
            img.draw(at: NSPoint(x: Self.edge + 7, y: (bounds.height - img.size.height) / 2), from: .zero, operation: .sourceOver, fraction: 1)
        }
        var x = Self.edge + 28
        if let running {
            // The same checkbox as the file rows: ticked while it runs, empty while it is paused.
            MenuCheckbox.draw(at: NSPoint(x: Self.checkX, y: ((bounds.height - MenuCheckbox.size) / 2).rounded()), state: running ? .on : .off, hovered: hovered)
            x += 19
        }
        if let image {
            let r = NSRect(x: x, y: (bounds.height - image.size.height) / 2, width: image.size.width, height: image.size.height)
            if image.isTemplate {
                image.draw(in: r, from: .zero, operation: .sourceOver, fraction: 1)
            } else {
                image.draw(in: r)
            }
            x += image.size.width + 8
        }
        var right = bounds.maxX - Self.edge - 10
        if playable {
            let c = NSImage.SymbolConfiguration(pointSize: 10.5, weight: .semibold).applying(.init(paletteColors: [faint]))
            if let img = NSImage(systemSymbolName: QAFlags.playGlyph, accessibilityDescription: nil)?.withSymbolConfiguration(c) {
                right -= img.size.width
                img.draw(at: NSPoint(x: right, y: (bounds.height - img.size.height) / 2), from: .zero, operation: .sourceOver, fraction: 1)
                right -= 6
            }
        }
        let style = NSMutableParagraphStyle()
        style.lineBreakMode = .byTruncatingTail
        let tAttrs: [NSAttributedString.Key: Any] = [.font: NSFont.menuFont(ofSize: 0), .foregroundColor: ink, .paragraphStyle: style]
        let sAttrs: [NSAttributedString.Key: Any] = [.font: NSFont.menuFont(ofSize: NSFont.smallSystemFontSize), .foregroundColor: faint, .paragraphStyle: style]
        let th = (title as NSString).size(withAttributes: tAttrs).height
        let sh = (subtitle as NSString).size(withAttributes: sAttrs).height
        let total = th + (subtitle.isEmpty ? 0 : sh + 1)
        var y = (bounds.height - total) / 2 + total - th
        (title as NSString).draw(in: NSRect(x: x, y: y, width: max(10, right - x), height: th), withAttributes: tAttrs)
        if !subtitle.isEmpty {
            y -= sh + 1
            (subtitle as NSString).draw(in: NSRect(x: x, y: y, width: max(10, right - x), height: sh), withAttributes: sAttrs)
        }
    }
}

private final class FlippedView: NSView { override var isFlipped: Bool { true } }

/// The expanded list under a row: the scrolling checklist and the footer (no Select All / None here:
/// the menu stays a quick list; the window's file list and the chooser have them).
@MainActor final class MenuFilesPanel: NSView {
    private let header = NSView()
    private let scroll = NSScrollView()
    private let doc = FlippedView()
    private let footer = NSTextField(labelWithString: "")
    private let headerHeight: CGFloat = 4
    private let footerHeight: CGFloat = 22
    /// The window's file rows are 21 pt; the menu's are as compact.
    private static let rowHeight: CGFloat = 20
    /// Visible height of the list: about seven rows, so an open list never takes over the menu.
    static let maxList: CGFloat = 144
    private(set) var open: Set<UInt32>
    private(set) var rows: [MenuCheckRow] = []
    private let state: () -> FileTreeModel?
    private let toggleNode: (UInt32) -> Void
    private let setAll: (Bool) -> Void
    private let more: () -> Void
    private let play: (UInt32) -> Void
    /// The rows moved under the pointer (a scroll, a rebuild): the controller looks at the hover again.
    var onMoved: (() -> Void)?

    init(tree: FileTreeModel, width: CGFloat, state: @escaping () -> FileTreeModel?, toggleNode: @escaping (UInt32) -> Void,
         setAll: @escaping (Bool) -> Void, more: @escaping () -> Void, play: @escaping (UInt32) -> Void) {
        open = Set(tree.menuInitiallyOpen(limit: 14))
        self.state = state; self.toggleNode = toggleNode; self.setAll = setAll; self.more = more; self.play = play
        let n = min(Int(tree.nodeCount()), Int(fileMenuCap()) + 1)
        let list = min(Self.maxList, CGFloat(n) * Self.rowHeight + 4)
        super.init(frame: NSRect(x: 0, y: 0, width: width, height: headerHeight + list + footerHeight))
        autoresizingMask = .width
        header.frame = NSRect(x: 0, y: bounds.height - headerHeight, width: width, height: headerHeight)
        header.autoresizingMask = [.width, .minYMargin]
        scroll.frame = NSRect(x: 0, y: footerHeight, width: width, height: list)
        scroll.autoresizingMask = [.width, .height]
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.scrollerStyle = .overlay
        scroll.borderType = .noBorder
        scroll.documentView = doc
        footer.font = .menuFont(ofSize: NSFont.smallSystemFontSize)
        footer.textColor = .secondaryLabelColor
        footer.frame = NSRect(x: 16, y: 3, width: width - 32, height: 16)
        footer.autoresizingMask = .width
        footer.lineBreakMode = .byTruncatingTail
        // NSMenu highlights the whole item under the pointer, and this panel is one item: behind its rows that
        // drew one grey block, as if every file were hovered. The menu's own material, opaque to that highlight,
        // keeps the only highlight the row's own.
        let backdrop = NSVisualEffectView(frame: bounds)
        backdrop.material = .menu
        backdrop.blendingMode = .behindWindow
        backdrop.state = .followsWindowActiveState
        backdrop.autoresizingMask = [.width, .height]
        addSubview(backdrop)
        addSubview(scroll); addSubview(header); addSubview(footer)
        scroll.contentView.postsBoundsChangedNotifications = true
        NotificationCenter.default.addObserver(forName: NSView.boundsDidChangeNotification, object: scroll.contentView, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.onMoved?() }
        }
        rebuild()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func toggleFolder(_ node: UInt32) {
        if open.contains(node) { open.remove(node) } else { open.insert(node) }
        rebuild()
    }

    /// All rows again (a folder opened or closed, or the list changed shape).
    func rebuild() {
        guard let tree = state() else { return }
        let list = tree.menuRows(open: Array(open), cap: fileMenuCap())
        // Replacing the rows must not send the list back to its top: keep where the clip view was.
        let keptY = scroll.contentView.bounds.origin.y
        doc.subviews.forEach { $0.removeFromSuperview() }
        rows = []
        let w = scroll.contentSize.width
        // The document has its final width before rows go in: its autoresizing would stretch them otherwise.
        doc.frame.size.width = w
        var y: CGFloat = 2
        let reservesPlay = list.rows.contains(where: \.isVideo)
        for e in list.rows {
            let row = MenuCheckRow(content: Self.content(e, reservesPlay: reservesPlay), width: w) { [toggleNode] in toggleNode(e.id) }
            row.onDisclosure = { [weak self] in self?.toggleFolder(e.id) }
            row.onPlay = { [play] in play(e.id) }
            row.frame = NSRect(x: 0, y: y, width: w, height: Self.rowHeight)
            row.autoresizingMask = .width
            row.identifier = NSUserInterfaceItemIdentifier(String(e.id))
            doc.addSubview(row)
            rows.append(row)
            y += Self.rowHeight
        }
        if list.hidden > 0 {
            let row = MenuCheckRow(content: .init(title: "Show All in \(AppInfo.name)… (\(list.hidden) more)"), width: w) { [more] in more() }
            row.frame = NSRect(x: 0, y: y, width: w, height: Self.rowHeight)
            doc.addSubview(row)
            y += Self.rowHeight
        }
        doc.setFrameSize(NSSize(width: w, height: y + 2))
        doc.autoresizingMask = .width
        let maxY = max(0, doc.frame.height - scroll.contentView.bounds.height)
        scroll.contentView.scroll(to: NSPoint(x: 0, y: min(keptY, maxY)))
        scroll.reflectScrolledClipView(scroll.contentView)
        footer.stringValue = tree.totals().menuFooter
        DispatchQueue.main.async { [weak self] in self?.onMoved?() }
    }

    private static func content(_ e: FileRow, reservesPlay: Bool) -> MenuCheckRow.Content {
        .init(title: e.name, trailing: e.menuSize, check: e.check, disclosure: e.isFolder ? e.isOpen : nil, indent: Int(e.depth),
              playable: e.isVideo, reservesPlay: reservesPlay, iconExt: e.isFolder ? "" : e.ext)
    }

    /// A tick changed (an edit, the engine's answer): checks and footer follow the tree.
    func refreshChecks() {
        guard let tree = state() else { return }
        for r in rows {
            guard let id = r.identifier.flatMap({ UInt32($0.rawValue) }), let f = tree.row(id: id) else { continue }
            r.content.check = f.check
        }
        footer.stringValue = tree.totals().menuFooter
    }

    /// QA: the row of a file or folder by (part of) its name, or the Select All / Select None command.
    func qaRow(_ name: String) -> MenuCheckRow? {
        if name == "all" || name == "none" {
            let want = name == "all" ? "Select All" : "Select None"
            return header.subviews.compactMap { $0 as? MenuCheckRow }.first { $0.content.title == want }
        }
        return rows.first { $0.content.title.localizedCaseInsensitiveContains(name) }
    }
    var qaFooter: String { footer.stringValue }
    /// QA: frames of the panel, its window, the scroll view, the document and the first rows.
    var qaFrames: String {
        func f(_ r: NSRect) -> String { "\(Int(r.minX)),\(Int(r.minY)) \(Int(r.width))x\(Int(r.height))" }
        let rs = rows.prefix(3).map { "\($0.content.title.prefix(6)) \(f($0.frame)) trailing '\($0.content.trailing)'" }.joined(separator: "; ")
        return "window \(window.map { f($0.frame) } ?? "-") panel \(f(frame)) scroll \(f(scroll.frame)) content \(Int(scroll.contentSize.width)) doc \(f(doc.frame)) clip \(f(scroll.contentView.frame)) rows [\(rs)]"
    }
}

// MARK: Controller

/// Owns the inline lists of the menu bar menu. One instance; at most one list is open at a time. It has a
/// private `FilesStore` (never the window's), so the window's own lists are not touched, and the edits go
/// through the window's `FileTreeState` (check at once, one engine call after a quiet moment).
@MainActor final class MenuFiles {
    private let commands: Commands
    private let store = FilesStore()
    private var flushTimer: Timer?
    private var cleanup: Task<Void, Never>?
    /// The torrent whose list is open.
    private(set) var expanded: String?
    private var panelItem: NSMenuItem?
    private var panel: MenuFilesPanel?
    private var rowViews: [String: WeakRow] = [:]
    private struct WeakRow { weak var view: MenuTorrentRow? }
    /// The list's rows moved or changed shape: the controller looks at the hover again.
    var onMoved: (() -> Void)?
    /// The open list's rows (hover).
    var hoverRows: [MenuCheckRow] { panel?.rows ?? [] }

    init(core: Core, commands: Commands) {
        self.commands = commands
        store.core = core
        store.onTicked = { [weak commands] id in _ = commands?.arrivals.userReselected(id) }
        store.onEdited = { [weak self] in self?.panel?.refreshChecks() }
    }

    /// A list is open: the menu is not rebuilt under it (the 1 s refresh only updates texts).
    var isOpen: Bool { expanded != nil }

    /// Gives the torrent's menu item its row view (ring, texts, and as the row needs them the chevron, the
    /// check and the Play icon). A Recent row without a file list keeps the plain item.
    func decorate(_ item: NSMenuItem, row r: TransferRow, revealOnClick: Bool, interactive: Bool = false,
                  onActivate: (@MainActor () -> Void)? = nil, running: Bool? = nil, onCheck: (@MainActor () -> Void)? = nil) {
        let expands = r.offersMenuFiles
        guard expands || interactive else { return }
        let view = MenuTorrentRow()
        view.canExpand = expands
        if revealOnClick { view.onActivate = { [commands] in commands.reveal([r]) } }
        if let onActivate { view.onActivate = onActivate }
        view.running = running
        view.onCheck = onCheck
        view.playable = r.showsPlayButton
        view.onPlay = { [commands] in commands.playButtonPressed(r) }
        view.onToggle = { [weak self, weak item] in if let item { self?.toggle(r, item: item) } }
        item.view = view
        view.sync(from: item)
        rowViews[r.id] = WeakRow(view: view)
    }

    // MARK: Opening and closing

    func toggle(_ row: TransferRow, item: NSMenuItem) {
        let (id, sharing) = (row.id, row.isSharing)
        if expanded == id { collapse(); return }
        collapse()
        guard let menu = item.menu else { return }
        let st = state(for: id)
        guard let tree = st.tree else { return }
        // An open menu does not grow wider: the list takes the menu's width as it is now, so the size
        // column is never drawn past its edge.
        let width = min(max(item.view?.frame.width ?? 340, 340), menu.size.width)
        let panel = MenuFilesPanel(tree: tree, width: width,
                                   state: { [weak self] in self?.store.states[id]?.tree },
                                   toggleNode: { [weak self] node in self?.store.states[id]?.toggle([node]) },
                                   setAll: { [weak self] on in
                                       guard let st = self?.store.states[id], let t = st.tree else { return }
                                       st.setWanted([t.menuStart()], on)
                                   },
                                   more: { [weak self] in
                                       let section: LibraryModel.Section = sharing ? .sharing : .downloads
                                       self?.commands.showMain(section, nil)  // a tab switch closes every list: open this one after it
                                       FilesStore.shared.expand(id)
                                       menu.cancelTracking()
                                   },
                                   play: { [weak self] node in
                                       guard let self, let file = self.store.states[id]?.tree?.torrentFile(id: node) else { return }
                                       menu.cancelTracking()
                                       let commands = self.commands
                                       DispatchQueue.main.async { MainActor.assumeIsolated { commands.play(row, file: file) } }
                                   })
        panel.onMoved = { [weak self] in self?.onMoved?() }
        let pi = NSMenuItem()
        pi.view = panel
        menu.insertItem(pi, at: menu.index(of: item) + 1)
        self.panel = panel
        panelItem = pi
        expanded = id
        (item.view as? MenuTorrentRow)?.expanded = true
        cleanup?.cancel()
        DispatchQueue.main.async { [weak self] in self?.onMoved?() }
    }

    func collapse() {
        if let id = expanded { rowViews[id]?.view?.expanded = false }
        if let pi = panelItem, let m = pi.menu { m.removeItem(pi) }
        panel = nil; panelItem = nil; expanded = nil
        scheduleCleanup()
        DispatchQueue.main.async { [weak self] in self?.onMoved?() }
    }

    /// The whole menu closed: the list goes, its data a moment later.
    func menuClosed() {
        panel = nil; panelItem = nil; expanded = nil
        rowViews = [:]
        scheduleCleanup()
    }

    /// The list of a torrent, fetched now. A list with edits still on their way to the engine is kept.
    private func state(for torrent: String) -> FileTreeState {
        if let st = store.states[torrent] {
            if !st.hasPending { st.loadNow() }
            return st
        }
        let st = FileTreeState(id: torrent, store: store)
        st.onChange = { [weak self, weak st] in
            self?.panel?.refreshChecks()
            self?.scheduleFlush(st)
        }
        store.adopt(st)
        st.loadNow()
        return st
    }

    /// The window's debounce (250 ms of quiet, then one engine call) on a timer that also runs while the menu tracks.
    private func scheduleFlush(_ st: FileTreeState?) {
        guard let st, st.hasPending else { return }
        flushTimer?.invalidate()
        let t = Timer(timeInterval: 0.25, repeats: false) { [weak st] _ in MainActor.assumeIsolated { st?.flushNow() } }
        RunLoop.main.add(t, forMode: .common)
        flushTimer = t
    }

    /// Frees the lists a moment after they closed, but keeps one with unsent edits until they are sent.
    private func scheduleCleanup() {
        cleanup?.cancel()
        cleanup = Task { [weak self] in
            while let self, !Task.isCancelled {
                try? await Task.sleep(for: .milliseconds(400))
                if Task.isCancelled || expanded != nil { return }
                for (id, st) in store.states where !st.hasPending { store.drop(id) }
                if store.states.isEmpty { return }
            }
        }
    }

    // MARK: QA

    /// QA: a click on the chevron of the row whose title contains `key`.
    func qaToggle(_ key: String, items: [NSMenuItem]) -> Bool {
        guard let v = items.first(where: { ($0.view is MenuTorrentRow) && ($0.attributedTitle?.string ?? $0.title).localizedCaseInsensitiveContains(key) })?.view as? MenuTorrentRow else { return false }
        v.qaToggle()
        return true
    }

    /// QA: clicks a row of the open list (`all`, `none`, or a name), or its fold chevron (`fold`).
    func qaPress(_ name: String, fold: Bool) -> Bool {
        guard let row = panel?.qaRow(name) else { return false }
        if fold { row.onDisclosure() } else { row.run() }
        return true
    }

    var qaFooter: String { panel?.qaFooter ?? "" }
    var qaFrames: String { panel?.qaFrames ?? "" }
    var qaRowCount: Int { panel?.rows.count ?? 0 }

    /// QA: "wanted/total" of the torrent as the engine has it.
    func qaEngineWanted(_ id: String, core: Core) -> String {
        guard let files = try? core.files(id: id) else { return "?" }
        return "\(files.filter { $0.priority != .skip }.count)/\(files.count)"
    }
}
