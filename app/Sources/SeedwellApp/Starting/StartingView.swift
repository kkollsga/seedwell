import SwiftUI

/// What the notifier's clicks do with one torrent (each runs the core's `Starting`, then looks again).
struct StartingActions {
    /// A countdown's chevron: ends the countdown and opens the block's checklist here.
    var chooseFiles: (String) -> Void = { _ in }
    /// Start on a countdown: it starts now.
    var startNow: (String) -> Void = { _ in }
    /// Start on a choice: the ticks go to the engine. Cancel: the held torrent is removed and its `.torrent` file goes
    /// to the Trash; the flag is ⇧-Cancel: keep the file (SPEC 7A.6).
    var confirm: (String) -> Void = { _ in }
    var cancel: (_ id: String, _ keepFile: Bool) -> Void = { _, _ in }
    /// The title and its line: open the main window on this torrent, its file list open (SPEC 8.6).
    var open: (String) -> Void = { _ in }
    /// The category pill's menu: re-plan the torrent as this category.
    var changeCategory: (String, Category) -> Void = { _, _ in }
    /// The small ×: put the popover away and leave the torrents waiting.
    var closePanel: () -> Void = {}
    /// A choice's chevron: opens or closes its checklist.
    var toggleExpanded: (String) -> Void = { _ in }
}

/// The notifier's content (SPEC 8.6): one block per torrent that waits, to start or for a choice, as the core lays it
/// out (`StartingPage`). Dark first, native materials from the popover, no boxes: hierarchy from type and spacing.
struct StartingView: View {
    let model: StartingModel
    let store: ChoiceStore
    let actions: StartingActions

    static let width: CGFloat = 320
    /// The whole popover stays within this; blocks scroll when more are waiting (SPEC 8.6).
    static let maxHeight: CGFloat = 560

    var body: some View {
        let page = model.page
        ScrollView(.vertical) {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(Array(page.blocks.enumerated()), id: \.element.id) { index, block in
                    if index > 0 { Divider().padding(.vertical, page.blocks.count > 1 ? 12 : 14) }
                    Group {
                        if case let .countdown(countdown) = block.state {
                            StartingRow(block: block, countdown: countdown, actions: actions, compact: page.compact)
                        } else {
                            ChoiceBlock(block: block, store: store, actions: actions)
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
            if let help = page.closeHelp {
                Button(action: actions.closePanel) {
                    Image(systemName: "xmark").font(.system(size: 9, weight: .bold)).foregroundStyle(.secondary)
                        .frame(width: 20, height: 20).contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help(help)
                .padding(6)
            }
        }
        .animation(.easeOut(duration: 0.2), value: page.blocks.map(\.id))
        .animation(.easeOut(duration: 0.22), value: page.blocks.first { $0.state == .choice(expanded: true) }?.id)
    }
}

/// A torrent counting down: its name, what and where, the countdown, Cancel and Start.
struct StartingRow: View {
    let block: StartingBlock
    let countdown: StartingCountdown
    let actions: StartingActions
    /// Several torrents share the popover: tighter spacing, so that three fit without scrolling.
    var compact = false

    var body: some View {
        VStack(alignment: .leading, spacing: compact ? 9 : 12) {
            // The chevron is the one way to the file list: it ends the countdown and opens the checklist here.
            BlockHeader(block: block, actions: actions) { actions.chooseFiles(block.id) }
            // The clock only runs while the popover is on screen: the view does not exist otherwise.
            TimelineView(.animation(minimumInterval: 1.0 / 15)) { context in
                let tick = startingTick(countdown: countdown, nowMs: context.date.unixMs)
                HStack(spacing: 8) {
                    CountdownRing(fraction: tick.fraction, paused: tick.paused)
                        .frame(width: 16, height: 16)
                    Text(tick.line)
                        .font(.system(size: 12.5))
                        .monospacedDigit()
                        .foregroundStyle(tick.paused ? .secondary : .primary)
                        .contentTransition(.numericText())
                }
                .accessibilityElement(children: .combine)
            }
            HeldButtons(actions: block.actions, cancel: { actions.cancel(block.id, $0) }, start: { actions.startNow(block.id) })
        }
    }
}

/// A block's first line: the fold chevron (24 x 28 hit area; none for a magnet link that is only fetching) and the
/// title with its line, which together open the torrent in the main window, and the category pill.
struct BlockHeader: View {
    let block: StartingBlock
    let actions: StartingActions
    let chevronAction: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 0) {
            if let chevron = block.chevron {
                FoldChevron(open: chevron.open, size: 10, width: 24, height: 28, help: chevron.help, label: chevron.label, action: chevronAction)
                    .padding(.leading, -6)
            }
            StartingHeader(title: block.title, detail: block.detail, help: block.openHelp) { actions.open(block.id) }
            if let pill = block.category {
                CategoryPill(pill: pill) { actions.changeCategory(block.id, $0) }
                    .padding(.leading, 6)
            }
            // The room of the × in the corner.
            Color.clear.frame(width: 18, height: 1)
        }
    }
}

/// The title and the "Movie · ~/Media/Movies" line every block starts with.
struct StartingHeader: View {
    let title: String
    let detail: String
    /// The tooltip of the two lines as a link.
    var help = ""
    /// Clicking the two lines (one link): open the torrent in the main window. Nil: plain text.
    var open: (() -> Void)?
    @State private var hovering = false

    var body: some View {
        let text = VStack(alignment: .leading, spacing: 3) {
            Text(title)
                .font(.system(size: 14, weight: .semibold))
                .underline(hovering && open != nil)
                .lineLimit(2)
                .truncationMode(.middle)
                .fixedSize(horizontal: false, vertical: true)
            Text(detail)
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
                .help(help)
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

/// The category of a torrent as a small capsule in the corner of its block (SPEC 8.6): symbol and short label, and a
/// menu of every category on a click; the choice re-plans the torrent (destination, rule, title, structure).
struct CategoryPill: View {
    let pill: StartingCategory
    let choose: (Category) -> Void

    /// The menu: every category, the current one ticked (also what QA's `starting pill` shows as a native menu).
    static func entries(_ pill: StartingCategory, choose: @escaping (Category) -> Void) -> [MenuEntry] {
        pill.choices.map { c in .item(c.title, checked: c.checked) { choose(c.category) } }
    }

    var body: some View {
        Menu {
            ForEach(pill.choices, id: \.title) { c in
                Toggle(c.title, isOn: Binding(get: { c.checked }, set: { _ in choose(c.category) }))
            }
        } label: {
            HStack(spacing: 4) {
                Image(systemName: pill.category.symbol).font(.system(size: 9.5, weight: .medium))
                Text(pill.title).font(.system(size: 11, weight: .medium))
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
        .help(pill.help)
    }
}
