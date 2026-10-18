import AppKit

// The menu bar's inline file list (SPEC §8.2): a multi-file torrent's row has a disclosure chevron; it opens
// a checklist right under the row, and ticking a file keeps the menu open. The list is fetched when it opens
// and dropped when the menu closes.

/// What the inline list shows, as plain data (no AppKit): the visible rows of a torrent (folders open or
/// closed), at most `cap` of them, and the footer line. Folders come first, then files, in the window's order.
struct MenuFileList: Equatable {
    struct Entry: Equatable {
        let node: Int
        let depth: Int
        let title: String
        /// "1.2 GB", or "42% · 1.2 GB" for a wanted file that is partly downloaded.
        let size: String
        let check: FileTree.Check
        let isFolder: Bool
        let isOpen: Bool
    }

    /// Rows drawn before "Show All in Seedwell…" (the list scrolls; this only bounds a huge archive).
    static let cap = 500

    let entries: [Entry]
    /// Visible rows left out by the cap.
    let hidden: Int
    /// "8 of 13 files · 16 MB of 17 MB".
    let footer: String

    /// The node whose children are the torrent's top level: a lone wrapper folder ("Show.S01/") is
    /// stepped into, so the list starts at the seasons or episodes.
    static func start(of tree: FileTree) -> Int {
        var n = 0
        while tree.nodes[n].children.count == 1, tree.nodes[tree.nodes[n].children[0]].isFolder { n = tree.nodes[n].children[0] }
        return n
    }

    /// Folders that start open: every one when the whole torrent is small, else none.
    static func initiallyOpen(_ tree: FileTree, limit: Int = 14) -> Set<Int> {
        tree.nodes.count - 1 <= limit ? Set(tree.folderIDs) : []
    }

    init(tree: FileTree, open: Set<Int>, cap: Int = MenuFileList.cap) {
        let start = Self.start(of: tree)
        var out: [Entry] = []
        var hidden = 0
        var stack: [(Int, Int)] = tree.nodes[start].children.reversed().map { ($0, 0) }
        while let (id, depth) = stack.popLast() {
            let n = tree.nodes[id]
            if out.count < cap {
                var size = Fmt.bytes(n.length)
                if !n.isFolder, n.priority != .skip, n.done > 0, n.done < n.length {
                    size = "\(Int(Double(n.done) / Double(n.length) * 100))% · \(size)"
                }
                out.append(Entry(node: id, depth: depth, title: n.name, size: size, check: tree.check(id), isFolder: n.isFolder, isOpen: open.contains(id)))
            } else {
                hidden += 1
            }
            if n.isFolder, open.contains(id) { for c in n.children.reversed() { stack.append((c, depth + 1)) } }
        }
        entries = out
        self.hidden = hidden
        footer = Self.footer(tree, start)
    }

    static func footer(_ tree: FileTree, _ node: Int) -> String {
        let n = tree.nodes[node]
        return "\(n.wantedCount) of \(n.fileCount) file\(n.fileCount == 1 ? "" : "s") · \(Fmt.bytes(n.wantedLength)) of \(Fmt.bytes(n.length))"
    }

    /// Whether a row gets the chevron: a multi-file torrent that is not held for the user's choice (that has the chooser).
    static func offers(_ r: TransferRow) -> Bool {
        r.kind == .torrent && r.fileCount > 1 && r.pending == nil && !r.awaitingChoice
    }
}

extension FileTree {
    /// Select All / Select None under these rows: skipped files become Normal (`on`), or wanted ones are skipped.
    func wantChanges(under ids: [Int], _ on: Bool) -> [Change] {
        fileNodes(under: ids).compactMap { i in
            guard let idx = nodes[i].file else { return nil }
            return (nodes[i].priority == .skip) == on ? (idx, on ? .normal : .skip) : nil
        }
    }
}

// MARK: Rows

/// A row of the checklist: [fold chevron] ✓ / − / empty, the name, the size on the right. A click on the
/// chevron folds a folder, a click elsewhere runs `run`; neither closes the menu.
@MainActor final class MenuCheckRow: NSView {
    struct Content: Equatable {
        var title: String
        var trailing = ""
        /// nil: a plain command ("Select All").
        var check: FileTree.Check?
        /// A folder: open or closed. nil for a file.
        var disclosure: Bool?
        var indent = 0
    }

    var content: Content { didSet { if content != oldValue { needsDisplay = true; refreshAccessibility() } } }
    var run: @MainActor () -> Void
    var onDisclosure: @MainActor () -> Void = {}
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

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        for a in trackingAreas { removeTrackingArea(a) }
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .mouseMoved, .activeAlways, .inVisibleRect], owner: self))
    }

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
        let p = convert(event.locationInWindow, from: nil)
        guard bounds.contains(p) else { return }
        if content.disclosure != nil, disclosureZone.contains(p) { onDisclosure() } else { run() }
    }

    override func accessibilityPerformPress() -> Bool { run(); return true }

    override func draw(_ dirtyRect: NSRect) {
        if hovered {
            NSColor.selectedContentBackgroundColor.setFill()
            NSBezierPath(roundedRect: bounds.insetBy(dx: Self.edge, dy: 1), xRadius: 5, yRadius: 5).fill()
        }
        let isFile = content.check != nil
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
            if let open = content.disclosure { _ = symbol(open ? "chevron.down" : "chevron.right", size: 8, color: faint, at: chevronX + 2) }
            x = chevronX + 18
            if let check = content.check, check != .off { _ = symbol(check == .on ? "checkmark" : "minus", size: 9.5, color: ink, at: x) }
            x += 17
        }

        // File rows sit in a scroll view whose overlay scroller and the menu's own inset eat into the
        // right edge: keep the size column clear of both.
        let right = bounds.maxX - Self.edge - (content.check != nil ? 24 : 9)
        var textRight = right
        if !content.trailing.isEmpty {
            let attrs: [NSAttributedString.Key: Any] = [.font: isFile ? Self.fileSize : Self.small, .foregroundColor: faint]
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
/// grey line, ▸ when it has a submenu) with a chevron at the left edge. The chevron (24 pt hit area)
/// opens the list; elsewhere a Recent row reveals the file and a download row leaves the click to its
/// submenu. It takes its texts from the NSMenuItem the controller keeps up to date (`sync`).
@MainActor final class MenuTorrentRow: NSView {
    private var title = ""
    private var subtitle = ""
    private var image: NSImage?
    private var dimmed = false
    private var hasSubmenu = false
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
        let changed = (t, s, dim, item.submenu != nil) != (title, subtitle, dimmed, hasSubmenu)
        title = t; subtitle = s; dimmed = dim; hasSubmenu = item.submenu != nil; image = item.image
        if changed { setAccessibilityLabel(s.isEmpty ? t : "\(t), \(s)") }
        needsDisplay = true
    }

    private var chevronZone: NSRect { NSRect(x: Self.edge, y: 0, width: 24, height: bounds.height) }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        for a in trackingAreas { removeTrackingArea(a) }
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .mouseMoved, .activeAlways, .inVisibleRect], owner: self))
    }

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
        let p = convert(event.locationInWindow, from: nil)
        guard bounds.contains(p) else { return }
        if chevronZone.contains(p) { onToggle(); return }
        guard let run = onActivate else { return }
        hovered = false
        enclosingMenuItem?.menu?.cancelTracking()
        DispatchQueue.main.async { MainActor.assumeIsolated { run() } }
    }

    override func accessibilityPerformPress() -> Bool { onToggle(); return true }

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
        if let img = NSImage(systemSymbolName: expanded ? "chevron.down" : "chevron.right", accessibilityDescription: nil)?.withSymbolConfiguration(cfg) {
            img.draw(at: NSPoint(x: Self.edge + 7, y: (bounds.height - img.size.height) / 2), from: .zero, operation: .sourceOver, fraction: 1)
        }
        var x = Self.edge + 28
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
        if hasSubmenu {
            let c = NSImage.SymbolConfiguration(pointSize: 10, weight: .semibold).applying(.init(paletteColors: [faint]))
            if let img = NSImage(systemSymbolName: "chevron.right", accessibilityDescription: nil)?.withSymbolConfiguration(c) {
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
    /// Visible height of the list.
    static let maxList: CGFloat = 200
    private(set) var open: Set<Int>
    private(set) var rows: [MenuCheckRow] = []
    private let state: () -> FileTree?
    private let toggleNode: (Int) -> Void
    private let setAll: (Bool) -> Void
    private let more: () -> Void

    init(tree: FileTree, width: CGFloat, state: @escaping () -> FileTree?, toggleNode: @escaping (Int) -> Void,
         setAll: @escaping (Bool) -> Void, more: @escaping () -> Void) {
        open = MenuFileList.initiallyOpen(tree)
        self.state = state; self.toggleNode = toggleNode; self.setAll = setAll; self.more = more
        let n = min(tree.nodes.count - 1, MenuFileList.cap + 1)
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
        addSubview(scroll); addSubview(header); addSubview(footer)
        rebuild()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func toggleFolder(_ node: Int) {
        if open.contains(node) { open.remove(node) } else { open.insert(node) }
        rebuild()
    }

    /// All rows again (a folder opened or closed, or the list changed shape).
    func rebuild() {
        guard let tree = state() else { return }
        let list = MenuFileList(tree: tree, open: open)
        doc.subviews.forEach { $0.removeFromSuperview() }
        rows = []
        let w = scroll.contentSize.width
        // The document has its final width before rows go in: its autoresizing would stretch them otherwise.
        doc.frame.size.width = w
        var y: CGFloat = 2
        for e in list.entries {
            let row = MenuCheckRow(content: Self.content(e), width: w) { [toggleNode] in toggleNode(e.node) }
            row.onDisclosure = { [weak self] in self?.toggleFolder(e.node) }
            row.frame = NSRect(x: 0, y: y, width: w, height: Self.rowHeight)
            row.autoresizingMask = .width
            row.identifier = NSUserInterfaceItemIdentifier(String(e.node))
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
        footer.stringValue = list.footer
    }

    private static func content(_ e: MenuFileList.Entry) -> MenuCheckRow.Content {
        .init(title: e.title, trailing: e.size, check: e.check, disclosure: e.isFolder ? e.isOpen : nil, indent: e.depth)
    }

    /// A tick changed (an edit, the engine's answer): checks and footer follow the tree.
    func refreshChecks() {
        guard let tree = state() else { return }
        for r in rows {
            guard let id = r.identifier.flatMap({ Int($0.rawValue) }), id < tree.nodes.count else { continue }
            r.content.check = tree.check(id)
        }
        footer.stringValue = MenuFileList.footer(tree, MenuFileList.start(of: tree))
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

    init(core: Core, commands: Commands) {
        self.commands = commands
        store.core = core
        store.onTicked = { [weak commands] id in _ = commands?.arrivals.userReselected(id) }
        store.onEdited = { [weak self] in self?.panel?.refreshChecks() }
    }

    /// A list is open: the menu is not rebuilt under it (the 1 s refresh only updates texts).
    var isOpen: Bool { expanded != nil }

    /// Gives the torrent's menu item its row view (chevron, ring, texts) when it has files to choose from.
    func decorate(_ item: NSMenuItem, row r: TransferRow, revealOnClick: Bool) {
        guard MenuFileList.offers(r) else { return }
        let view = MenuTorrentRow()
        if revealOnClick { view.onActivate = { [commands] in commands.reveal([r]) } }
        view.onToggle = { [weak self, weak item] in if let item { self?.toggle(r.id, item: item, sharing: r.isSharing) } }
        item.view = view
        view.sync(from: item)
        rowViews[r.id] = WeakRow(view: view)
    }

    // MARK: Opening and closing

    func toggle(_ id: String, item: NSMenuItem, sharing: Bool) {
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
                                       st.setWanted([MenuFileList.start(of: t)], on)
                                   },
                                   more: { [weak self] in
                                       let section: LibraryModel.Section = sharing ? .sharing : .downloads
                                       FilesStore.shared.expand(id)
                                       self?.commands.showMain(section, nil)
                                       menu.cancelTracking()
                                   })
        let pi = NSMenuItem()
        pi.view = panel
        menu.insertItem(pi, at: menu.index(of: item) + 1)
        self.panel = panel
        panelItem = pi
        expanded = id
        (item.view as? MenuTorrentRow)?.expanded = true
        cleanup?.cancel()
    }

    func collapse() {
        if let id = expanded { rowViews[id]?.view?.expanded = false }
        if let pi = panelItem, let m = pi.menu { m.removeItem(pi) }
        panel = nil; panelItem = nil; expanded = nil
        scheduleCleanup()
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
