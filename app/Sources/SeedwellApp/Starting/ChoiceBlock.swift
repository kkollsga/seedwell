import SwiftUI

/// A torrent that waits for the user's choice of files (SPEC 7A.6, 8.6), inside the notifier. Compact
/// first: its header (title, "Category · size · folder") and two buttons, Download (the smart default) and
/// Cancel. The chevron in front of the title opens this block's checklist in place (the popover grows by at most
/// `listHeight`, the list scrolls): tree, tri-state boxes, sizes, "Select all / none", the selected size, OK and
/// Cancel; the chevron folds it back. The title opens the torrent in the main window.
struct ChoiceBlock: View {
    let item: StartingItem
    /// The shared chooser model; nil while the magnet link has no file list yet.
    let model: ChooserModel?
    let expanded: Bool
    /// Owns Return (OK) and Escape (Cancel) when the popover is key.
    let active: Bool
    let actions: StartingActions
    let toggleExpanded: () -> Void

    /// The checklist never grows past this; longer lists scroll inside.
    static let listHeight: CGFloat = 220

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            BlockHeader(item: item,
                        chevron: item.awaiting
                            ? .init(open: expanded, help: expanded ? "Back to the short view" : "Show the files") { toggleExpanded() }
                            : nil,
                        actions: actions)
            if let model, item.awaiting {
                if expanded {
                    ChoiceChecklist(model: model, active: active, id: item.id, hasSource: item.hasSource, actions: actions)
                        .transition(.opacity)
                } else {
                    HeldButtons(hasSourceFile: item.hasSource, active: active, canStart: model.canConfirm,
                                startHelp: "Start downloading every file. Open the list to choose",
                                cancel: { actions.cancel(item.id, $0) }, start: { actions.confirm(item.id) })
                }
            } else {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Fetching file list…").font(.system(size: 12.5)).foregroundStyle(.secondary)
                    Spacer()
                    CancelControl(hasSourceFile: false, escape: active) { actions.cancel(item.id, $0) }
                }
                .accessibilityElement(children: .combine)
            }
        }
    }
}

/// The list and the buttons under it.
struct ChoiceChecklist: View {
    @Bindable var model: ChooserModel
    let active: Bool
    let id: String
    var hasSource = false
    let actions: StartingActions
    /// False in the main window, where Download and Cancel sit on the torrent's row.
    var showButtons = true
    /// The list never grows past this; the popover's is `ChoiceBlock.listHeight`.
    var maxListHeight = ChoiceBlock.listHeight
    @FocusState private var focused: Bool
    @State private var cursor: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Button { model.toggleAll() } label: {
                    HStack(spacing: 6) {
                        TriCheck(state: model.allState) { model.toggleAll() }.scaleEffect(0.85).frame(width: 16, height: 16)
                        Text(model.allState == .on ? "Select none" : "Select all").font(.system(size: 12.5))
                    }
                }
                .buttonStyle(.plain)
                .help(model.allState == .on ? "Untick every file" : "Tick every file")
                Spacer()
                Text("\(Fmt.bytes(model.selectedBytes)) of \(Fmt.bytes(model.totalBytes))")
                    .font(.system(size: 12)).monospacedDigit()
                    .foregroundStyle(model.notEnoughSpace ? Color.orange : .secondary)
            }
            ScrollView {
                VStack(spacing: 0) {
                    ForEach(model.visibleRows, id: \.item.id) { entry in
                        row(entry.item, level: entry.level)
                    }
                }
            }
            .frame(height: listHeight)
            .background(RoundedRectangle(cornerRadius: 8).fill(.primary.opacity(0.05)))
            .clipShape(RoundedRectangle(cornerRadius: 8))
            .focusable()
            .focused($focused)
            .focusEffectDisabled()
            .onKeyPress(.space) { toggleCursor(); return .handled }
            .onKeyPress(.downArrow) { moveCursor(1); return .handled }
            .onKeyPress(.upArrow) { moveCursor(-1); return .handled }
            if model.notEnoughSpace {
                Text("Not enough space at the destination").font(.system(size: 12)).foregroundStyle(Color.orange)
            }
            if showButtons {
                HeldButtons(hasSourceFile: hasSource, active: active, canStart: model.canConfirm,
                            startHelp: "Start downloading the ticked files",
                            cancel: { actions.cancel(id, $0) }, start: { actions.confirm(id) })
            }
        }
    }

    private var listHeight: CGFloat {
        min(maxListHeight, max(60, CGFloat(model.visibleRows.count) * 28 + 4))
    }

    private func toggleCursor() {
        let rows = model.visibleRows
        guard let item = rows.first(where: { $0.item.id == cursor })?.item ?? rows.first?.item else { return }
        cursor = item.id
        model.toggle(item)
    }

    private func moveCursor(_ delta: Int) {
        let rows = model.visibleRows.map(\.item.id)
        guard !rows.isEmpty else { return }
        let at = cursor.flatMap(rows.firstIndex(of:)) ?? (delta > 0 ? -1 : rows.count)
        cursor = rows[min(rows.count - 1, max(0, at + delta))]
    }

    private func row(_ item: ChoiceItem, level: Int) -> some View {
        let open = model.expanded.contains(item.id)
        let state = model.state(item)
        let dim = (item.inLibrary && state == .off) || item.isExtras
        return HStack(spacing: 4) {
            if level == 0 {
                FoldChevron(open: open, size: 9, width: 26, height: 28, help: open ? "Hide the contents" : "Show the contents") {
                    cursor = item.id
                    model.toggleExpanded(item)
                }
                .opacity(item.children.isEmpty ? 0 : 1).allowsHitTesting(!item.children.isEmpty)
            } else {
                Spacer().frame(width: 26)
            }
            TriCheck(state: state) { cursor = item.id; model.toggle(item) }
                .scaleEffect(0.85).frame(width: 22, height: 22)
            // The tap that ticks the row covers the title area only, never the chevron or the box.
            HStack(spacing: 6) {
                Text(item.title).font(.system(size: 12.5))
                    .foregroundStyle(dim ? .secondary : .primary)
                    .lineLimit(1).truncationMode(.middle)
                if item.inLibrary {
                    Text(item.badge).font(.system(size: 10.5)).foregroundStyle(Color.green)
                        .padding(.horizontal, 6).padding(.vertical, 1)
                        .background(Capsule().fill(Color.green.opacity(0.16)))
                }
                Spacer(minLength: 4)
                Text(Fmt.bytes(item.size)).font(.system(size: 11.5)).foregroundStyle(.secondary).monospacedDigit()
            }
            .frame(maxHeight: .infinity)
            .contentShape(Rectangle())
            .onTapGesture { cursor = item.id; focused = true; model.toggle(item) }
        }
        .padding(.leading, level == 0 ? 4 : 18).padding(.trailing, 8).frame(height: 28)
        .background(focused && cursor == item.id ? Color.accentColor.opacity(0.18) : .clear)
        .accessibilityElement(children: .contain)
        .accessibilityValue(state == .on ? "checked" : state == .mixed ? "partly checked" : "unchecked")
    }
}
