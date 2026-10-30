import SwiftUI

/// QA only: `playglyph <symbol>` swaps the Play icon of the rows to compare glyphs (the shipped one is the default).
@MainActor enum QAFlags {
    static var playGlyph = "play.fill"
}

/// Play a video: a plain triangle in the text colour (white in dark mode), no circle or button chrome,
/// so it never reads as the accent-coloured Resume control.
struct PlayGlyph: View {
    var size: CGFloat = 12

    var body: some View {
        Image(systemName: QAFlags.playGlyph).font(.system(size: size)).foregroundStyle(.primary)
    }
}

extension TransferRow {
    /// The tracker said no or cannot be reached: Ask Tracker Again is offered (Get Info).
    var canAskAgain: Bool { rowCanAskAgain(row: self) }
}

/// Finder's own icon for the file type; direct downloads get a small link badge.
struct FileIconView: View {
    let ext: String
    var linked = false
    var size: CGFloat = 38

    var body: some View {
        Image(nsImage: FileIcons.icon(ext: ext))
            .resizable()
            .interpolation(.high)
            .frame(width: size, height: size)
            .overlay(alignment: .bottomTrailing) {
                if linked {
                    Image(systemName: "link")
                        .font(.system(size: 8, weight: .bold))
                        .foregroundStyle(.white)
                        .frame(width: 16, height: 16)
                        .background(Circle().fill(Color.accentColor))
                        .overlay(Circle().stroke(Color(nsColor: .windowBackgroundColor), lineWidth: 1.5))
                        .offset(x: 3, y: 3)
                }
            }
    }
}

struct ThinProgress: View {
    let value: Double

    var body: some View {
        Capsule().fill(.primary.opacity(0.10))
            .overlay(alignment: .leading) {
                GeometryReader { geo in
                    Capsule().fill(Color.accentColor).frame(width: geo.size.width * max(0.01, min(1, value)))
                }
            }
            .frame(height: 3)
            .clipShape(Capsule())
    }
}

/// Progress and pause in one control, like Safari's downloads.
struct PauseRing: View {
    let fraction: Double
    let paused: Bool
    /// The row stopped on an error: the button retries (same action as resume, distinct glyph).
    var retry = false
    let help: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            ZStack {
                Circle().stroke(.primary.opacity(0.14), lineWidth: 2)
                Circle().trim(from: 0, to: max(0.02, fraction))
                    .stroke(Color.accentColor, style: StrokeStyle(lineWidth: 2, lineCap: .round))
                    .rotationEffect(.degrees(-90))
                Image(systemName: retry ? "arrow.clockwise" : paused ? "play.fill" : "pause.fill")
                    .font(.system(size: retry ? 10 : 9, weight: .bold))
                    .foregroundStyle(.primary)
            }
            .frame(width: 26, height: 26)
            .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .help(help)
    }
}

struct TransferRowView: View {
    let row: ListRow
    let commands: Commands
    /// The info button; and the popover was dismissed (the row's `infoOpen` says whether it is open).
    let openInfo: () -> Void
    let infoClosed: () -> Void
    /// A torrent with several files: the triangle that opens its file list. Nil = no triangle (its column stays).
    var disclosure: (expanded: Bool, toggle: () -> Void)?
    /// A click on the category pill (filters the list to it); nil = the pill is not clickable.
    var categoryAction: ((Category) -> Void)?
    @State private var hovering = false
    /// Get Info opens below the row, or above it when the screen has no room below (PopoverPlacement).
    @State private var infoEdge: Edge = .bottom
    @State private var rowFrame: CGRect = .zero

    /// Wide enough for "↑ 999.9 MB/s", so the text beside it never moves as the value changes.
    static let rateWidth: CGFloat = 96

    var body: some View {
        HStack(spacing: 14) {
            HStack(spacing: FileLayout.gap) {
                if let d = disclosure {
                    DisclosureTriangle(expanded: d.expanded, toggle: d.toggle)
                } else {
                    Color.clear.frame(width: FileLayout.triangle, height: 1)
                }
                FileIconView(ext: row.iconExt, linked: row.linked)
                    .opacity(row.dimmed ? 0.55 : 1)
            }
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    if row.attentionDot {
                        Circle().fill(Color.orange).frame(width: 8, height: 8).accessibilityHidden(true)
                    }
                    Text(row.title)
                        .font(.system(size: 13.5, weight: row.dimmed ? .regular : .medium))
                        .foregroundStyle(row.dimmed ? .secondary : .primary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .help(row.titleHelp ?? "")
                    RowCategoryPill(pill: row.pill, action: categoryAction)
                        .layoutPriority(1)
                }
                statusText
                    .font(.system(size: 12))
                    .monospacedDigit()
                    .lineLimit(1)
                if let detail = row.detail {
                    Text(detail)
                        .font(.system(size: 12))
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                if let progress = row.progress { ThinProgress(value: progress).padding(.top, 5) }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            if let slot = row.rate {
                Text(slot.text)
                    .font(.system(size: 13, weight: .medium)).monospacedDigit()
                    .foregroundStyle(slot.active ? Color.primary : Color.secondary)
                    .lineLimit(1)
                    .frame(width: Self.rateWidth, alignment: .trailing)
                    .help(slot.help)
            }
            ForEach(row.controls.indices, id: \.self) { i in control(row.controls[i]) }
        }
        .padding(.leading, FileLayout.lead)
        .padding(.trailing, 10)
        .padding(.vertical, 9)
        .background(RoundedRectangle(cornerRadius: 10).fill(row.selected ? Color.accentColor.opacity(0.22) : .clear))
        .contentShape(Rectangle())
        .onHover { hovering = $0 }
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { frame in
            rowFrame = frame
            if row.infoOpen { infoEdge = PopoverPlacement.edge(rowInWindow: frame, height: InfoCard.maxHeight + 60) }
        }
        .onChange(of: row.infoOpen) { _, open in
            guard open else { return }
            infoEdge = PopoverPlacement.edge(rowInWindow: rowFrame, height: InfoCard.maxHeight + 60)
        }
        .popover(isPresented: Binding(get: { row.infoOpen }, set: { if !$0 { infoClosed() } }), arrowEdge: infoEdge) {
            InfoCard(core: commands.core, row: row.transfer, commands: commands)
        }
    }

    /// The grey line in the core's colours: problems that need the user red, a row that waits for them orange.
    private var statusText: Text {
        row.status.reduce(Text("")) { line, run in
            switch run.tone {
            case .secondary: line + Text(run.text).foregroundStyle(.secondary)
            case .attention: line + Text(run.text).foregroundStyle(Color.orange)
            case .problem: line + Text(run.text).foregroundStyle(Color.red)
            }
        }
    }

    /// One control at the right of the row, as the core lists them.
    @ViewBuilder private func control(_ c: RowControl) -> some View {
        let t = row.transfer
        switch c.kind {
        case .askAgain:
            // The way out of a halted tracker: ask once more now (the tracker's minimum interval still applies).
            Button(c.title ?? "") { commands.askTrackerAgain([t]) }
                .controlSize(.small)
                .help(c.help)
        case .getInfo:
            if c.keepsSpace {
                // A fixed slot so the text never shifts.
                ZStack {
                    if row.selected || hovering { infoButton(c) }
                }
                .frame(width: 22, height: 22)
            } else if !c.hoverOnly || row.selected || hovering {
                infoButton(c)
            }
        case .playWhileDownloading:
            // The same plain triangle as on finished rows, next to the pause ring.
            Button { commands.playButtonPressed(t) } label: {
                PlayGlyph().frame(width: 22, height: 26).contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(c.help)
            .accessibilityLabel(c.accessibility ?? c.help)
        case .play:
            // Play in the user's own player (SPEC §7F.3): a finished video opens, like Show in Finder next to it.
            Button { commands.playButtonPressed(t) } label: {
                PlayGlyph().frame(width: 22, height: 22).contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(c.help)
            .accessibilityLabel(c.accessibility ?? c.help)
            .padding(.trailing, t.isSharing ? 0 : 6)
        case .pauseRing:
            let ring = c.ring ?? RingLook(fraction: 0, paused: false, retry: false)
            PauseRing(fraction: ring.fraction, paused: ring.paused, retry: ring.retry, help: c.help) {
                if ring.paused { commands.core.resume(ids: [t.id]) } else { commands.core.pause(ids: [t.id]) }
            }
        case .reveal:
            Button { commands.reveal([t]) } label: {
                Image(systemName: "magnifyingglass").font(.system(size: 14, weight: .medium)).foregroundStyle(Color.accentColor)
            }
            .buttonStyle(.plain)
            .help(c.help)
            .padding(.trailing, 4)
        case .lowPower:
            Image(systemName: "moon.fill").font(.system(size: 11)).foregroundStyle(.secondary)
                .frame(width: 16)
                .help(c.help)
        case .errorBadge:
            Image(systemName: "exclamationmark.triangle.fill").font(.system(size: 13)).foregroundStyle(Color.orange).padding(.trailing, 4)
                .help(c.help)
        case .deadBadge:
            Image(systemName: "xmark.octagon.fill").font(.system(size: 13)).foregroundStyle(Color.red).help(c.help)
        case .remove:
            Button(c.title ?? "") { commands.confirmRemove([t], deleteFiles: false) }.controlSize(.small)
                .help(c.help)
        case .resumeSharing:
            Button { commands.core.resume(ids: [t.id]) } label: {
                Image(systemName: "play.circle").font(.system(size: 18, weight: .light)).foregroundStyle(Color.accentColor)
            }
            .buttonStyle(.plain)
            .help(c.help)
            .padding(.trailing, 2)
        case .choiceActions:
            HeldRowActions(id: t.id, commands: commands, startHelp: c.help)
        case .countdownActions:
            HeldRowActions(id: t.id, commands: commands, awaiting: false, startHelp: c.help)
        }
    }

    private func infoButton(_ c: RowControl) -> some View {
        Button(action: openInfo) {
            Image(systemName: "info.circle").font(.system(size: 19, weight: .light)).foregroundStyle(Color.accentColor)
        }
        .buttonStyle(.plain)
        .help(c.help)
    }
}

/// The row's category as a small capsule after its name. A click filters the list to that category (and
/// clears the filter when it is on already); the context menu's Category submenu changes it.
struct RowCategoryPill: View {
    let pill: RowPill
    var action: ((Category) -> Void)?
    @State private var hovering = false

    var body: some View {
        let active = pill.active
        let capsule = HStack(spacing: 3) {
            Image(systemName: pill.category.symbol).font(.system(size: 8.5, weight: .medium))
            Text(pill.label).font(.system(size: 10.5, weight: .medium))
        }
        .foregroundStyle(active ? Color.accentColor : .secondary)
        .padding(.horizontal, 6).padding(.vertical, 1.5)
        .background(Capsule().fill(active ? Color.accentColor.opacity(0.16) : Color.primary.opacity(hovering && action != nil ? 0.12 : 0.07)))
        .lineLimit(1).fixedSize()
        if let action {
            Button { action(pill.category) } label: { capsule.contentShape(Capsule()) }
                .buttonStyle(.plain)
                .onHover { hovering = $0 }
                .help(pill.help)
                .accessibilityLabel(pill.accessibility)
        } else {
            capsule
        }
    }
}

/// "TV ×" above a filtered list: the category being shown, and the way back.
struct CategoryFilterChip: View {
    let chip: FilterChip
    let clear: () -> Void

    var body: some View {
        Button(action: clear) {
            HStack(spacing: 5) {
                Image(systemName: chip.category.symbol).font(.system(size: 10, weight: .medium))
                Text(chip.label).font(.system(size: 11.5, weight: .medium))
                Image(systemName: "xmark").font(.system(size: 8.5, weight: .bold))
            }
            .foregroundStyle(Color.accentColor)
            .padding(.horizontal, 9).padding(.vertical, 3)
            .background(Capsule().fill(Color.accentColor.opacity(0.16)))
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .help(chip.help)
        .accessibilityLabel(chip.accessibility)
    }
}
