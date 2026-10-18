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

enum RowStyle {
    case download, queued, recent, popular, active, dormant, error, dead, stopped

    init(_ g: SeedGroup) {
        switch g {
        case .popular: self = .popular
        case .active: self = .active
        case .dormant: self = .dormant
        case .error: self = .error
        case .dead: self = .dead
        case .stopped: self = .stopped
        }
    }
}

extension TransferRow {
    /// The extension whose Finder icon the row shows: the file's own; for a multi-file torrent whose main
    /// content is video, the largest video's (the same icon a single-file video row gets); else a folder.
    var iconExt: String { fileExt.isEmpty && hasVideo && !videoExt.isEmpty ? videoExt : fileExt }

    /// The tracker said no or cannot be reached: Ask Tracker Again is offered.
    var canAskAgain: Bool {
        kind == .torrent && (trackerHalted || (waitingIsError && state == .downloading) || seedGroup == .error || seedGroup == .dead)
    }

    /// A download that is not moving, in the engine's words ("Tracker: Unregistered torrent",
    /// "Waiting for peers"): the progress, then why. `nil` while data flows or for anything but a download.
    var downloadWaiting: (lead: String, note: String, tail: String)? {
        guard let note = waitingNote, state == .downloading || waitingIsError, error == nil else { return nil }
        let lead = "\(Fmt.progress(done: progressDone, total: progressSize)) — "
        let tail = waitingIsError && !trackerHalted ? retryInSecs.map { " · retrying in \(Fmt.until($0))" } ?? "" : ""
        return (lead, note, tail)
    }

    /// What to do about a download that stopped with an error (not enough space, a disk problem).
    static let retryHint = " · Resume to try again"

    /// "4 of 250 peers": the connected peers against the swarm (the tracker's last seeders + leechers, else
    /// the peers known), else just "4 peers". Empty when nothing is connected. Downloads only: a Sharing row
    /// has its own short line (`sharingLine`). The rate is not here: it stands out at the right of the name
    /// (`headlineRate`).
    func liveSuffix(_ style: RowStyle) -> String {
        guard style == .download, peers > 0 || (state == .downloading && downRate > 0) else { return "" }
        guard let swarm = swarm.flatMap({ $0 > 0 ? $0 : nil }) ?? (knownPeers > 0 ? knownPeers : nil) else {
            return "\(peers) peer\(peers == 1 ? "" : "s")"
        }
        return "\(peers) of \(max(swarm, peers)) peers"
    }

    /// The live rate a row shows at the right of its name, big and in the primary colour: "↓ 2.1 MB/s" while a
    /// download moves data, "↑ 1.2 MB/s" while a sharing row uploads. Nil when nothing flows.
    func headlineRate(_ style: RowStyle) -> String? {
        switch style {
        case .download: return state == .downloading && downRate > 0 ? "↓ " + Fmt.rate(downRate) : nil
        case .popular, .active, .dormant: return movingPercent == nil && upRate > 0 ? "↑ " + Fmt.rate(upRate) : nil
        default: return nil
        }
    }

    /// Second line of a Sharing row: "1 peer · Ratio 0.27" (the upload rate stands out at the right of the
    /// name, `headlineRate`). The tracker, the goal, the seeding time and the swarm are in Get Info.
    var sharingLine: String {
        if let p = movingPercent { return "Moving… \(p)%" }
        let goal = ratioGoal.map { " of \(Fmt.goal($0))" } ?? ""
        return "\(Fmt.count(peers, "peer")) · Ratio \(Fmt.ratio(ratio))\(goal)"
    }

    /// Third line of a Sharing row: "3 of 24 files · 1.8 GB of 14 GB downloaded · 3 GB uploaded". The
    /// files are the ones fully downloaded, of all files; a single file leaves them out. The sizes
    /// are of the whole torrent.
    var sharingTotalsLine: String {
        var parts: [String] = []
        if fileCount > 1 { parts.append("\(filesDone) of \(fileCount) files") }
        // Against the whole torrent, skipped files included.
        let total = max(totalBytes, sizeBytes)
        let held = min(max(heldBytes, doneBytes), total)
        let down = held >= total ? Fmt.bytes(total) : Fmt.bytes(held) + " of " + Fmt.bytes(total)
        parts.append(down + " downloaded")
        parts.append(Fmt.bytes(uploadedBytes) + " uploaded")
        return parts.joined(separator: " · ")
    }

    /// Grey second line of a row.
    func statusLine(_ style: RowStyle, showTracker: Bool = true, now: Date = Date()) -> String {
        let line = baseLine(style, showTracker: showTracker, now: now)
        let live = liveSuffix(style)
        return live.isEmpty ? line : line + " · " + live
    }

    private func baseLine(_ style: RowStyle, showTracker: Bool, now: Date) -> String {
        let size = Fmt.bytes(sizeBytes)
        let goal = ratioGoal.map { " of \(Fmt.goal($0))" } ?? ""
        // Inside one tracker's view its name on every row is noise.
        let lead = showTracker ? "\(trackerName) · " : ""
        switch style {
        case .download:
            if awaitingChoice { return "Choose what to download…" }
            if let p = pending {
                // Starting in 7 s (the number follows the snapshot, once a second)
                return StartingText.line(remaining: p.remaining(at: now), paused: p.paused) + (p.paused ? " — won't start until you resume" : "")
            }
            if let p = movingPercent { return "Moving… \(p)%" }
            if let error { return state == .error ? error + Self.retryHint : error }
            if let w = downloadWaiting { return w.lead + w.note + w.tail }
            switch state {
            case .metadata: return "Fetching metadata…"
            case .checking: return "Checking files… \(percent)%"
            case .paused: return "Paused — \(Fmt.progress(done: progressDone, total: progressSize))"
            default: return "\(Fmt.progress(done: progressDone, total: progressSize)) — \(Fmt.remaining(etaSecs))"
            }
        case .queued:
            let lead = kind == .direct ? "\(trackerName) — " : ""
            if let p = movingPercent { return "Moving… \(p)%" }
            return lead + (state == .paused ? "Paused" : "Waiting") + " — \(size)"
        case .recent:
            let when = finishedAt.map { Fmt.ago($0) } ?? ""
            return "\(size) — \(when) · \(category.plural)"
        case .popular, .active, .dormant:
            return sharingLine
        case .error:
            var parts = (showTracker ? [trackerName] : []) + [error ?? "Problem"]
            if let r = retryInSecs { parts.append("retrying in \(Fmt.until(r))") }
            return parts.joined(separator: " · ")
        case .dead:
            return ((showTracker ? [trackerName] : []) + [error ?? "Gone"]).joined(separator: " · ")
        case .stopped:
            return "Ratio \(Fmt.ratio(ratio))\(goal) · Stopped" + (idleStopped.map { " — \($0)" } ?? "")
        }
    }
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
        .help(retry ? "Try again" : paused ? "Resume" : "Pause")
    }
}

struct TransferRowView: View {
    let row: TransferRow
    let style: RowStyle
    let selected: Bool
    let commands: Commands
    @Binding var infoID: String?
    /// False inside a tracker's own view, where every row would repeat its name.
    var showTracker = true
    /// A torrent with several files: the triangle that opens its file list. Nil = no triangle (its column stays).
    var disclosure: (expanded: Bool, toggle: () -> Void)?
    @State private var hovering = false
    /// Get Info opens below the row, or above it when the screen has no room below (PopoverPlacement).
    @State private var infoEdge: Edge = .bottom
    @State private var rowFrame: CGRect = .zero

    private var dimmed: Bool { style == .queued || style == .dormant || style == .stopped }
    private var attention: Bool { row.awaitingChoice || (style == .download && row.error != nil) }

    var body: some View {
        HStack(spacing: 14) {
            HStack(spacing: FileLayout.gap) {
                if let d = disclosure {
                    DisclosureTriangle(expanded: d.expanded, toggle: d.toggle)
                } else {
                    Color.clear.frame(width: FileLayout.triangle, height: 1)
                }
                FileIconView(ext: row.iconExt, linked: row.kind == .direct)
                    .opacity(dimmed ? 0.55 : 1)
            }
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    if row.awaitingChoice {
                        Circle().fill(Color.orange).frame(width: 8, height: 8).accessibilityHidden(true)
                    }
                    Text(row.displayName)
                        .font(.system(size: 13.5, weight: dimmed ? .regular : .medium))
                        .foregroundStyle(dimmed ? .secondary : .primary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .help(row.name == row.displayName ? "" : row.name)
                    if let rate = row.headlineRate(style) {
                        Spacer(minLength: 8)
                        Text(rate)
                            .font(.system(size: 13, weight: .medium)).monospacedDigit()
                            .foregroundStyle(.primary)
                            .lineLimit(1).fixedSize()
                            .help(style == .download ? "Download speed" : "Upload speed")
                    }
                }
                statusText
                    .font(.system(size: 12))
                    .monospacedDigit()
                    .foregroundStyle(attention ? Color.orange : .secondary)
                    .lineLimit(1)
                if style == .popular || style == .active || style == .dormant || style == .stopped {
                    Text(row.sharingTotalsLine)
                        .font(.system(size: 12))
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                if style == .download, !row.isHeld { ThinProgress(value: row.fraction).padding(.top, 5) }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            trailing
        }
        .padding(.leading, FileLayout.lead)
        .padding(.trailing, 10)
        .padding(.vertical, 9)
        .background(RoundedRectangle(cornerRadius: 10).fill(selected ? Color.accentColor.opacity(0.22) : .clear))
        .contentShape(Rectangle())
        .onHover { hovering = $0 }
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { frame in
            rowFrame = frame
            if infoID == row.id { infoEdge = PopoverPlacement.edge(rowInWindow: frame, height: InfoCard.maxHeight + 60) }
        }
        .onChange(of: infoID == row.id) { _, open in
            guard open else { return }
            infoEdge = PopoverPlacement.edge(rowInWindow: rowFrame, height: InfoCard.maxHeight + 60)
        }
        .popover(isPresented: Binding(get: { infoID == row.id }, set: { if !$0 { infoID = nil } }), arrowEdge: infoEdge) {
            InfoCard(core: commands.core, row: row, commands: commands)
        }
    }

    /// Errors and dead torrents colour only the reason: "tracker-z.cc · **Tracker unreachable** · retrying in 2 h".
    private var statusText: Text {
        switch style {
        case .error, .dead:
            let reason = Text(row.error ?? (style == .dead ? "Gone" : "Problem"))
                .foregroundStyle(style == .dead ? Color.red : Color.orange)
            var t = Text(showTracker ? row.trackerName + " · " : "") + reason
            if style == .error, let r = row.retryInSecs { t = t + Text(" · retrying in \(Fmt.until(r))") }
            return t
        case .download where row.downloadWaiting != nil && row.waitingIsError:
            // The reason alone takes the error colour: "0.0 of 1.9 GB — **Tracker: Unregistered torrent**"
            let w = row.downloadWaiting!
            return Text(w.lead) + Text(w.note).foregroundStyle(row.trackerHalted ? Color.red : Color.orange) + Text(w.tail)
        default:
            return Text(row.statusLine(style, showTracker: showTracker))
        }
    }

    @ViewBuilder private var trailing: some View {
        switch style {
        case .download:
            if row.awaitingChoice {
                HeldRowActions(id: row.id, commands: commands)
            } else if row.pending != nil {
                HeldRowActions(id: row.id, commands: commands, awaiting: false)
            } else {
                if row.canAskAgain { askAgainButton }
                if selected || hovering { infoButton }
                // Only next to a pause glyph: beside a paused row's resume triangle it would read as a
                // second resume button (Play stays in the context menu).
                if row.showsPlayButton && row.state != .paused && row.state != .error { playRing }
                PauseRing(fraction: row.fraction, paused: row.state == .paused || row.state == .error, retry: row.state == .error) {
                    if row.state == .paused || row.state == .error { commands.core.resume(ids: [row.id]) } else { commands.core.pause(ids: [row.id]) }
                }
            }
        case .queued:
            if selected || hovering { infoButton }
        case .recent:
            // Play sits right before the magnifier, the same plain accent glyph, so the two read as a pair.
            if row.showsPlayButton { playButton }
            Button { commands.reveal([row]) } label: {
                Image(systemName: "magnifyingglass").font(.system(size: 14, weight: .medium)).foregroundStyle(Color.accentColor)
            }
            .buttonStyle(.plain)
            .help("Show in Finder")
            .padding(.trailing, 4)
        case .popular, .active:
            // The rate and peers are in the grey line; the right edge keeps only controls.
            if row.showsPlayButton { playButton }
            infoSlot
        case .dormant:
            // Play first, then the moon in a fixed spot next to the info slot, so the moon lines
            // up on every resting row whether or not it has a Play button.
            if row.showsPlayButton { playButton }
            Image(systemName: "moon.fill").font(.system(size: 11)).foregroundStyle(.secondary)
                .frame(width: 16)
                .help("Low power: resting, wakes instantly")
            infoSlot
        case .error:
            Image(systemName: "exclamationmark.triangle.fill").font(.system(size: 13)).foregroundStyle(Color.orange).padding(.trailing, 4)
                .help(row.error ?? "Problem")
        case .dead:
            Image(systemName: "xmark.octagon.fill").font(.system(size: 13)).foregroundStyle(Color.red).help(row.error ?? "Gone")
            if row.canAskAgain { askAgainButton }
            Button("Remove…") { commands.confirmRemove([row], deleteFiles: false) }.controlSize(.small)
                .help("Remove this torrent from Seedwell; asks under its row first, and keeps the files")
        case .stopped:
            if row.showsPlayButton { playButton }
            Button { commands.core.resume(ids: [row.id]) } label: {
                Image(systemName: "play.circle").font(.system(size: 18, weight: .light)).foregroundStyle(Color.accentColor)
            }
            .buttonStyle(.plain)
            .help("Resume sharing")
            .padding(.trailing, 2)
        }
    }

    /// Play in the user's own player (SPEC §7F.3): a finished video opens, like Show in Finder next to it.
    private var playButton: some View {
        Button { commands.playButtonPressed(row) } label: {
            PlayGlyph().frame(width: 22, height: 22).contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help("Play")
        .accessibilityLabel("Play")
        .padding(.trailing, row.isSharing ? 0 : 6)
    }

    /// Play while downloading: the same plain triangle as on finished rows, next to the pause ring.
    private var playRing: some View {
        Button { commands.playButtonPressed(row) } label: {
            PlayGlyph().frame(width: 22, height: 26).contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help("Play while downloading")
        .accessibilityLabel("Play")
    }

    /// Get Info on a Sharing row: shown on hover or selection, in a fixed slot so the text never shifts.
    private var infoSlot: some View {
        ZStack {
            if selected || hovering { infoButton }
        }
        .frame(width: 22, height: 22)
    }

    /// The way out of a halted tracker: ask once more now (the tracker's minimum interval still applies).
    private var askAgainButton: some View {
        Button("Ask Again") { commands.askTrackerAgain([row]) }
            .controlSize(.small)
            .help("Ask the tracker again now. Seedwell never does this by itself after a tracker said no, and asks at most once a minute.")
    }

    private var infoButton: some View {
        Button { infoID = row.id } label: {
            Image(systemName: "info.circle").font(.system(size: 19, weight: .light)).foregroundStyle(Color.accentColor)
        }
        .buttonStyle(.plain)
        .help("Get Info")
    }
}
