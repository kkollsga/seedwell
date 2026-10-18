import SwiftUI

/// The pending "Remove…" (SPEC §8.3): no alert, a small strip slides down from the torrent's row
/// and asks there. One at a time; a new Remove… replaces it. It is dropped by Escape, Cancel, the user
/// choosing another section, or its rows leaving the screen; never by the window or the app losing and
/// regaining focus.
@MainActor @Observable final class RemovalPrompt {
    struct Pending: Equatable {
        let ids: [String]
        let title: String
        let plural: Bool
        let deleteFiles: Bool
    }

    private(set) var pending: Pending?

    func ask(_ rows: [TransferRow], deleteFiles: Bool) {
        let title = rows.count == 1 ? "“\(rows[0].displayName)”" : "\(rows.count) transfers"
        withAnimation(.snappy(duration: 0.22)) { pending = Pending(ids: rows.map(\.id), title: title, plural: rows.count > 1, deleteFiles: deleteFiles) }
    }

    /// The user picked another place in the sidebar: the question was about rows of this one.
    func userChangedSection() { cancel() }

    /// The rows on screen changed. The question goes when none of its rows is there any more (removed,
    /// or in another section); it stays while any is.
    func visibleRowsChanged(_ visible: Set<String>) {
        guard let p = pending, !p.ids.contains(where: visible.contains) else { return }
        cancel()
    }

    /// Whether a Return should answer it: only while at least one of its rows is on screen.
    func isAnswerable(visible: Set<String>) -> Bool {
        pending.map { $0.ids.contains(where: visible.contains) } ?? false
    }

    func cancel() {
        guard pending != nil else { return }
        withAnimation(.snappy(duration: 0.18)) { pending = nil }
    }
}

/// The question itself: one quiet line and two small buttons. Under the row for one torrent; in the
/// title bar for a multi-selection, which spans rows and may run off screen (user, 2026-10-14).
struct RemoveConfirmContent: View {
    let pending: RemovalPrompt.Pending
    let confirm: () -> Void
    let cancel: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: pending.deleteFiles ? "trash" : "minus.circle")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
            Text(message)
                .font(.system(size: 12))
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer(minLength: 8)
            Button("Cancel", action: cancel)
                .controlSize(.small)
                .keyboardShortcut(.cancelAction)
                .help("Keep \(pending.plural ? "them" : "it")")
            // Red text, not a tint: in the title bar the toolbar's button style drops tints.
            Button(role: .destructive, action: confirm) { Text(pending.deleteFiles ? "Delete" : "Remove").foregroundStyle(.red) }
                .controlSize(.small)
                .help(pending.deleteFiles ? "Remove from Seedwell and move the downloaded files to the Trash (Return)"
                                          : "Remove from Seedwell; the downloaded files stay where they are (Return)")
        }
    }

    private var message: String {
        pending.deleteFiles ? "Remove \(pending.title) and move \(pending.plural ? "their" : "its") files to the Trash?"
                            : "Remove \(pending.title)? Files are kept."
    }
}

/// One torrent: the strip that slides down under its row.
struct RemoveConfirmStrip: View {
    static let id = "remove-confirm-strip"
    let pending: RemovalPrompt.Pending
    let confirm: () -> Void
    let cancel: () -> Void

    var body: some View {
        RemoveConfirmContent(pending: pending, confirm: confirm, cancel: cancel)
            .padding(.leading, 52 + FileLayout.iconX)
            .padding(.trailing, 10)
            .padding(.vertical, 6)
            .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.05)))
            .padding(.bottom, 4)
            .transition(.move(edge: .top).combined(with: .opacity))
    }
}

/// Several torrents: the same question in the title bar, trailing edge.
struct RemoveConfirmToolbarItem: ToolbarContent {
    let commands: Commands

    var body: some ToolbarContent {
        if #available(macOS 26.0, *) {
            ToolbarItem(placement: .primaryAction) { content }.sharedBackgroundVisibility(.hidden)
        } else {
            ToolbarItem(placement: .primaryAction) { content }
        }
    }

    @ViewBuilder private var content: some View {
        if let p = commands.removal.pending, p.plural {
            RemoveConfirmContent(pending: p, confirm: commands.confirmedRemove, cancel: commands.removal.cancel)
                .fixedSize()
                .padding(.horizontal, 12)
                .padding(.vertical, 5)
                .background(Capsule().fill(Color.primary.opacity(0.07)))
                .transition(.move(edge: .top).combined(with: .opacity))
        }
    }
}
