import SwiftUI

// The pending "Remove…" (SPEC §8.3): no alert, a small strip slides down from the torrent's row and asks there. The
// core keeps it (`LibraryViewModel.removal`): one at a time, a new Remove… replaces it, and it goes with Escape, Cancel,
// another place, or its rows leaving the screen; never with the window or the app losing and regaining focus.

/// The question itself: one quiet line and two small buttons. Under the row for one torrent; in the
/// title bar for a multi-selection, which spans rows and may run off screen (user, 2026-10-14).
struct RemoveConfirmContent: View {
    let pending: RemovalQuestion
    let confirm: () -> Void
    let cancel: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: pending.deleteFiles ? "trash" : "minus.circle")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
            Text(pending.message)
                .font(.system(size: 12))
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer(minLength: 8)
            Button(pending.cancelTitle, action: cancel)
                .controlSize(.small)
                .keyboardShortcut(.cancelAction)
                .help(pending.cancelHelp)
            // Red text, not a tint: in the title bar the toolbar's button style drops tints.
            Button(role: .destructive, action: confirm) { Text(pending.confirmTitle).foregroundStyle(.red) }
                .controlSize(.small)
                .help(pending.confirmHelp)
        }
    }
}

/// One torrent: the strip that slides down under its row.
struct RemoveConfirmStrip: View {
    static let id = "remove-confirm-strip"
    let pending: RemovalQuestion
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
    let model: LibraryModel

    var body: some ToolbarContent {
        if #available(macOS 26.0, *) {
            ToolbarItem(placement: .primaryAction) { content }.sharedBackgroundVisibility(.hidden)
        } else {
            ToolbarItem(placement: .primaryAction) { content }
        }
    }

    @ViewBuilder private var content: some View {
        if let p = model.view.removal, p.question.plural {
            RemoveConfirmContent(pending: p.question, confirm: model.confirmRemoval, cancel: model.cancelRemoval)
                .fixedSize()
                .padding(.horizontal, 12)
                .padding(.vertical, 5)
                .background(Capsule().fill(Color.primary.opacity(0.07)))
                .transition(.move(edge: .top).combined(with: .opacity))
        }
    }
}
