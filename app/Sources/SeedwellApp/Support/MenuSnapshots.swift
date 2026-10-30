import AppKit

// Debug-only visual QA (`--render-snapshots <dir>`, see SnapshotRenderer): the menu bar menu, its Folder Sync
// submenu and a row's right-click menu, in dark and light, from the mock library. A real NSMenu cannot be drawn
// offscreen, so each picture stacks the menu's items in one panel, top to bottom, and draws that panel:
//
//  - REAL: every item that has a view of its own (`NSMenuItem.view`) is drawn by that view, the one the app's own
//    code builds for the menu (StatusMenuController, MenuSync): the torrent rows (MenuTorrentRow: checkbox, ring,
//    texts, Play), the Sharing line (MenuCheckRow), the Open / Quit row (FooterRow), the Folder Sync rows
//    (MenuSyncRow) and the delete confirmation (SyncConfirmPanel, with its real NSButtons). Their own menu-material
//    backdrops are hidden here: in the menu they show the menu's material, which the panel already is.
//  - STAND-IN: what NSMenu draws itself. The items without a view (section headers, separators, "+ N more", Recent
//    rows without a file list, the Folder Sync line, Add Folder…, and every item of the right-click menu) are drawn
//    here from the real NSMenuItems the app builds (title, subtitle, image, check, key, submenu arrow, enabled) in
//    the menu font, laid out like NSMenu, but not by NSMenu. The panel's material, corners and outline are
//    stand-ins too.
//
// Files: status-menu-{dark,light}.png (real rows + stand-ins), sync-menu-real-{dark,light}.png (real rows + stand-ins),
// row-menu-{dark,light}.png (stand-in only). A caption under each picture says which is which, and `menus.txt` lists
// every row of every picture as "real <view class>" or "stand-in".
extension SnapshotRenderer {
    func renderMenus() async {
        var listing: [String] = []
        for dark in [true, false] {
            let suffix = dark ? "dark" : "light"

            // The menu bar menu, as opening it builds it. The second downloading row is hovered.
            let controller = StatusMenuController(core: core, commands: commands, settings: settings, inMenuBar: false)
            let status = MenuPanel(items: controller.qaBuildItems(), minWidth: 220)
            let hover = status.rows.compactMap { $0 as? MenuTorrentRow }.dropFirst().first
            await capture("status-menu-\(suffix)", dark: dark, panels: [(status, .zero)],
                          caption: "Menu bar menu. Real item views: torrent rows, the Sharing line, Open/Quit. Stand-ins (NSMenu draws these): section headers, separators, plain rows, the Folder Sync line, the panel.") {
                hover?.hovered = true
            }
            listing += ["status-menu-\(suffix).png (row 2 of Downloading hovered)"] + status.notes

            // The Folder Sync submenu, as MenuSync builds it: row 1 hovered, the confirmation open under row 2.
            let sync = MenuSync(actions: commands.sync)
            let snapshot = core.syncSnapshot()
            sync.menuOpened(snapshot)
            if snapshot.folders.indices.contains(2) { sync.toggleConfirm(snapshot.folders[2].id) }
            let sub = MenuPanel(items: sync.menu.items, minWidth: MenuSyncRow.width)
            let rows = sub.rows.compactMap { $0 as? MenuSyncRow }
            await capture("sync-menu-real-\(suffix)", dark: dark, panels: [(sub, .zero)],
                          caption: "Folder Sync submenu. Real item views: the folder rows (row 2 hovered) and the delete confirmation with its real buttons. Stand-ins: the separator, Add Folder…, the panel.") {
                if rows.indices.contains(1) { rows[1].hovered = true }
            }
            listing += ["sync-menu-real-\(suffix).png (row 2 hovered, confirmation open under row 3)"] + sub.notes

            // A downloading row's right-click menu (the menu bar's): the NSMenu the app pops, as stand-ins, with its
            // submenus beside it.
            if let row = core.snapshot().downloading.first {
                let menu = MenuBuilder.menu(commands.entries(for: [row], inMenuBar: true))
                let panels = MenuPanel.withSubmenus(menu.items, minWidth: 200)
                let main = panels[0].0
                listing += ["row-menu-\(suffix).png (\(row.name))"] + main.notes
                for ((item, _), (p, _)) in zip(main.submenuTops, panels.dropFirst()) {
                    listing += ["row-menu-\(suffix).png submenu \"\(item.title)\""] + p.notes
                }
                await capture("row-menu-\(suffix)", dark: dark, panels: panels,
                              caption: "Right-click menu of a downloading row (\(row.name)). STAND-IN ONLY: NSMenu draws this menu; here its real items are drawn in the menu font, with its submenus beside it, top to bottom: \(main.submenuTops.map { $0.0.title }.joined(separator: ", ")).") {}
            }
        }
        let text = listing.joined(separator: "\n") + "\n"
        try? text.write(to: URL(fileURLWithPath: dir).appendingPathComponent("menus.txt"), atomically: true, encoding: .utf8)
        print("wrote menus.txt")
    }

    /// The picture of the panels with the caption under them, written as `<name>.png`.
    private func capture(_ name: String, dark: Bool, panels: [(MenuPanel, NSPoint)], caption: String,
                         prepare: () -> Void) async {
        guard let rep = await MenuPicture.image(panels: panels, caption: caption, appearance: NSAppearance(named: dark ? .darkAqua : .aqua),
                                                prepare: prepare),
              let png = rep.representation(using: .png, properties: [:]) else { return }
        try? png.write(to: URL(fileURLWithPath: dir).appendingPathComponent("\(name).png"))
        print("wrote \(name).png (\(rep.pixelsWide)x\(rep.pixelsHigh))")
    }
}

/// A menu drawn offscreen (the visual QA run and QA's `shot … menu`): its panels on the window background.
@MainActor enum MenuPicture {
    /// Draws the panels (each at its offset, flipped: y down) on the window background with the caption under them
    /// (none: no caption), at 2x, in a window drawn as key, of the given appearance. `prepare` runs before each
    /// drawing, once the views are in the window (a row drops its hover when it joins one). The real item views go
    /// back to their menu items afterwards.
    static func image(panels: [(MenuPanel, NSPoint)], caption: String?, appearance: NSAppearance?,
                      prepare: () -> Void) async -> NSBitmapImageRep? {
        defer { for (p, _) in panels { p.giveBackViews() } }
        let margin: CGFloat = 16
        let content = panels.reduce(NSSize.zero) { s, p in
            NSSize(width: max(s.width, p.1.x + p.0.frame.width), height: max(s.height, p.1.y + p.0.frame.height))
        }
        let label = caption.map { NSTextField(wrappingLabelWithString: $0) }
        label?.font = .systemFont(ofSize: 10.5)
        label?.textColor = .secondaryLabelColor
        let width = caption == nil ? content.width : max(content.width, 320)
        label?.preferredMaxLayoutWidth = width
        let labelHeight = label.map { ceil($0.fittingSize.height) + 10 } ?? 0
        let root = Canvas(frame: NSRect(x: 0, y: 0, width: width + 2 * margin, height: content.height + labelHeight + 2 * margin))
        for (panel, at) in panels {
            panel.frame.origin = NSPoint(x: margin + at.x, y: margin + at.y)
            root.addSubview(panel)
        }
        if let label {
            label.frame = NSRect(x: margin, y: margin + content.height + 10, width: width, height: labelHeight - 10)
            root.addSubview(label)
        }

        let w = ShotWindow(contentRect: root.frame, styleMask: [.borderless], backing: .buffered, defer: false)
        w.isReleasedWhenClosed = false
        w.contentView = root
        w.appearance = appearance
        // Off every screen, drawn as key (the highlight in the accent colour): SnapshotWindow.
        w.setFrameOrigin(SnapshotWindow.offscreen)
        w.orderFront(nil)
        defer { w.orderOut(nil); w.contentView = nil; w.close() }
        return await SnapshotWindow.settledImage(of: root, limit: .seconds(0.8), before: prepare)
    }
}

/// A borderless snapshot window (SnapshotWindow: off every screen, drawn as key, no input).
private final class ShotWindow: SnapshotWindow {}

/// The picture's background: the window background colour, y down.
private final class Canvas: NSView {
    override var isFlipped: Bool { true }
    override func draw(_ dirtyRect: NSRect) {
        NSColor.windowBackgroundColor.setFill()
        bounds.fill()
    }
}

/// One menu: its visible items top to bottom, each the item's own view (real) or a `StandInMenuItem`, on a stand-in
/// of the menu's panel (its material, the 5 pt inset at the top and bottom, the rounded outline).
@MainActor final class MenuPanel: NSVisualEffectView {
    static let pad: CGFloat = 5
    /// The rows top to bottom: a real item's own view, or its stand-in.
    private(set) var rows: [NSView] = []
    /// One line per row: "real <class>: <title>" or "stand-in: <title>".
    private(set) var notes: [String] = []
    /// The items with a submenu, and the top of their row in the panel (y down).
    private(set) var submenuTops: [(NSMenuItem, CGFloat)] = []

    init(items: [NSMenuItem], minWidth: CGFloat) {
        let visible = items.filter { !$0.isHidden }
        let stateColumn = visible.contains { $0.view == nil && $0.state != .off }
        // As NSMenu: as wide as its widest item.
        let width = visible.reduce(minWidth) { w, item in
            max(w, item.view?.frame.width ?? StandInMenuItem.fittingWidth(item, stateColumn: stateColumn))
        }
        var views: [NSView] = []
        var rows: [NSView] = []
        var notes: [String] = []
        var tops: [(NSMenuItem, CGFloat)] = []
        var y = Self.pad
        for item in visible {
            let title = item.attributedTitle?.string ?? item.title
            let v: NSView
            if let own = item.view {
                rows.append(own)
                // In a holder of its own: the item keeps its view, and its frame is the item's alone.
                own.removeFromSuperview()
                let holder = NSView(frame: NSRect(x: 0, y: 0, width: width, height: own.frame.height))
                own.frame = holder.bounds
                holder.addSubview(own)
                Self.hideBackdrops(own)
                v = holder
                notes.append("  real \(type(of: own)): \(Self.describe(own, title))")
            } else {
                v = StandInMenuItem(item: item, width: width, stateColumn: stateColumn)
                rows.append(v)
                notes.append("  stand-in: \(item.isSeparatorItem ? "separator" : item.isSectionHeader ? "header \(title)" : title)\(item.submenu != nil ? " ▸" : "")")
            }
            if item.submenu != nil { tops.append((item, y)) }
            views.append(v)
            y += v.frame.height
        }
        let height = y + Self.pad
        super.init(frame: NSRect(x: 0, y: 0, width: width, height: height))
        material = .menu
        state = .active
        blendingMode = .withinWindow
        maskImage = Self.mask(radius: 10)
        var top = height - Self.pad
        for v in views {
            top -= v.frame.height
            v.frame.origin = NSPoint(x: 0, y: top)
            addSubview(v)
        }
        let outline = Outline(frame: bounds)
        addSubview(outline)
        self.rows = rows
        self.notes = notes
        submenuTops = tops
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    /// A menu and, beside it, each of its submenus at the height of its item (top to bottom, not overlapping).
    static func withSubmenus(_ items: [NSMenuItem], minWidth: CGFloat) -> [(MenuPanel, NSPoint)] {
        let main = MenuPanel(items: items, minWidth: minWidth)
        var panels: [(MenuPanel, NSPoint)] = [(main, .zero)]
        var bottom: CGFloat = 0
        for (item, top) in main.submenuTops {
            guard let submenu = item.submenu else { continue }
            let p = MenuPanel(items: submenu.items, minWidth: 160)
            let y = max(top - pad, bottom == 0 ? 0 : bottom + 8)
            panels.append((p, NSPoint(x: main.frame.width + 2, y: y)))
            bottom = y + p.frame.height
        }
        return panels
    }

    /// The real item views leave the panel: they belong to their menu items (a live menu keeps showing them).
    func giveBackViews() {
        for v in rows where !(v is StandInMenuItem) { v.removeFromSuperview() }
    }

    private static func describe(_ v: NSView, _ title: String) -> String {
        if let r = v as? MenuSyncRow { return "\(r.row.title) — \(r.row.subtitle)" }
        if let r = v as? MenuCheckRow { return "\(r.content.title) \(r.content.trailing)" }
        if v is FooterRow { return "Open / Quit" }
        if v is SyncConfirmPanel { return "delete confirmation" }
        return title
    }

    /// The menu-material backdrops inside the real views: in the menu they show the menu's own material.
    private static func hideBackdrops(_ v: NSView) {
        for s in v.subviews {
            if s is NSVisualEffectView { s.isHidden = true } else { hideBackdrops(s) }
        }
    }

    private static func mask(radius r: CGFloat) -> NSImage {
        let image = NSImage(size: NSSize(width: 2 * r + 1, height: 2 * r + 1), flipped: false) { rect in
            NSColor.black.setFill()
            NSBezierPath(roundedRect: rect, xRadius: r, yRadius: r).fill()
            return true
        }
        image.capInsets = NSEdgeInsets(top: r, left: r, bottom: r, right: r)
        image.resizingMode = .stretch
        return image
    }

    /// The panel's hairline outline (stand-in).
    private final class Outline: NSView {
        override func draw(_ dirtyRect: NSRect) {
            NSColor.separatorColor.setStroke()
            let p = NSBezierPath(roundedRect: bounds.insetBy(dx: 0.25, dy: 0.25), xRadius: 10, yRadius: 10)
            p.lineWidth = 0.5
            p.stroke()
        }
        override func hitTest(_ point: NSPoint) -> NSView? { nil }
    }
}

/// STAND-IN for an item NSMenu draws itself (QA: a menu cannot be drawn offscreen): a separator, a section header,
/// or a row with its check, image, title (its attributed title as given), subtitle, key equivalent and submenu arrow,
/// in the menu font, at the insets of the app's own rows. Not the system's drawing.
@MainActor private final class StandInMenuItem: NSView {
    private let item: NSMenuItem
    private let stateColumn: Bool
    /// Text starts where the app's own rows start theirs (5 pt inset + 10).
    private static let left: CGFloat = 15
    private static let checkColumn: CGFloat = 18
    private static let imageColumn: CGFloat = 16 + 8
    private static let right: CGFloat = 15
    private static var font: NSFont { .menuFont(ofSize: 0) }
    private static var small: NSFont { .menuFont(ofSize: NSFont.smallSystemFontSize) }
    private static var header: NSFont { .systemFont(ofSize: NSFont.smallSystemFontSize, weight: .bold) }

    init(item: NSMenuItem, width: CGFloat, stateColumn: Bool) {
        self.item = item
        self.stateColumn = stateColumn
        super.init(frame: NSRect(x: 0, y: 0, width: width, height: Self.height(item)))
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    static func height(_ item: NSMenuItem) -> CGFloat {
        if item.isSeparatorItem { return 11 }
        if item.isSectionHeader { return FooterRow.height - 2 }
        if let s = item.subtitle, !s.isEmpty { return FooterRow.height + ceil(small.boundingRectForFont.height) - 2 }
        return FooterRow.height
    }

    static func fittingWidth(_ item: NSMenuItem, stateColumn: Bool) -> CGFloat {
        if item.isSeparatorItem { return 0 }
        var w = left + (stateColumn ? checkColumn : 0) + (item.image != nil ? imageColumn : 0)
        let t = title(item).size().width
        let s = ((item.subtitle ?? "") as NSString).size(withAttributes: [.font: small]).width
        w += max(t, s)
        if item.submenu != nil { w += 30 }
        if let k = keyText(item) { w += (k as NSString).size(withAttributes: [.font: font]).width + 30 }
        return ceil(w + right)
    }

    private var ink: NSColor { item.isEnabled ? .labelColor : .tertiaryLabelColor }

    /// The title as NSMenu would show it: its attributed title (colour kept) or the plain one, in the menu font.
    private static func title(_ item: NSMenuItem) -> NSAttributedString {
        let ink: NSColor = item.isEnabled ? .labelColor : .tertiaryLabelColor
        if item.isSectionHeader {
            return NSAttributedString(string: item.title, attributes: [.font: header, .foregroundColor: NSColor.secondaryLabelColor])
        }
        guard let a = item.attributedTitle else { return NSAttributedString(string: item.title, attributes: [.font: font, .foregroundColor: ink]) }
        let m = NSMutableAttributedString(attributedString: a)
        let all = NSRange(location: 0, length: m.length)
        if m.length > 0 {
            if m.attribute(.font, at: 0, effectiveRange: nil) == nil { m.addAttribute(.font, value: font, range: all) }
            if m.attribute(.foregroundColor, at: 0, effectiveRange: nil) == nil { m.addAttribute(.foregroundColor, value: ink, range: all) }
            if !item.isEnabled { m.addAttribute(.foregroundColor, value: NSColor.tertiaryLabelColor, range: all) }
        }
        return m
    }

    private static func keyText(_ item: NSMenuItem) -> String? {
        guard !item.keyEquivalent.isEmpty else { return nil }
        let key = item.keyEquivalent == "\u{8}" ? "⌫" : item.keyEquivalent.uppercased()
        var mods = ""
        let m = item.keyEquivalentModifierMask
        if m.contains(.control) { mods += "⌃" }
        if m.contains(.option) { mods += "⌥" }
        if m.contains(.shift) { mods += "⇧" }
        if m.contains(.command) { mods += "⌘" }
        return mods + key
    }

    private func symbol(_ name: String, size: CGFloat, color: NSColor) -> NSImage? {
        let cfg = NSImage.SymbolConfiguration(pointSize: size, weight: .semibold).applying(.init(paletteColors: [color]))
        return NSImage(systemSymbolName: name, accessibilityDescription: nil)?.withSymbolConfiguration(cfg)
    }

    override func draw(_ dirtyRect: NSRect) {
        if item.isSeparatorItem {
            NSColor.separatorColor.setFill()
            NSRect(x: 14, y: (bounds.height / 2).rounded(.down), width: bounds.width - 28, height: 1).fill()
            return
        }
        let t = Self.title(item)
        if item.isSectionHeader {
            let h = t.size().height
            t.draw(at: NSPoint(x: Self.left, y: (bounds.height - h) / 2 - 1))
            return
        }
        var x = Self.left
        if stateColumn {
            if item.state != .off, let img = symbol(item.state == .on ? "checkmark" : "minus", size: 11, color: ink) {
                img.draw(at: NSPoint(x: x + 1, y: ((bounds.height - img.size.height) / 2).rounded()), from: .zero, operation: .sourceOver, fraction: 1)
            }
            x += Self.checkColumn
        }
        if let image = item.image {
            let s = NSSize(width: 16, height: 16)
            let drawn = image.isTemplate ? image.tinted(ink) : image
            drawn.draw(in: NSRect(x: x, y: ((bounds.height - s.height) / 2).rounded(), width: s.width, height: s.height))
            x += Self.imageColumn
        }
        var right = bounds.maxX - Self.right
        if item.submenu != nil, let img = symbol("chevron.right", size: 10, color: .secondaryLabelColor) {
            right -= img.size.width
            img.draw(at: NSPoint(x: right, y: ((bounds.height - img.size.height) / 2).rounded()), from: .zero, operation: .sourceOver, fraction: 1)
            right -= 10
        }
        if let k = Self.keyText(item) {
            let attrs: [NSAttributedString.Key: Any] = [.font: Self.font, .foregroundColor: item.isEnabled ? NSColor.secondaryLabelColor : NSColor.tertiaryLabelColor]
            let size = (k as NSString).size(withAttributes: attrs)
            right -= size.width
            (k as NSString).draw(at: NSPoint(x: right, y: (bounds.height - size.height) / 2), withAttributes: attrs)
            right -= 12
        }
        let subtitle = item.subtitle ?? ""
        let sAttrs: [NSAttributedString.Key: Any] = [.font: Self.small, .foregroundColor: item.isEnabled ? NSColor.secondaryLabelColor : NSColor.tertiaryLabelColor]
        let th = t.size().height
        let sh = subtitle.isEmpty ? 0 : (subtitle as NSString).size(withAttributes: sAttrs).height
        let total = th + (subtitle.isEmpty ? 0 : sh + 1)
        var y = (bounds.height - total) / 2 + total - th
        t.draw(with: NSRect(x: x, y: y, width: max(10, right - x), height: th), options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine])
        if !subtitle.isEmpty {
            y -= sh + 1
            (subtitle as NSString).draw(with: NSRect(x: x, y: y, width: max(10, right - x), height: sh),
                                        options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine], attributes: sAttrs)
        }
    }
}
