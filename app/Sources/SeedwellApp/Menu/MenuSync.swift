import AppKit

// The menu bar's Folder Sync submenu (SPEC 8.2 item 5, 7G.5): one row per folder (status image, name, grey line,
// Edit and Delete at the right edge on hover), a delete confirmation that slides out under its row while the menu
// stays open (the same mechanism as the inline file list), and Add Folder… last.

extension SyncBadge {
    /// The row's status image, 16 pt: the green tick, a progress ring, or a symbol.
    @MainActor func image(hovered: Bool) -> NSImage? {
        let ink: NSColor = hovered ? .selectedMenuItemTextColor : .secondaryLabelColor
        func symbol(_ name: String, _ color: NSColor) -> NSImage? {
            let cfg = NSImage.SymbolConfiguration(pointSize: 13, weight: .regular).applying(.init(paletteColors: [color]))
            return NSImage(systemSymbolName: name, accessibilityDescription: nil)?.withSymbolConfiguration(cfg)
        }
        switch self {
        case .upToDate: return Drawing.check()
        case .progress(let f): return Drawing.ring(f).tinted(hovered ? .selectedMenuItemTextColor : .labelColor)
        case .paused: return symbol("pause.circle.fill", ink)
        case .attention: return symbol("exclamationmark.triangle.fill", hovered ? .selectedMenuItemTextColor : .systemOrange)
        case .missing: return symbol("questionmark.folder.fill", ink)
        }
    }

    var accessibilityWord: String {
        switch self {
        case .upToDate: "up to date"
        case .progress: "syncing"
        case .paused: "paused"
        case .attention: "needs attention"
        case .missing: "folder missing"
        }
    }
}

extension NSImage {
    /// A template image drawn in one colour (a view draws templates black otherwise).
    func tinted(_ color: NSColor) -> NSImage {
        let out = NSImage(size: size, flipped: false) { rect in
            self.draw(in: rect)
            color.set()
            rect.fill(using: .sourceAtop)
            return true
        }
        out.isTemplate = false
        return out
    }
}

/// One folder of the submenu: status image, the name cut in the middle and the grey line; Edit (pencil) and Delete
/// (trash) at the right edge while the pointer is over the row (or the keyboard has it). A click elsewhere reveals
/// the folder in Finder. Both buttons are accessibility elements of their own ("Edit Photos", "Remove Photos").
@MainActor final class MenuSyncRow: NSView, HoverRow {
    private(set) var folder: SyncFolder
    var onReveal: @MainActor () -> Void = {}
    var onEdit: @MainActor () -> Void = {}
    var onDelete: @MainActor () -> Void = {}
    var hovered = false { didSet { if hovered != oldValue { needsDisplay = true } } }
    /// Its delete confirmation is open: the trash stays visible and lit.
    var confirming = false { didSet { if confirming != oldValue { needsDisplay = true } } }
    private static let edge: CGFloat = 5
    static let height: CGFloat = 40
    static let width: CGFloat = 330
    private static let button: CGFloat = 24
    private let editElement = NSAccessibilityElement()
    private let deleteElement = NSAccessibilityElement()

    init(folder: SyncFolder) {
        self.folder = folder
        super.init(frame: NSRect(x: 0, y: 0, width: Self.width, height: Self.height))
        autoresizingMask = .width
        setAccessibilityElement(true)
        setAccessibilityRole(.menuItem)
        for (e, _) in [(editElement, 0), (deleteElement, 1)] {
            e.setAccessibilityRole(.button)
            e.setAccessibilityParent(self)
        }
        setAccessibilityChildren([editElement, deleteElement])
        refreshAccessibility()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func update(_ f: SyncFolder) {
        guard f != folder else { return }
        folder = f
        refreshAccessibility()
        needsDisplay = true
    }

    private var title: String { Fmt.middleTruncated(folder.name, to: SyncText.titleLimit) }
    private var subtitle: String { SyncText.subtitle(folder) }

    private func refreshAccessibility() {
        setAccessibilityLabel("\(folder.name), \(subtitle)")
        setAccessibilityHelp("Show in Finder")
        editElement.setAccessibilityLabel("Edit \(folder.name)")
        deleteElement.setAccessibilityLabel("Remove \(folder.name)")
        setAccessibilityCustomActions([
            NSAccessibilityCustomAction(name: "Edit \(folder.name)") { [weak self] in self?.press(edit: true); return true },
            NSAccessibilityCustomAction(name: "Remove \(folder.name)") { [weak self] in self?.press(edit: false); return true },
        ])
    }

    private var deleteZone: NSRect { NSRect(x: bounds.maxX - Self.edge - 6 - Self.button, y: 0, width: Self.button, height: bounds.height) }
    private var editZone: NSRect { deleteZone.offsetBy(dx: -Self.button, dy: 0) }
    private var showsButtons: Bool { hovered || confirming }

    override func layout() {
        super.layout()
        // The accessibility frames of the two buttons, in screen coordinates.
        guard window != nil else { return }
        editElement.setAccessibilityFrame(NSAccessibility.screenRect(fromView: self, rect: editZone))
        deleteElement.setAccessibilityFrame(NSAccessibility.screenRect(fromView: self, rect: deleteZone))
    }

    override func accessibilityChildren() -> [Any]? {
        editElement.setAccessibilityFrame(NSAccessibility.screenRect(fromView: self, rect: editZone))
        deleteElement.setAccessibilityFrame(NSAccessibility.screenRect(fromView: self, rect: deleteZone))
        return [editElement, deleteElement]
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        for a in trackingAreas { removeTrackingArea(a) }
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .mouseMoved, .activeAlways, .inVisibleRect], owner: self))
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
        if deleteZone.contains(p) { press(edit: false) }
        else if editZone.contains(p) { press(edit: true) }
        else { hovered = false; MenuSync.closeThen(enclosingMenuItem?.menu, onReveal) }
    }

    /// The Edit or Delete button, from a click, the keyboard or VoiceOver.
    func press(edit: Bool) {
        if edit { hovered = false; MenuSync.closeThen(enclosingMenuItem?.menu, onEdit) } else { onDelete() }
    }

    override func accessibilityPerformPress() -> Bool { MenuSync.closeThen(enclosingMenuItem?.menu, onReveal); return true }

    override func draw(_ dirtyRect: NSRect) {
        if hovered {
            NSColor.selectedContentBackgroundColor.setFill()
            NSBezierPath(roundedRect: bounds.insetBy(dx: Self.edge, dy: 1), xRadius: 5, yRadius: 5).fill()
        }
        let ink: NSColor = hovered ? .selectedMenuItemTextColor : .labelColor
        let faint: NSColor = hovered ? .selectedMenuItemTextColor : .secondaryLabelColor
        var x = Self.edge + 10
        let badge = SyncText.badge(folder)
        if let img = badge.image(hovered: hovered) {
            let s = NSSize(width: 16, height: 16)
            img.draw(in: NSRect(x: x, y: ((bounds.height - s.height) / 2).rounded(), width: s.width, height: s.height))
        }
        x += 16 + 8
        var right = bounds.maxX - Self.edge - 10
        if showsButtons {
            for (zone, name) in [(editZone, "pencil"), (deleteZone, "trash")] {
                let lit = name == "trash" && confirming
                let color: NSColor = hovered ? .selectedMenuItemTextColor : (lit ? .controlAccentColor : .secondaryLabelColor)
                let cfg = NSImage.SymbolConfiguration(pointSize: 12, weight: .medium).applying(.init(paletteColors: [color]))
                if let img = NSImage(systemSymbolName: name, accessibilityDescription: nil)?.withSymbolConfiguration(cfg) {
                    img.draw(at: NSPoint(x: zone.midX - img.size.width / 2, y: (bounds.height - img.size.height) / 2), from: .zero, operation: .sourceOver, fraction: 1)
                }
            }
            right = editZone.minX - 4
        }
        let style = NSMutableParagraphStyle()
        style.lineBreakMode = .byTruncatingTail
        let tAttrs: [NSAttributedString.Key: Any] = [.font: NSFont.menuFont(ofSize: 0), .foregroundColor: ink, .paragraphStyle: style]
        let sAttrs: [NSAttributedString.Key: Any] = [.font: NSFont.menuFont(ofSize: NSFont.smallSystemFontSize), .foregroundColor: faint, .paragraphStyle: style]
        let th = (title as NSString).size(withAttributes: tAttrs).height
        let sh = (subtitle as NSString).size(withAttributes: sAttrs).height
        let total = th + sh + 1
        var y = (bounds.height - total) / 2 + total - th
        (title as NSString).draw(in: NSRect(x: x, y: y, width: max(10, right - x), height: th), withAttributes: tAttrs)
        y -= sh + 1
        (subtitle as NSString).draw(in: NSRect(x: x, y: y, width: max(10, right - x), height: sh), withAttributes: sAttrs)
    }
}

private final class FlippedPanel: NSView { override var isFlipped: Bool { true } }

/// The confirmation under a row: the question, "Also move the files to the Trash", then Cancel and Remove at the
/// bottom right. Real controls: they work in a menu item's view, and the menu stays open while they are used.
@MainActor final class SyncConfirmPanel: NSView {
    private let question = NSTextField(wrappingLabelWithString: "")
    let trash = NSButton(checkboxWithTitle: SyncRemoval.trashLabel, target: nil, action: nil)
    let cancel = NSButton(title: "Cancel", target: nil, action: nil)
    let remove = NSButton(title: "Remove", target: nil, action: nil)
    var onTrash: (Bool) -> Void = { _ in }
    var onCancel: () -> Void = {}
    var onRemove: () -> Void = {}
    private static let pad: CGFloat = 20
    static let height: CGFloat = 104

    init(name: String, trashOn: Bool, width: CGFloat) {
        super.init(frame: NSRect(x: 0, y: 0, width: width, height: Self.height))
        autoresizingMask = .width
        // NSMenu highlights the whole item under the pointer; the menu's own material keeps that from showing.
        let backdrop = NSVisualEffectView(frame: bounds)
        backdrop.material = .menu
        backdrop.blendingMode = .behindWindow
        backdrop.state = .followsWindowActiveState
        backdrop.autoresizingMask = [.width, .height]
        addSubview(backdrop)
        let content = FlippedPanel(frame: bounds)
        content.autoresizingMask = [.width, .height]
        addSubview(content)

        question.stringValue = SyncRemoval.question(name)
        question.font = .menuFont(ofSize: NSFont.smallSystemFontSize + 0.5)
        question.textColor = .secondaryLabelColor
        question.maximumNumberOfLines = 2
        question.lineBreakMode = .byWordWrapping
        trash.font = .menuFont(ofSize: NSFont.smallSystemFontSize + 0.5)
        trash.state = trashOn ? .on : .off
        trash.controlSize = .small
        for b in [cancel, remove] {
            b.bezelStyle = .push
            b.controlSize = .small
            b.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        }
        remove.hasDestructiveAction = true
        cancel.toolTip = "Keep syncing this folder"
        remove.toolTip = "Stop syncing this folder on this Mac"
        trash.target = self; trash.action = #selector(trashChanged)
        cancel.target = self; cancel.action = #selector(cancelPressed)
        remove.target = self; remove.action = #selector(removePressed)
        for v in [question, trash, cancel, remove] as [NSView] { content.addSubview(v) }
        layoutControls(width: width)
        setAccessibilityElement(false)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func resizeSubviews(withOldSize oldSize: NSSize) {
        super.resizeSubviews(withOldSize: oldSize)
        layoutControls(width: bounds.width)
    }

    private func layoutControls(width: CGFloat) {
        let w = width - 2 * Self.pad
        question.preferredMaxLayoutWidth = w
        question.frame = NSRect(x: Self.pad + 20, y: 4, width: w - 20, height: 32)
        trash.sizeToFit()
        trash.frame.origin = NSPoint(x: Self.pad + 18, y: 42)
        remove.sizeToFit(); cancel.sizeToFit()
        let bw = max(remove.frame.width, cancel.frame.width, 64)
        remove.frame = NSRect(x: width - Self.pad - bw, y: 70, width: bw, height: remove.frame.height)
        cancel.frame = NSRect(x: remove.frame.minX - 8 - bw, y: 70, width: bw, height: cancel.frame.height)
    }

    @objc private func trashChanged() { onTrash(trash.state == .on) }
    @objc private func cancelPressed() { onCancel() }
    @objc private func removePressed() { onRemove() }
}

/// Owns the Folder Sync submenu. Built when the menu opens, refreshed once a second while it is open: the rows'
/// texts change in place; a folder added or removed rebuilds the submenu's items (the open confirmation is kept).
@MainActor final class MenuSync: NSObject, NSMenuDelegate {
    let menu = NSMenu(title: "Folder Sync")
    private let actions: SyncActions
    private(set) var removal = SyncRemoval()
    private var rows: [String: MenuSyncRow] = [:]
    private var folders: [SyncFolder] = []
    private var structure = ""
    private var confirmItem: NSMenuItem?
    /// The folder the keyboard (or the pointer) has: ⌘E and ⌘⌫ act on it.
    private var highlighted: String?

    init(actions: SyncActions) {
        self.actions = actions
        super.init()
        menu.delegate = self
        menu.autoenablesItems = false
    }

    /// The rows (hover).
    var hoverRows: [MenuSyncRow] { Array(rows.values) }

    /// What decides the submenu's items: the folders and their names (texts change in place).
    nonisolated static func structureOf(_ s: SyncSnapshot) -> String {
        s.folders.map { "\($0.id):\($0.name)" }.joined(separator: ",")
    }

    /// The whole menu opened: every confirmation starts closed (SPEC 8.2 item 5).
    func menuOpened(_ s: SyncSnapshot) {
        removal.close()
        rebuild(s)
    }

    /// The once-a-second refresh.
    func update(_ s: SyncSnapshot) {
        removal.keep(only: Set(s.folders.map(\.id)))
        if Self.structureOf(s) != structure || (confirmItem == nil) != (removal.open == nil) { rebuild(s); return }
        folders = s.folders
        for f in s.folders { rows[f.id]?.update(f) }
    }

    private func rebuild(_ s: SyncSnapshot) {
        structure = Self.structureOf(s)
        folders = s.folders
        menu.removeAllItems()
        rows = [:]
        confirmItem = nil
        if s.folders.isEmpty {
            let note = NSMenuItem(title: "", action: nil, keyEquivalent: "")
            note.attributedTitle = NSAttributedString(string: "Keep folders the same on your devices",
                                                      attributes: [.foregroundColor: NSColor.secondaryLabelColor, .font: NSFont.menuFont(ofSize: 0)])
            note.isEnabled = false
            menu.addItem(note)
            if let p = s.problem {
                let why = NSMenuItem(title: "", action: nil, keyEquivalent: "")
                why.attributedTitle = NSAttributedString(string: p, attributes: [.foregroundColor: NSColor.secondaryLabelColor, .font: NSFont.menuFont(ofSize: NSFont.smallSystemFontSize)])
                why.isEnabled = false
                menu.addItem(why)
            }
        }
        for f in s.folders {
            let item = ActionItem(title: f.name) { [weak self] in self?.reveal(f.id) }
            item.representedObject = f.id
            let row = MenuSyncRow(folder: f)
            row.onReveal = { [weak self] in self?.reveal(f.id) }
            row.onEdit = { [weak self] in self?.actions.showEdit(f.id) }
            row.onDelete = { [weak self] in self?.toggleConfirm(f.id) }
            row.confirming = removal.isOpen(f.id)
            item.view = row
            rows[f.id] = row
            menu.addItem(item)
            if removal.isOpen(f.id) { menu.addItem(makeConfirm(f)) }
        }
        menu.addItem(.separator())
        let add = ActionItem(title: "Add Folder…") { [actions] in actions.showAdd(nil) }
        add.toolTip = "Sync a folder of this Mac, or one shared with you"
        menu.addItem(add)
        // The keyboard's way to the hover buttons: ⌘E edits and ⌘⌫ asks to remove the highlighted folder.
        for (title, key, edit) in [("Edit Folder…", "e", true), ("Remove Folder…", "\u{8}", false)] {
            let item = ActionItem(title: title, key: key) { [weak self] in self?.keyboard(edit: edit) }
            item.isHidden = true
            item.allowsKeyEquivalentWhenHidden = true
            menu.addItem(item)
        }
        refreshHover()
    }

    private func makeConfirm(_ f: SyncFolder) -> NSMenuItem {
        let width = max(MenuSyncRow.width, menu.size.width)
        let panel = SyncConfirmPanel(name: f.name, trashOn: removal.trash, width: width)
        panel.onTrash = { [weak self] on in self?.removal.trash = on }
        panel.onCancel = { [weak self] in self?.closeConfirm() }
        panel.onRemove = { [weak self] in
            guard let self else { return }
            let trash = self.removal.trash
            self.removal.close()
            Self.closeThen(self.menu) { [actions = self.actions] in actions.remove(f, trash: trash) }
        }
        let item = NSMenuItem()
        item.view = panel
        confirmItem = item
        return item
    }

    /// The trash button: the confirmation slides out under this row (another one closes), or closes again.
    func toggleConfirm(_ id: String) {
        removal.toggle(id)
        if let ci = confirmItem, ci.menu === menu { menu.removeItem(ci) }
        confirmItem = nil
        for (fid, row) in rows { row.confirming = removal.isOpen(fid) }
        guard let f = folders.first(where: { $0.id == id }), removal.isOpen(id),
              let rowItem = menu.items.first(where: { ($0.representedObject as? String) == id }) else { refreshHover(); return }
        menu.insertItem(makeConfirm(f), at: menu.index(of: rowItem) + 1)
        DispatchQueue.main.async { [weak self] in self?.refreshHover() }
    }

    private func closeConfirm() {
        guard let id = removal.open else { return }
        toggleConfirm(id)
    }

    private func reveal(_ id: String) {
        guard let f = folders.first(where: { $0.id == id }) else { return }
        actions.reveal(f)
    }

    private func keyboard(edit: Bool) {
        guard let id = highlighted ?? rows.first(where: { $0.value.menuPointerInside })?.key else { return }
        // A key equivalent closes the menu: Remove asks on the folder's page in the window instead.
        if edit { actions.showEdit(id) } else { actions.showFolder(id, true) }
    }

    // MARK: Hover

    func menu(_ menu: NSMenu, willHighlight item: NSMenuItem?) {
        guard menu === self.menu else { return }
        highlighted = item?.representedObject as? String
        refreshHover(highlighted: item)
    }

    func menuDidClose(_ menu: NSMenu) {
        guard menu === self.menu else { return }
        for r in rows.values { r.hovered = false }
    }

    /// At most one row lit: the highlighted one (the keyboard too), else the one under the pointer.
    private func refreshHover(highlighted: NSMenuItem?? = nil) {
        let item = highlighted ?? menu.highlightedItem
        var taken = false
        for (id, row) in rows {
            let on = !taken && ((item?.representedObject as? String) == id || (item == nil && row.menuPointerInside))
            if on { taken = true }
            row.hovered = on
        }
    }

    /// The menu (and its parents) close first, then `run` (as a plain item's action would).
    static func closeThen(_ menu: NSMenu?, _ run: @escaping @MainActor () -> Void) {
        var top = menu
        while let parent = top?.supermenu { top = parent }
        top?.cancelTracking()
        DispatchQueue.main.async { MainActor.assumeIsolated { run() } }
    }

    // MARK: QA

    /// QA: presses Edit or Delete on the row whose name contains `key`.
    func qaPress(_ key: String, edit: Bool) -> Bool {
        guard let row = rows.values.first(where: { $0.folder.name.localizedCaseInsensitiveContains(key) }) else { return false }
        row.press(edit: edit)
        return true
    }

    var qaState: String {
        "rows \(rows.count), confirm \(removal.open.flatMap { id in folders.first { $0.id == id }?.name } ?? "none"), trash \(removal.trash)"
    }
}
