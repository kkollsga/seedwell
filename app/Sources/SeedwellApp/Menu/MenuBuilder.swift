import AppKit

/// NSMenuItem that runs a closure; lets `MenuEntry` trees become real menus.
@MainActor final class ActionItem: NSMenuItem {
    private let run: @MainActor () -> Void

    init(title: String, key: String = "", run: @escaping @MainActor () -> Void) {
        self.run = run
        super.init(title: title, action: #selector(fire), keyEquivalent: key)
        target = self
    }

    required init(coder: NSCoder) { fatalError("not used") }

    @objc private func fire() { run() }
}

@MainActor enum MenuBuilder {
    static func menu(_ entries: [MenuEntry], title: String = "") -> NSMenu {
        let menu = NSMenu(title: title)
        menu.autoenablesItems = false
        for entry in entries { menu.addItem(item(entry)) }
        return menu
    }

    static func item(_ entry: MenuEntry) -> NSMenuItem {
        switch entry {
        case .separator:
            return .separator()
        case let .submenu(title, children):
            let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
            item.submenu = menu(children, title: title)
            return item
        case let .item(title, checked, enabled, role, key, run):
            // "⌘I" style hints are display only: a plain letter plus the command modifier.
            let item = ActionItem(title: title, run: run)
            if let key, key.hasPrefix("⌘") { item.keyEquivalent = String(key.dropFirst()).lowercased() }
            item.state = checked ? .on : .off
            item.isEnabled = enabled
            if role == .destructive {
                item.attributedTitle = NSAttributedString(string: title, attributes: [.foregroundColor: NSColor.systemRed])
            }
            return item
        }
    }
}
