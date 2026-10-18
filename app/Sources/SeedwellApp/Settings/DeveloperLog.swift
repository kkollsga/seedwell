import AppKit
import Observation
import SwiftUI

// Developer mode (SPEC 8.7): Settings > Advanced > Developer. The switch, three buttons and the viewer
// sheet. The log itself is written by the engine, with every secret already masked; nothing here sees
// a passkey.

/// The Developer group of Settings > Advanced.
struct DeveloperGroup: View {
    @Bindable var model: SettingsModel
    @Bindable var nav: SettingsNavigation

    var body: some View {
        let on = $model.app.advanced.developerMode
        SettingsGroup(title: "Developer") {
            HStack(alignment: .center, spacing: 12) {
                VStack(alignment: .leading, spacing: 3) {
                    Text("Developer mode")
                    Text("Logs every tracker request and answer to a file. Passkeys are never written.")
                        .font(.system(size: 11.5)).foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Spacer(minLength: 12)
                sw(on)
                    .accessibilityLabel("Developer mode")
                    .onChange(of: model.app.advanced.developerMode) { _, _ in model.save() }
            }
            .font(.system(size: 13))
            .padding(.horizontal, 14).padding(.vertical, 9)
            .frame(minHeight: 40)
            if model.app.advanced.developerMode {
                SettingsRow(label: "Tracker log") {
                    HStack(spacing: 8) {
                        Button("View Log…") { nav.showTrackerLog = true }
                            .help("Open the tracker log viewer with live updates and filters")
                        Button("Show Log in Finder") { TrackerLogFiles.revealInFinder(core: model.core) }
                            .help("Reveal the tracker log files in Finder")
                        Button("Clear Log") { model.core.trackerLogClear() }
                            .help("Delete all recorded tracker log entries")
                    }
                }
            }
        }
        .sheet(isPresented: $nav.showTrackerLog) {
            TrackerLogView(model: nav.logModel(core: model.core), core: model.core, isPresented: $nav.showTrackerLog)
        }
    }
}

enum TrackerLogFiles {
    /// Reveals `trackers.log`; before the first exchange there is no file, so the folder (or the state
    /// folder) opens instead.
    @MainActor static func revealInFinder(core: Core) {
        let file = URL(fileURLWithPath: core.trackerLogPath())
        let fm = FileManager.default
        if fm.fileExists(atPath: file.path) {
            NSWorkspace.shared.activateFileViewerSelecting([file])
        } else if fm.fileExists(atPath: file.deletingLastPathComponent().path) {
            NSWorkspace.shared.open(file.deletingLastPathComponent())
        } else {
            NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: core.stateDir())])
        }
    }
}

/// One line of the viewer; the id keeps a row's expansion while the tail refreshes.
struct TrackerLogItem: Identifiable, Equatable {
    let id: String
    let row: TrackerLogRow

    static func == (a: TrackerLogItem, b: TrackerLogItem) -> Bool { a.id == b.id && a.row.summary == b.row.summary }
}

/// The viewer's state: a live tail of the log with its filters. Reads off the main thread.
@MainActor @Observable final class TrackerLogModel {
    let core: Core
    var text = "" { didSet { if text != oldValue { refreshSoon() } } }
    /// "" = any event.
    var event = "" { didSet { if event != oldValue { refreshSoon() } } }
    var errorsOnly = false { didSet { if errorsOnly != oldValue { refreshSoon() } } }
    var live = true
    private(set) var items: [TrackerLogItem] = []
    private(set) var dropped: UInt64 = 0
    var expanded: Set<String> = []
    @ObservationIgnored private var task: Task<Void, Never>?

    init(core: Core) { self.core = core }

    func start() {
        guard task == nil else { return }
        task = Task { [weak self] in
            while !Task.isCancelled {
                if let self, self.live || self.items.isEmpty { await self.refresh() }
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }

    func stop() {
        task?.cancel()
        task = nil
    }

    private func refreshSoon() { Task { await refresh() } }

    func refresh() async {
        let q = TrackerLogQuery(text: text.trimmingCharacters(in: .whitespaces), tracker: "", event: event, errorsOnly: errorsOnly, limit: 400)
        let core = self.core
        let (rows, dropped) = await Task.detached(priority: .utility) { (core.trackerLog(query: q), core.trackerLogDropped()) }.value
        var seen: [String: Int] = [:]
        let next = rows.map { r -> TrackerLogItem in
            let base = "\(r.tsMs)|\(r.torrent)|\(r.kind)|\(r.event)|\(r.host)"
            let n = seen[base, default: 0]
            seen[base] = n + 1
            return TrackerLogItem(id: "\(base)|\(n)", row: r)
        }
        if next != items { items = next }
        self.dropped = dropped
    }

    func clear() {
        core.trackerLogClear()
        items = []
        expanded = []
        Task { await refresh() }
    }

    func toggle(_ id: String) {
        if expanded.contains(id) { expanded.remove(id) } else { expanded.insert(id) }
    }
}

private let timeFormat: DateFormatter = {
    let f = DateFormatter()
    f.locale = Locale(identifier: "en_US_POSIX")
    f.dateFormat = "HH:mm:ss"
    return f
}()

/// The sheet: filters on top, the tail below, newest at the bottom.
struct TrackerLogView: View {
    @Bindable var model: TrackerLogModel
    let core: Core
    @Binding var isPresented: Bool

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            filters
            Divider()
            list
            Divider()
            footer
        }
        .frame(minWidth: 780, idealWidth: 900, minHeight: 460, idealHeight: 620)
        .background(Color(nsColor: .windowBackgroundColor))
        .onAppear { model.start() }
        .onDisappear { model.stop() }
        .environment(\.locale, Locale(identifier: "en_US"))
        .accessibilityIdentifier("tracker-log")
    }

    private var header: some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 2) {
                Text("Tracker Log").font(.system(size: 15, weight: .semibold))
                Text("Every tracker request and answer, newest last. Passkeys and the client key are masked in the file itself.")
                    .font(.system(size: 11.5)).foregroundStyle(.secondary)
            }
            Spacer()
            Button("Done") { isPresented = false }.keyboardShortcut(.defaultAction)
                .help("Close the tracker log")
        }
        .padding(.horizontal, 20).padding(.vertical, 14)
    }

    private var filters: some View {
        HStack(spacing: 12) {
            HStack(spacing: 6) {
                Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                TextField("Torrent or tracker", text: $model.text).textFieldStyle(.plain)
                if !model.text.isEmpty {
                    Button { model.text = "" } label: { Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary) }
                        .buttonStyle(.plain).accessibilityLabel("Clear filter")
                        .help("Clear the search text")
                }
            }
            .padding(.horizontal, 8).padding(.vertical, 5)
            .background(RoundedRectangle(cornerRadius: 7).fill(.primary.opacity(0.06)))
            .frame(maxWidth: 300)
            Picker("Event", selection: $model.event) {
                Text("All events").tag("")
                Divider()
                Text("Started").tag("started")
                Text("Regular").tag("regular")
                Text("Completed").tag("completed")
                Text("Stopped").tag("stopped")
            }
            .labelsHidden().fixedSize()
            .accessibilityLabel("Event")
            Toggle("Errors only", isOn: $model.errorsOnly).toggleStyle(.checkbox)
            Spacer()
            Toggle(isOn: $model.live) {
                Label("Live", systemImage: model.live ? "dot.radiowaves.left.and.right" : "pause.circle")
            }
            .toggleStyle(.button)
            .help(model.live ? "Following the log. Click to pause." : "Paused. Click to follow the log again.")
        }
        .font(.system(size: 12.5))
        .padding(.horizontal, 20).padding(.vertical, 9)
    }

    private var list: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    if model.items.isEmpty {
                        emptyState
                    }
                    ForEach(model.items) { item in
                        TrackerLogRowView(item: item, expanded: model.expanded.contains(item.id)) { model.toggle(item.id) }
                        Divider().padding(.leading, 20)
                    }
                    Color.clear.frame(height: 1).id("end")
                }
            }
            .defaultScrollAnchor(.bottom)
            .onChange(of: model.items.last?.id) { _, _ in
                if model.live { proxy.scrollTo("end", anchor: .bottom) }
            }
        }
    }

    private var emptyState: some View {
        VStack(spacing: 6) {
            Image(systemName: "text.alignleft").font(.system(size: 26)).foregroundStyle(.tertiary)
            Text("Nothing logged yet").font(.system(size: 13, weight: .medium))
            Text("Entries appear here as torrents announce to their trackers.")
                .font(.system(size: 12)).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity).padding(.top, 90)
    }

    private var footer: some View {
        HStack(spacing: 12) {
            Text("\(model.items.count) entries").foregroundStyle(.secondary).monospacedDigit()
            if model.dropped > 0 {
                Label("\(model.dropped) dropped: the log could not keep up", systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
            }
            Spacer()
            Button("Show in Finder") { TrackerLogFiles.revealInFinder(core: core) }
                .help("Reveal the tracker log files in Finder")
            Button("Clear Log") { model.clear() }
                .help("Delete all recorded tracker log entries")
        }
        .font(.system(size: 12))
        .padding(.horizontal, 20).padding(.vertical, 9)
    }
}

struct TrackerLogRowView: View {
    let item: TrackerLogItem
    let expanded: Bool
    let toggle: () -> Void

    private var row: TrackerLogRow { item.row }

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            Button(action: toggle) {
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 9, weight: .semibold)).foregroundStyle(.tertiary)
                        .rotationEffect(.degrees(expanded ? 90 : 0))
                        .frame(width: 10)
                    Text(timeFormat.string(from: Date(timeIntervalSince1970: Double(row.tsMs) / 1000)))
                        .font(.system(size: 11.5, design: .monospaced)).foregroundStyle(.secondary)
                    Text(row.title.isEmpty ? row.id : row.title)
                        .font(.system(size: 12.5, weight: .medium)).lineLimit(1)
                    Text(row.host).font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(1)
                    Spacer(minLength: 8)
                    pill
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(expanded ? "Hide the request and the answer" : "Show the request and the answer")
            .accessibilityLabel("\(row.title.isEmpty ? row.id : row.title), \(row.host), \(label)")
            .accessibilityHint(expanded ? "Hides the request and the answer" : "Shows the request and the answer")
            Text(row.summary)
                .font(.system(size: 12))
                .foregroundStyle(row.error ? Color.red : .secondary)
                .lineLimit(expanded ? nil : 1)
                .textSelection(.enabled)
                .padding(.leading, 20)
            if expanded {
                Text(row.detail)
                    .font(.system(size: 11.5, design: .monospaced))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(10)
                    .background(RoundedRectangle(cornerRadius: 8).fill(.primary.opacity(0.055)))
                    .padding(.leading, 20)
                    .padding(.top, 2)
            }
        }
        .padding(.horizontal, 20).padding(.vertical, 8)
        .background(row.error ? Color.red.opacity(0.06) : .clear)
    }

    private var label: String { row.event.isEmpty ? row.kind : row.event }

    private var pill: some View {
        HStack(spacing: 5) {
            if !row.transport.isEmpty { Text(row.transport) }
            Text(label)
        }
        .font(.system(size: 10.5, weight: .medium)).textCase(.uppercase)
        .foregroundStyle(row.error ? Color.red : .secondary)
        .padding(.horizontal, 7).padding(.vertical, 2)
        .background(Capsule().fill(.primary.opacity(0.07)))
    }
}
