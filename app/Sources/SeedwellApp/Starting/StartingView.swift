import SwiftUI

/// What the notifier can do with one torrent.
struct StartingActions {
    var chooseFiles: (String) -> Void
    var togglePause: (String, Bool) -> Void
    var startNow: (String) -> Void
    /// OK on a checklist: apply the ticks and start. Cancel: remove the held torrent and move its `.torrent`
    /// file to the Trash; the flag is ⇧-Cancel: keep the file (SPEC 7A.6).
    var confirm: (String) -> Void = { _ in }
    var cancel: (_ id: String, _ keepFile: Bool) -> Void = { _, _ in }
    /// The title and its line: open the main window on this torrent, its file list open (SPEC 8.6).
    var open: (String) -> Void = { _ in }
    /// The category pill's menu: re-plan the torrent as this category.
    var changeCategory: (String, Category) -> Void = { _, _ in }
    /// The small ×: put the popover away and leave the torrents waiting.
    var closePanel: () -> Void = {}
}

/// The notifier's content (SPEC 8.6): one block per torrent that waits, to start or for a choice. Dark
/// first, native materials from the popover, no boxes: hierarchy from type and spacing.
struct StartingView: View {
    let model: StartingModel
    let store: ChoiceStore
    let actions: StartingActions

    static let width: CGFloat = 320
    /// The whole popover stays within this; blocks scroll when more are waiting (SPEC 8.6).
    static let maxHeight: CGFloat = 560

    var body: some View {
        let items = model.items
        ScrollView(.vertical) {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(Array(items.enumerated()), id: \.element.id) { index, item in
                    if index > 0 { Divider().padding(.vertical, items.count > 1 ? 12 : 14) }
                    Group {
                        if item.choosing {
                            ChoiceBlock(item: item, model: store.models[item.id], expanded: model.isExpanded(item.id),
                                        active: model.activeBlockID == item.id, actions: actions,
                                        toggleExpanded: { model.toggleExpanded(item.id) })
                        } else {
                            StartingRow(item: item, actions: actions, compact: items.count > 1, active: model.activeBlockID == item.id)
                        }
                    }
                    .transition(.opacity)
                }
            }
            .padding(18)
        }
        .scrollBounceBehavior(.basedOnSize)
        .frame(width: Self.width)
        .frame(maxHeight: Self.maxHeight)
        .fixedSize(horizontal: false, vertical: true)
        .overlay(alignment: .topTrailing) {
            if model.hasChoice {
                Button(action: actions.closePanel) {
                    Image(systemName: "xmark").font(.system(size: 9, weight: .bold)).foregroundStyle(.secondary)
                        .frame(width: 20, height: 20).contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Close this and leave the torrents waiting. They stay in the menu and the main window")
                .padding(6)
            }
        }
        .animation(.easeOut(duration: 0.2), value: model.items.map(\.id))
        .animation(.easeOut(duration: 0.22), value: model.expandedID)
    }
}

/// A torrent: its name, what and where, the countdown, and the three things to do about it.
struct StartingRow: View {
    let item: StartingItem
    let actions: StartingActions
    /// Several torrents share the popover: the primary button shrinks to sit on one line with the
    /// secondary controls, so that three fit without scrolling.
    var compact = false
    /// Owns Return (Start) and Escape (Cancel).
    var active = false

    var body: some View {
        VStack(alignment: .leading, spacing: compact ? 9 : 12) {
            // The chevron is the one way to the file list: it ends the countdown and opens the checklist here.
            BlockHeader(item: item, chevron: .init(open: false, help: "Choose files: ends the countdown and shows the files") { actions.chooseFiles(item.id) },
                        actions: actions)
            // The clock only runs while the popover is on screen: the view does not exist otherwise.
            TimelineView(.animation(minimumInterval: 1.0 / 15)) { context in
                let now = context.date
                HStack(spacing: 8) {
                    CountdownRing(fraction: item.fraction(at: now), paused: item.paused)
                        .frame(width: 16, height: 16)
                    Text(item.line(at: now))
                        .font(.system(size: 12.5))
                        .monospacedDigit()
                        .foregroundStyle(item.paused ? .secondary : .primary)
                        .contentTransition(.numericText())
                }
                .accessibilityElement(children: .combine)
            }
            HeldButtons(hasSourceFile: item.hasSource, active: active, startHelp: "Skip the wait and start downloading now",
                        cancel: { actions.cancel(item.id, $0) }, start: { actions.startNow(item.id) })
        }
    }
}

/// A block's first line: the fold chevron (24 x 28 hit area; nil for a magnet link that is only fetching) and the
/// title with its line, which together open the torrent in the main window.
struct BlockHeader: View {
    struct Chevron {
        var open: Bool
        var help: String
        var action: () -> Void
    }
    let item: StartingItem
    var chevron: Chevron?
    let actions: StartingActions

    var body: some View {
        HStack(alignment: .top, spacing: 0) {
            if let chevron {
                FoldChevron(open: chevron.open, size: 10, width: 24, height: 28, help: chevron.help, action: chevron.action)
                    .padding(.leading, -6)
            }
            StartingHeader(item: item) { actions.open(item.id) }
            if item.showsCategory {
                CategoryPill(category: item.category) { actions.changeCategory(item.id, $0) }
                    .padding(.leading, 6)
            }
            // The room of the × in the corner.
            Color.clear.frame(width: 18, height: 1)
        }
    }
}

/// The title and the "Movie · ~/Media/Movies" line every block starts with.
struct StartingHeader: View {
    let item: StartingItem
    /// Clicking the two lines (one link): open the torrent in the main window. Nil: plain text.
    var open: (() -> Void)?
    @State private var hovering = false

    var body: some View {
        let text = VStack(alignment: .leading, spacing: 3) {
            Text(item.title)
                .font(.system(size: 14, weight: .semibold))
                .underline(hovering && open != nil)
                .lineLimit(2)
                .truncationMode(.middle)
                .fixedSize(horizontal: false, vertical: true)
            Text(item.detail)
                .font(.system(size: 12))
                .foregroundStyle(.secondary)
                .underline(hovering && open != nil)
                .lineLimit(1)
                .truncationMode(.middle)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        if let open {
            Button(action: open) { text.contentShape(Rectangle()) }
                .buttonStyle(.plain)
                .pointerStyle(.link)
                .onHover { hovering = $0 }
                .help("Open in Seedwell")
        } else {
            text
        }
    }
}

/// The small ring: full when the wait begins, empty when the torrent starts. Paused, it holds still and dims.
struct CountdownRing: View {
    let fraction: Double
    let paused: Bool

    var body: some View {
        ZStack {
            Circle().stroke(.primary.opacity(0.14), lineWidth: 2)
            Circle()
                .trim(from: 0, to: fraction)
                .stroke(paused ? Color.secondary : Color.accentColor, style: StrokeStyle(lineWidth: 2, lineCap: .round))
                .rotationEffect(.degrees(-90))
            if paused {
                Image(systemName: "pause.fill").font(.system(size: 6, weight: .bold)).foregroundStyle(.secondary)
            }
        }
    }
}

/// A secondary control: plain secondary text that takes the accent colour under the pointer.
struct SubtleButton: View {
    let title: String
    var help: String = ""
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            Text(title)
                .font(.system(size: 12.5))
                .lineLimit(1)
                .fixedSize()
                .foregroundStyle(hovering ? Color.accentColor : .secondary)
                .padding(.vertical, 3)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(help)
        .onHover { hovering = $0 }
    }
}

/// The category of a torrent as a small capsule in the corner of its block (SPEC 8.6): symbol and short label, and a
/// menu of every category on a click; the choice re-plans the torrent (destination, rule, title, structure).
struct CategoryPill: View {
    let category: Category
    let choose: (Category) -> Void

    /// The menu: every category, the current one ticked (also what QA's `starting pill` shows as a native menu).
    static func entries(category: Category, choose: @escaping (Category) -> Void) -> [MenuEntry] {
        Category.all.map { c in .item(c.label, checked: c == category) { choose(c) } }
    }

    var body: some View {
        Menu {
            ForEach(Array(Self.entries(category: category, choose: choose).enumerated()), id: \.offset) { _, entry in
                if case let .item(title, checked, _, _, _, run) = entry {
                    Toggle(title, isOn: Binding(get: { checked }, set: { _ in run() }))
                }
            }
        } label: {
            HStack(spacing: 4) {
                Image(systemName: category.symbol).font(.system(size: 9.5, weight: .medium))
                Text(category.label).font(.system(size: 11, weight: .medium))
            }
            .foregroundStyle(.secondary)
            .padding(.horizontal, 8).padding(.vertical, 3)
            .background(Capsule().fill(.primary.opacity(0.12)))
            .overlay(Capsule().strokeBorder(.primary.opacity(0.10), lineWidth: 0.5))
            .contentShape(Capsule())
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .help("Category: \(category.label) — click to change")
    }
}
