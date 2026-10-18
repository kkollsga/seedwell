import SwiftUI

/// Renders a `MenuEntry` tree as SwiftUI context-menu content.
struct MenuEntriesView: View {
    let entries: [MenuEntry]

    var body: some View {
        ForEach(entries.indices, id: \.self) { i in entry(entries[i]) }
    }

    @ViewBuilder private func entry(_ e: MenuEntry) -> some View {
        switch e {
        case .separator:
            Divider()
        case let .submenu(title, children):
            Menu(title) { MenuEntriesView(entries: children) }
        case let .item(title, checked, enabled, role, _, run):
            Button(role: role == .destructive ? .destructive : nil) { run() } label: {
                if checked { Label(title, systemImage: "checkmark") } else { Text(title) }
            }
            .disabled(!enabled)
        }
    }
}
