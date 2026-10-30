import AppKit
import SwiftUI

/// A text field that is edited in place: focused when it appears, Return commits, Escape cancels,
/// and losing the focus commits too (Finder-style).
struct InlineField: View {
    let editing: FavoriteEditing
    let target: FavoriteEditing.Target
    var prompt = ""
    var font: Font = .system(size: 13)
    @FocusState private var focused: Bool

    var body: some View {
        @Bindable var e = editing
        TextField(prompt, text: $e.text)
            .textFieldStyle(.plain)
            .font(font)
            .focused($focused)
            .onSubmit { editing.commit() }
            .onExitCommand { editing.cancel() }
            .onChange(of: focused) { _, now in
                if !now, editing.target == target { editing.commit() }
            }
            .onAppear { DispatchQueue.main.async { focused = true } }
            .padding(.horizontal, 4).padding(.vertical, 1)
            .background(RoundedRectangle(cornerRadius: 4).fill(Color.primary.opacity(0.08)))
            .overlay(RoundedRectangle(cornerRadius: 4).strokeBorder(Color.accentColor.opacity(0.7), lineWidth: 1))
    }
}

/// A tracker in the sidebar. A favorite has a small star; while it is being renamed the label is a field.
struct TrackerRow: View {
    let item: SidebarItem
    let editing: FavoriteEditing

    var body: some View {
        let id = item.trackerID
        HStack(spacing: 8) {
            Image(systemName: SidebarSymbols.symbol(item.icon))
                .frame(width: 20)
                .foregroundStyle(item.icon == .favoriteTracker ? Color.accentColor.opacity(0.85) : Color.secondary)
            if editing.target == .sidebarName(id) {
                InlineField(editing: editing, target: .sidebarName(id))
            } else {
                Text(item.title).lineLimit(1)
            }
            Spacer(minLength: 4)
            Text("\(item.count ?? 0)").foregroundStyle(.secondary).monospacedDigit()
        }
        .accessibilityElement(children: .combine)
    }
}

extension SidebarItem {
    /// The tracker of a tracker's item ("" for other places).
    var trackerID: String {
        if case .tracker(let id) = place { return id }
        return ""
    }
}

/// "Remove “X” from Favorites? 2 rules use it." One quiet line and two small buttons.
struct FavoriteRemoveStrip: View {
    let question: FavoriteEditing.Removal
    let confirm: () -> Void
    let cancel: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "star.slash").font(.system(size: 11)).foregroundStyle(.secondary)
            Text(question.message).font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(2)
            Spacer(minLength: 8)
            Button("Cancel", action: cancel).controlSize(.small).keyboardShortcut(.cancelAction)
                .help("Keep it as a favorite")
            Button(role: .destructive, action: confirm) { Text("Remove").foregroundStyle(.red) }
                .controlSize(.small)
                .help("The torrents stay; the tracker goes back to the automatic list")
        }
        .padding(.horizontal, 10).padding(.vertical, 6)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.05)))
        .transition(.move(edge: .top).combined(with: .opacity))
    }
}
