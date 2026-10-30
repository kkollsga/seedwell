import SwiftUI

/// Get Info popover (SPEC §8.3), in the style of Finder's: grouped sections with a label column and a value column,
/// values selectable. The core decides every line, value, button and menu and how often the card asks again
/// (`InfoCard`, info_card.rs); this draws its model and runs its commands.
struct InfoCard: View {
    let core: Core
    let row: TransferRow
    let commands: Commands
    @State private var card: InfoCardHolder?
    @State private var contentHeight: CGFloat = 480
    @Environment(\.dismiss) private var dismiss

    /// As tall as the content wants, up to what the screen can show (never under 480 pt, never over 900).
    static var maxHeight: CGFloat {
        let screen = NSApp.keyWindow?.screen ?? NSScreen.main
        return min(900, max(480, (screen?.visibleFrame.height ?? 800) - 200))
    }
    static let gutter: CGFloat = 20
    /// The popover is this wide whatever the torrent's name or tracker URLs are (PopoverPlacement.width).
    static let width: CGFloat = PopoverPlacement.width
    static let contentWidth: CGFloat = width - 2 * gutter

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if let m = card?.model {
                ScrollView {
                    VStack(alignment: .leading, spacing: 0) {
                        header(m)
                        ForEach(m.sections.indices, id: \.self) { n in section(m.sections[n]) }
                    }
                    // Nothing inside may be wider than the popover: every line wraps or truncates in this width.
                    .frame(width: Self.contentWidth, alignment: .leading)
                    .padding(.horizontal, Self.gutter)
                    .padding(.top, 18)
                    .padding(.bottom, 6)
                    .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { contentHeight = $0 }
                }
                .scrollBounceBehavior(.basedOnSize)
                .frame(height: min(contentHeight, Self.maxHeight))
                Divider()
                HStack(spacing: 8) {
                    Spacer()
                    ForEach(m.buttons.indices, id: \.self) { n in
                        let b = m.buttons[n]
                        Button { run(b.command, closes: b.closesCard) } label: {
                            if b.destructive { Text(b.title).foregroundStyle(.red) } else { Text(b.title) }
                        }
                        .help(b.help)
                    }
                }
                .padding(.horizontal, 20)
                .padding(.vertical, 12)
            }
        }
        .frame(width: Self.width)
        .clipped()
        .onAppear {
            if card == nil { card = InfoCardHolder(core: core, row: row, commands: commands) }
            card?.start()
        }
        .onDisappear { card?.stop() }
        .onChange(of: row) { _, now in card?.update(row: now) }
    }

    // MARK: Header

    private func header(_ m: InfoCardModel) -> some View {
        HStack(spacing: 12) {
            FileIconView(ext: m.iconExt, linked: m.linked, size: 40)
            VStack(alignment: .leading, spacing: 2) {
                Text(m.title).font(.system(size: 15, weight: .semibold)).lineLimit(2).truncationMode(.middle)
                Text(m.subtitle).font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(1).truncationMode(.tail)
            }
            Spacer(minLength: 0)
        }
        .padding(.bottom, 4)
    }

    // MARK: Sections

    private func section(_ s: InfoCardSection) -> some View {
        InfoSectionView(s.title) {
            ForEach(s.rows.indices, id: \.self) { n in line(s.rows[n]) }
            if let ask = s.askAgain {
                HStack(spacing: 8) {
                    Button(ask.title) { run(ask.command) }
                        .controlSize(.small)
                        .help(ask.help)
                    if let note = s.askAgainNote {
                        Text(note).font(.system(size: 11.5)).foregroundStyle(.secondary)
                    }
                }
            }
            if !s.trackers.isEmpty {
                VStack(alignment: .leading, spacing: 12) {
                    ForEach(s.trackers, id: \.index) { t in
                        TrackerLineView(line: t, copy: { run(t.copy.command, tracker: t.index) },
                                        announceAnyway: { if let b = t.announceAnyway { run(b.command) } })
                    }
                }
                .padding(.top, 2)
            }
        }
    }

    @ViewBuilder private func line(_ r: InfoCardLine) -> some View {
        InfoRowView(r.label, selectable: r.selectable) {
            if let menu = r.menu {
                Menu(menu.title) {
                    MenuEntriesView(entries: commands.draw(menu.entries, rows: [row], videos: [], inMenuBar: false) { card?.reload() })
                }
                .menuStyle(.button)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: Self.contentWidth - 108, alignment: .leading)
                .help(menu.help)
            } else if !r.playControls.isEmpty {
                PlayControls(controls: r.playControls) { entries in
                    commands.draw(entries, rows: [row], videos: card?.videos ?? [], inMenuBar: false)
                }
            } else if let b = r.button {
                HStack(alignment: .firstTextBaseline, spacing: b.symbolOnly ? 6 : 10) {
                    value(r)
                    if b.symbolOnly {
                        Button { run(b.command) } label: {
                            Image(systemName: "arrow.right.circle.fill").foregroundStyle(.secondary)
                        }
                        .buttonStyle(.plain)
                        .help(b.help)
                        .accessibilityLabel(b.title)
                    } else {
                        Button(b.title) { run(b.command) }.controlSize(.small)
                            .help(b.help)
                    }
                }
            } else {
                value(r)
            }
        }
    }

    /// The value's runs in their colours, set as its style says.
    @ViewBuilder private func value(_ r: InfoCardLine) -> some View {
        let text = r.value.reduce(Text("")) { line, run in
            switch run.tone {
            case .primary: line + Text(run.text)
            case .secondary: line + Text(run.text).foregroundStyle(.secondary)
            case .problem: line + Text(run.text).foregroundStyle(Color.red)
            }
        }
        switch r.style {
        case .text: text.hoverHelp(r.help)
        case .long: text.lineLimit(3).truncationMode(.middle).hoverHelp(r.help)
        case .code: text.font(.system(size: 11, design: .monospaced)).hoverHelp(r.help)
        }
    }

    // MARK: Commands

    /// Runs a button's command for the card's row, then reads the torrent again.
    private func run(_ command: InfoCommand, closes: Bool = false, tracker: UInt32 = 0) {
        if closes { dismiss() }
        switch command {
        case .reveal: commands.reveal([row])
        case .remove: commands.confirmRemove([row], deleteFiles: false)
        case .chooseFiles: commands.showChooser(row.id)
        case .askTrackerAgain: commands.askTrackerAgain([row])
        case .announceAnyway: commands.announceAnyway([row])
        case .copyTrackerUrl: commands.copyTrackerURL(id: row.id, index: tracker)
        }
        if !closes { card?.reload() }
    }
}

/// The core's card of one row while its popover is open: a new model after `refreshMs` (the trackers' live status
/// ticks), when the list's row changes and after each command; the torrent read again off the main thread when the
/// model says (`reloadDueMs`).
@MainActor @Observable final class InfoCardHolder {
    @ObservationIgnored private let card: GetInfoCard
    @ObservationIgnored private var row: TransferRow
    /// The videos the Play entries name.
    @ObservationIgnored let videos: [TorrentFile]
    private(set) var model: InfoCardModel
    @ObservationIgnored private var timer: Timer?
    @ObservationIgnored private var running = false
    @ObservationIgnored private var reading = false
    /// The card on screen (QA reads it: `getinfo-card`).
    static weak var shown: InfoCardHolder?

    init(core: Core, row: TransferRow, commands: Commands) {
        let offer = commands.playOffer(row)
        self.row = row
        videos = offer.videos
        card = GetInfoCard(core: core, row: row, clock: MacClock(), chooserAvailable: commands.chooserAvailable, play: offer)
        model = card.model(row: row, nowUnixMs: Date().unixMs)
    }

    func start() {
        running = true
        Self.shown = self
        refresh()
    }

    func stop() {
        running = false
        if Self.shown === self { Self.shown = nil }
        timer?.invalidate()
        timer = nil
    }

    func update(row: TransferRow) {
        self.row = row
        refresh()
    }

    /// Reads the torrent again (after a command), then draws it.
    func reload() {
        card.reload()
        refresh()
    }

    private func refresh() {
        let next = card.model(row: row, nowUnixMs: Date().unixMs)
        if next != model { model = next }
        timer?.invalidate()
        timer = nil
        guard running else { return }
        if model.reloadDueMs == 0 { readAgain() }
        let wait = model.reloadDueMs == 0 ? model.refreshMs : min(model.refreshMs, model.reloadDueMs)
        let t = Timer(timeInterval: Double(wait) / 1000, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated { self?.refresh() }
        }
        t.tolerance = Double(model.refreshLeewayMs) / 1000
        RunLoop.main.add(t, forMode: .common)
        timer = t
    }

    /// The torrent's read is a slow call (the engine's detail of it): off the main thread, then drawn.
    private func readAgain() {
        guard !reading else { return }
        reading = true
        let card = self.card
        Task {
            await Task.detached(priority: .utility) { card.reload() }.value
            reading = false
            refresh()
        }
    }
}

private extension View {
    /// The hover text, when there is one.
    @ViewBuilder func hoverHelp(_ help: String?) -> some View {
        if let help { self.help(help) } else { self }
    }
}

/// A titled group of rows with a hairline above it.
private struct InfoSectionView<Content: View>: View {
    let title: String
    @ViewBuilder let content: Content

    init(_ title: String, @ViewBuilder content: () -> Content) {
        self.title = title
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(title).font(.system(size: 11, weight: .semibold)).foregroundStyle(.secondary)
            content
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.top, 14)
        .padding(.bottom, 2)
        .overlay(alignment: .top) { Divider().padding(.top, 0) }
        .padding(.top, 14)
    }
}

/// Label column (right-aligned, grey) and value column, like Finder's Get Info.
private struct InfoRowView<Value: View>: View {
    let label: String
    var selectable = true
    @ViewBuilder let value: Value

    init(_ label: String, selectable: Bool = true, @ViewBuilder value: () -> Value) {
        self.label = label
        self.selectable = selectable
        self.value = value()
    }

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text(label).foregroundStyle(.secondary)
                .frame(width: 96, alignment: .trailing)
            if selectable {
                value.textSelection(.enabled).fixedSize(horizontal: false, vertical: true).frame(maxWidth: .infinity, alignment: .leading)
            } else {
                value.fixedSize(horizontal: false, vertical: true).frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .font(.system(size: 12.5))
    }
}

/// One tracker: the URL (secret parts masked), then its live status and what it reported.
private struct TrackerLineView: View {
    let line: InfoTracker
    let copy: () -> Void
    let announceAnyway: () -> Void
    @State private var hovering = false

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                // One line, cut in the middle: the host and the end stay readable, the masked URL is in the tooltip.
                Text(line.url)
                    .font(.system(size: 11.5, design: .monospaced))
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .help(line.url)
                Button(action: copy) { Image(systemName: "doc.on.doc").font(.system(size: 11)) }
                    .buttonStyle(.plain)
                    .foregroundStyle(.secondary)
                    .opacity(hovering ? 1 : 0)
                    .help(line.copy.help)
                    .accessibilityLabel(line.copy.title)
            }
            VStack(alignment: .leading, spacing: 3) {
                Text(line.live)
                if let allowed = line.allowed {
                    HStack(spacing: 6) {
                        Text(allowed)
                        if let b = line.announceAnyway {
                            Button(b.title, action: announceAnyway).buttonStyle(.link).controlSize(.small)
                                .help(b.help)
                        }
                    }
                }
            }
            .font(.system(size: 11.5)).foregroundStyle(.secondary).monospacedDigit()
            .frame(maxWidth: .infinity, alignment: .leading)
            Text(line.detail).font(.system(size: 11.5)).foregroundStyle(.secondary).monospacedDigit()
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .contentShape(Rectangle())
        .onHover { hovering = $0 }
        .contextMenu { Button(line.copy.title, action: copy) }
    }
}

/// The Play entries as controls: a button for "Play", pop-up buttons for the submenus.
struct PlayControls: View {
    let controls: [InfoPlayControl]
    /// The entries as menu entries that run their commands.
    let draw: ([CommandEntry]) -> [MenuEntry]

    var body: some View {
        HStack(spacing: 8) {
            ForEach(controls.indices, id: \.self) { i in
                let c = controls[i]
                switch draw([c.entry]).first {
                case let .item(title, _, _, _, _, run):
                    Button(title) { run() }
                        .help(c.help)
                case let .submenu(title, children):
                    Menu(title) { MenuEntriesView(entries: children) }.menuStyle(.button).fixedSize()
                        .help(c.help)
                case .separator, nil:
                    EmptyView()
                }
            }
        }
    }
}
