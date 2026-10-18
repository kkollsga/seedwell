import SwiftUI

/// Get Info popover (SPEC §8.3), in the style of Finder's: grouped sections with a label column and a
/// value column, values selectable. General, Peers, Trackers, then the rule and the seeding goal.
struct InfoCard: View {
    let core: Core
    let row: TransferRow
    let commands: Commands
    @State private var info: TorrentInfo?
    /// Play and Play With (a pack lists its episodes); empty when there is no video to play.
    @State private var playEntries: [MenuEntry] = []
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
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    header
                    if let info {
                        general(info)
                        peers(info)
                        trackers(info)
                        goal(info)
                    }
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
                Button("Show in Finder") { commands.reveal([row]) }
                    .help("Show the downloaded files in Finder")
                Button { dismiss(); commands.confirmRemove([row], deleteFiles: false) } label: { Text("Remove…").foregroundStyle(.red) }
                    .help("Remove this torrent from Seedwell; asks under its row first, and keeps the files")
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 12)
        }
        .frame(width: Self.width)
        .clipped()
        .onAppear {
            info = try? core.info(id: row.id)
            playEntries = commands.playEntries(for: row)
        }
    }

    // MARK: Header

    private var header: some View {
        HStack(spacing: 12) {
            FileIconView(ext: row.fileExt, linked: row.kind == .direct, size: 40)
            VStack(alignment: .leading, spacing: 2) {
                Text(row.displayName).font(.system(size: 15, weight: .semibold)).lineLimit(2).truncationMode(.middle)
                Text(subtitle).font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(1).truncationMode(.tail)
            }
            Spacer(minLength: 0)
        }
        .padding(.bottom, 4)
    }

    // MARK: Sections

    private func general(_ i: TorrentInfo) -> some View {
        InfoSection("General") {
            InfoRow("Title") { Text(row.displayName) }
            if row.name != row.displayName { InfoRow("Release") { Text(row.name).lineLimit(3).truncationMode(.middle) } }
            if let problem = i.error { InfoRow("Problem") { Text(problem).foregroundStyle(i.trackerHalted ? Color.red : Color.orange) } }
            InfoRow("Category") { Text(i.category.label) }
            InfoRow("Size") { Text(Fmt.bytes(i.sizeBytes)) }
            InfoRow("State") { Text(Self.stateLabel(i.state)) }
            InfoRow("Added") { Text(Self.date(i.addedAt)) }
            if let f = i.finishedAt { InfoRow("Finished") { Text(Self.date(f)) } }
            if i.seededSecs > 0 { InfoRow("Seeding time") { Text(Fmt.duration(i.seededSecs)) } }
            if let t = row.lastActivity, row.isSharing { InfoRow("Last active") { Text(Fmt.ago(t)) } }
            InfoRow("Ratio") {
                Text(Fmt.ratio(i.ratio) + (row.ratioGoal.map { " of \(Fmt.goal($0))" } ?? ""))
            }
            InfoRow("Uploaded") { Text(Fmt.bytes(i.uploadedTotal)) }
            InfoRow("Downloaded") { Text(Fmt.bytes(i.downloadedTotal)) }
            InfoRow("Location") {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text(Self.tidy(i.savePath)).lineLimit(3).truncationMode(.middle)
                    Button { commands.reveal([row]) } label: {
                        Image(systemName: "arrow.right.circle.fill").foregroundStyle(.secondary)
                    }
                    .buttonStyle(.plain)
                    .help("Show in Finder")
                    .accessibilityLabel("Show in Finder")
                }
            }
            InfoRow("Files") {
                HStack(spacing: 10) {
                    (Text("\(i.filesWanted) of \(i.filesTotal)")
                        + Text(i.skippedNote.isEmpty ? "" : " · \(i.skippedNote)").foregroundStyle(.secondary))
                    if commands.chooserAvailable && i.filesTotal > 1 {
                        Button("Choose…") { commands.showChooser(row.id) }.controlSize(.small)
                            .help("Choose which files to download")
                    }
                }
            }
            if i.pieceCount > 0 { InfoRow("Pieces") { Text("\(i.pieceCount) × \(Self.pieceSize(i.pieceLength))") } }
            InfoRow("Info-hash") { Text(i.infoHash).font(.system(size: 11, design: .monospaced)) }
            InfoRow("Type") { Text(i.isPrivate ? "Private" : "Public") }
            if !playEntries.isEmpty { InfoRow("Play", selectable: false) { PlayControls(entries: playEntries) } }
        }
    }

    private func peers(_ i: TorrentInfo) -> some View {
        InfoSection("Peers") {
            InfoRow("Connected") {
                let leechers = i.peers > i.seedsConnected ? i.peers - i.seedsConnected : 0
                Text("\(i.peers)") + Text(i.peers > 0 ? " · \(i.seedsConnected) seeds, \(leechers) leechers" : "").foregroundStyle(.secondary)
            }
            InfoRow("Tracker swarm") {
                if i.swarmSeeders != nil || i.swarmLeechers != nil {
                    Text("\(i.swarmSeeders.map(String.init) ?? "?") seeders · \(i.swarmLeechers.map(String.init) ?? "?") leechers")
                } else {
                    Text("Not reported by the tracker").foregroundStyle(.secondary)
                }
            }
            if i.state == .downloading { InfoRow("Speed") { Text("\(Fmt.rate(i.downRate)) down") } }
        }
    }

    @ViewBuilder private func trackers(_ i: TorrentInfo) -> some View {
        if !i.trackers.isEmpty {
            InfoSection("Trackers") {
                if row.canAskAgain || i.trackerHalted {
                    HStack(spacing: 8) {
                        Button("Ask Tracker Again") { commands.askTrackerAgain([row]); reload() }
                            .controlSize(.small)
                            .help("Ask the tracker again now. Seedwell never does this by itself after a tracker said no, and asks at most once a minute.")
                        if i.trackerHalted {
                            Text("Announcing stopped until you ask.").font(.system(size: 11.5)).foregroundStyle(.secondary)
                        }
                    }
                }
                VStack(alignment: .leading, spacing: 12) {
                    ForEach(Array(i.trackers.enumerated()), id: \.offset) { n, t in
                        TrackerLineView(line: t) { commands.copyTrackerURL(id: row.id, index: UInt32(n)) }
                    }
                }
                .padding(.top, 2)
            }
        }
    }

    private func goal(_ i: TorrentInfo) -> some View {
        InfoSection("Rule and goal") {
            InfoRow("Rule") { Text(i.ruleName) }
            InfoRow("Then", selectable: false) {
                Menu(i.seedingGoal) {
                    Button("Rule default") { set(.ruleDefault) }
                    Button("Ratio 1.0") { set(.ratio(ratio: 1)) }
                    Button("Ratio 3.0") { set(.ratio(ratio: 3)) }
                    Button("Seed Forever") { set(.forever) }
                    Divider()
                    Button("Custom…") { commands.customGoal([row]); reload() }
                }
                .menuStyle(.button)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: Self.contentWidth - 108, alignment: .leading)
                .help("Choose what happens when this torrent is done downloading")
            }
            if let idle = i.idleText { InfoRow("Inactive") { Text(idle) } }
        }
    }

    // MARK: Helpers

    private func reload() { info = try? core.info(id: row.id) }

    private func set(_ goal: SeedingGoal) {
        core.setSeedingGoal(ids: [row.id], goal: goal)
        info = try? core.info(id: row.id)
    }

    private var subtitle: String {
        var parts = [row.category.label, Fmt.bytes(row.sizeBytes)]
        if row.state == .downloading, let i = info { parts.append("\(Fmt.rate(i.downRate)) from \(i.peers) peers") }
        else if let i = info, row.isSharing { parts.append(i.trackerName) }
        return parts.joined(separator: " · ")
    }

    static func stateLabel(_ s: TransferState) -> String {
        switch s {
        case .checking: "Checking files"
        case .metadata: "Fetching metadata"
        case .downloading: "Downloading"
        case .queued: "Waiting"
        case .paused: "Paused"
        case .seeding: "Seeding"
        case .lowPower: "Seeding (resting)"
        case .error: "Error"
        case .missingData: "Files missing"
        case .completed: "Finished"
        }
    }

    static func date(_ unix: UInt64) -> String {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US")
        f.dateStyle = .medium
        f.timeStyle = .short
        return f.string(from: Date(timeIntervalSince1970: TimeInterval(unix)))
    }

    static func pieceSize(_ bytes: UInt32) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .binary)
    }

    /// "~/Movies/Sprite Fright" for a path in the user's home.
    static func tidy(_ path: String) -> String { (path as NSString).abbreviatingWithTildeInPath }

    /// "Movies › Sprite Fright (2021)": the last two path components.
    static func breadcrumb(_ path: String) -> String {
        path.split(separator: "/").suffix(2).joined(separator: " › ")
    }
}

/// A titled group of rows with a hairline above it.
private struct InfoSection<Content: View>: View {
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
private struct InfoRow<Value: View>: View {
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

/// One tracker: the URL (secret parts masked), then its status and what it reported.
private struct TrackerLineView: View {
    let line: TrackerLine
    let copy: () -> Void
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
                    .help("Copy URL")
                    .accessibilityLabel("Copy URL")
            }
            Text(detail).font(.system(size: 11.5)).foregroundStyle(.secondary).monospacedDigit()
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .contentShape(Rectangle())
        .onHover { hovering = $0 }
        .contextMenu { Button("Copy URL", action: copy) }
    }

    private var detail: String {
        var parts = [line.status.prefix(1).uppercased() + line.status.dropFirst()]
        if let t = line.lastAnnounce { parts.append("last " + Fmt.ago(t)) }
        if let t = line.nextAnnounce { parts.append("next " + Fmt.inFuture(t)) }
        if line.seeders != nil || line.leechers != nil {
            parts.append("\(line.seeders.map(String.init) ?? "?") seeders · \(line.leechers.map(String.init) ?? "?") leechers")
        }
        return parts.joined(separator: " · ")
    }
}

/// The Play entries as controls: a button for "Play", pop-up buttons for the submenus.
struct PlayControls: View {
    let entries: [MenuEntry]

    var body: some View {
        HStack(spacing: 8) {
            ForEach(entries.indices, id: \.self) { i in
                switch entries[i] {
                case let .item(title, _, _, _, _, run):
                    Button(title) { run() }
                        .help("Play with \(title)")
                case let .submenu(title, children):
                    Menu(title) { MenuEntriesView(entries: children) }.menuStyle(.button).fixedSize()
                        .help("Choose what to play")
                case .separator:
                    EmptyView()
                }
            }
        }
    }
}
