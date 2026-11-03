import SwiftUI

/// A torrent that waits for the user's choice of files (SPEC 7A.6, 8.6), inside the notifier, as the core lays it
/// out. Compact first: its header (title, "Category · size · folder") and Cancel and Start (everything but the
/// Extras). The chevron in front of the title opens this block's checklist in place (the popover grows by at most
/// `listHeight`, the list scrolls): tree, tri-state boxes, sizes, "Select all / none", the selected size, Cancel and
/// Start; the chevron folds it back. The title opens the torrent in the main window. A magnet link fetching its file
/// list shows a spinner and Cancel.
struct ChoiceBlock: View {
    let block: StartingBlock
    let store: ChoiceStore
    let actions: StartingActions

    /// The checklist never grows past this; longer lists scroll inside.
    static let listHeight: CGFloat = 220

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            BlockHeader(block: block, actions: actions) { actions.toggleExpanded(block.id) }
            if case let .choice(expanded) = block.state, let page = store.page(block.id) {
                if expanded {
                    ChoiceChecklist(id: block.id, page: page, store: store, buttons: block.actions,
                                    cancel: { actions.cancel(block.id, $0) }, start: { actions.confirm(block.id) })
                        .transition(.opacity)
                } else {
                    HeldButtons(actions: block.actions, cancel: { actions.cancel(block.id, $0) }, start: { actions.confirm(block.id) })
                }
            } else {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text(block.line).font(Theme.secondary).foregroundStyle(.secondary)
                    Spacer()
                    CancelControl(cancel: block.actions.cancel, keep: nil) { actions.cancel(block.id, $0) }
                }
                .accessibilityElement(children: .combine)
            }
        }
    }
}

/// The list of a chooser and the buttons under it: the notifier's open block, and (without buttons) the list under a
/// held torrent's row in the window. Every tick, fold and key goes to the shared chooser in the core.
struct ChoiceChecklist: View {
    let id: String
    let page: ChooserPage
    let store: ChoiceStore
    /// Cancel and Start under the list (the notifier's); nil in the main window, where they sit on the torrent's row.
    var buttons: HeldActions?
    var cancel: (_ keepFile: Bool) -> Void = { _ in }
    var start: () -> Void = {}
    /// The list never grows past this; the popover's is `ChoiceBlock.listHeight`.
    var maxListHeight = ChoiceBlock.listHeight
    /// The main window's list has "Show All" (SPEC 8.3); the popover's does not.
    var offersShowAll = false
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Button { edit { $0.toggleAll() } } label: {
                    HStack(spacing: 6) {
                        TriCheck(state: page.all.check, help: page.all.checkHelp, value: page.all.checkValue) { edit { $0.toggleAll() } }
                            .scaleEffect(0.85).frame(width: 16, height: 16)
                        Text(page.all.title).font(Theme.secondary)
                    }
                }
                .buttonStyle(.plain)
                .help(page.all.help)
                Spacer()
                if offersShowAll, fullHeight > maxListHeight || FilesStore.shared.showAll {
                    ShowAllButton(button: fileListShowAll(on: FilesStore.shared.showAll))
                        .buttonStyle(.plain).font(Theme.secondary).foregroundStyle(Color.accentColor)
                }
                Text(page.selectedLine)
                    .font(Theme.secondary).monospacedDigit()
                    .foregroundStyle(page.notEnoughSpace ? Color.orange : .secondary)
            }
            ScrollView {
                VStack(spacing: 0) {
                    ForEach(page.rows, id: \.id) { r in row(r) }
                }
            }
            .scrollDisabled(unlimited)
            .frame(height: listHeight)
            .background(RoundedRectangle(cornerRadius: 8).fill(.primary.opacity(0.05)))
            .clipShape(RoundedRectangle(cornerRadius: 8))
            .focusable()
            .focused($focused)
            .focusEffectDisabled()
            .onKeyPress(.space) { edit { $0.toggleCursor() }; return .handled }
            .onKeyPress(.downArrow) { edit { $0.moveCursor(delta: 1) }; return .handled }
            .onKeyPress(.upArrow) { edit { $0.moveCursor(delta: -1) }; return .handled }
            if page.notEnoughSpace {
                Text(page.spaceNote).font(Theme.secondary).foregroundStyle(Color.orange)
            }
            if let buttons {
                HeldButtons(actions: buttons, cancel: cancel, start: start)
            }
        }
    }

    /// A change to the shared chooser; everything that shows it draws again.
    private func edit(_ body: (Chooser) -> Void) {
        guard let c = store.loaded(id) else { return }
        store.edit { body(c) }
    }

    private var fullHeight: CGFloat { max(60, CGFloat(page.rows.count) * 28 + 4) }
    /// "Show All" is on: the list is as tall as its rows and does not scroll inside.
    private var unlimited: Bool { offersShowAll && FilesStore.shared.showAll }
    private var listHeight: CGFloat { unlimited ? fullHeight : min(maxListHeight, fullHeight) }

    private func row(_ item: ChooserRow) -> some View {
        HStack(spacing: 4) {
            if item.level == 0 {
                FoldChevron(open: item.open, size: Theme.Size.micro, width: 26, height: 28, help: item.foldHelp, label: item.foldLabel) {
                    edit { $0.toggleFolded(id: item.id) }
                }
                .opacity(item.foldable ? 1 : 0).allowsHitTesting(item.foldable)
            } else {
                Spacer().frame(width: 26)
            }
            TriCheck(state: item.check, help: item.checkHelp, value: item.checkValue) { edit { $0.toggle(id: item.id) } }
                .scaleEffect(0.85).frame(width: 22, height: 22)
            // The tap that ticks the row covers the title area only, never the chevron or the box.
            HStack(spacing: 6) {
                Text(item.title).font(Theme.secondary)
                    .foregroundStyle(item.dimmed ? .secondary : .primary)
                    .lineLimit(1).truncationMode(.middle)
                if let badge = item.badge {
                    Text(badge).font(Theme.micro).foregroundStyle(Color.green)
                        .padding(.horizontal, 6).padding(.vertical, 1)
                        .background(Capsule().fill(Color.green.opacity(0.16)))
                }
                Spacer(minLength: 4)
                Text(item.size).font(Theme.caption).foregroundStyle(.secondary).monospacedDigit()
            }
            .frame(maxHeight: .infinity)
            .contentShape(Rectangle())
            .onTapGesture { focused = true; edit { $0.toggle(id: item.id) } }
        }
        .padding(.leading, item.level == 0 ? 4 : 18).padding(.trailing, 8).frame(height: 28)
        .background(focused && item.cursor ? Color.accentColor.opacity(0.18) : .clear)
        .accessibilityElement(children: .contain)
        .accessibilityValue(item.checkValue)
    }
}
