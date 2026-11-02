import AppKit
import Observation
import SwiftUI

// Developer mode (SPEC 8.7): Settings > Advanced > Developer (its rows are the settings model's): the tracker log
// viewer sheet. The viewer's lines, filters, words and pace are the core's `TrackerLog`; this draws its view. The log
// itself is written by the engine, with every secret already masked; nothing here sees a passkey.

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

/// The viewer's state, the core's (`TrackerLog`): it lives as long as the app, so the filters survive closing the
/// sheet. While the sheet is open it reads the tail again as often as the view says, off the main thread.
@MainActor @Observable final class TrackerLogModel {
    let core: Core
    let log: TrackerLog
    private(set) var view: TrackerLogPage
    @ObservationIgnored private var task: Task<Void, Never>?

    init(core: Core) {
        self.core = core
        log = TrackerLog(core: core)
        view = log.view()
    }

    var text: String {
        get { view.text }
        set { log.setText(text: newValue); changed() }
    }
    var event: String {
        get { view.event }
        set { log.setEvent(event: newValue); changed() }
    }
    var errorsOnly: Bool {
        get { view.errorsOnly }
        set { log.setErrorsOnly(on: newValue); changed() }
    }
    var live: Bool {
        get { view.live }
        set { log.setLive(on: newValue); view = log.view() }
    }

    func start() {
        guard task == nil else { return }
        task = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                if self.view.reads { await self.refresh() }
                try? await Task.sleep(for: .milliseconds(Int(self.view.refreshMs)))
            }
        }
    }

    func stop() {
        task?.cancel()
        task = nil
    }

    /// A filter changed: shown at once, then read for it.
    private func changed() {
        view = log.view()
        Task { await refresh() }
    }

    func refresh() async {
        await log.read()
        let next = log.view()
        if next != view { view = next }
    }

    func clear() {
        Task {
            await log.clear()
            view = log.view()
        }
    }

    func toggle(_ id: String) {
        log.toggle(id: id)
        view = log.view()
    }

    /// QA: opens exactly these lines.
    func open(_ ids: [String]) {
        log.setOpen(ids: ids)
        view = log.view()
    }
}

/// The sheet: filters on top, the tail below, newest at the bottom.
struct TrackerLogView: View {
    @Bindable var model: TrackerLogModel
    let core: Core
    @Binding var isPresented: Bool

    private var v: TrackerLogPage { model.view }

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
                Text(v.title).font(.system(size: 15, weight: .semibold))
                Text(v.subtitle).font(.system(size: 11.5)).foregroundStyle(.secondary)
            }
            Spacer()
            Button(v.doneTitle) { isPresented = false }.keyboardShortcut(.defaultAction)
                .help(v.doneHelp)
        }
        .padding(.horizontal, 20).padding(.vertical, 14)
    }

    private var filters: some View {
        HStack(spacing: 12) {
            HStack(spacing: 6) {
                Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                TextField(v.textPrompt, text: $model.text).textFieldStyle(.plain)
                if !v.text.isEmpty {
                    Button { model.text = "" } label: { Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary) }
                        .buttonStyle(.plain).accessibilityLabel(v.clearTextName)
                        .help(v.clearTextHelp)
                }
            }
            .padding(.horizontal, 8).padding(.vertical, 5)
            .background(RoundedRectangle(cornerRadius: 7).fill(.primary.opacity(0.06)))
            .frame(maxWidth: 300)
            Picker(v.eventName, selection: $model.event) {
                ForEach(v.events, id: \.value) { e in
                    Text(e.title).tag(e.value)
                    if e.separatorAfter { Divider() }
                }
            }
            .labelsHidden().fixedSize()
            .accessibilityLabel(v.eventName)
            Toggle(v.errorsOnlyTitle, isOn: $model.errorsOnly).toggleStyle(.checkbox)
            Spacer()
            Toggle(isOn: $model.live) {
                Label(v.liveTitle, systemImage: v.live ? "dot.radiowaves.left.and.right" : "pause.circle")
            }
            .toggleStyle(.button)
            .help(v.liveHelp)
        }
        .font(.system(size: 12.5))
        .padding(.horizontal, 20).padding(.vertical, 9)
    }

    private var list: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    if let e = v.empty {
                        emptyState(e)
                    }
                    ForEach(v.rows, id: \.id) { line in
                        TrackerLogRowView(line: line) { model.toggle(line.id) }
                        Divider().padding(.leading, 20)
                    }
                    Color.clear.frame(height: 1).id("end")
                }
            }
            .defaultScrollAnchor(.bottom)
            .onChange(of: v.rows.last?.id) { _, _ in
                if v.live { proxy.scrollTo("end", anchor: .bottom) }
            }
        }
    }

    private func emptyState(_ e: TrackerLogEmpty) -> some View {
        VStack(spacing: 6) {
            Image(systemName: "text.alignleft").font(.system(size: 26)).foregroundStyle(.tertiary)
            Text(e.title).font(.system(size: 13, weight: .medium))
            Text(e.text).font(.system(size: 12)).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity).padding(.top, 90)
    }

    private var footer: some View {
        HStack(spacing: 12) {
            Text(v.countLine).foregroundStyle(.secondary).monospacedDigit()
            if let d = v.droppedLine {
                Label(d, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
            }
            Spacer()
            Button(v.revealTitle) { TrackerLogFiles.revealInFinder(core: core) }
                .help(v.revealHelp)
            Button(v.clearTitle) { model.clear() }
                .help(v.clearHelp)
        }
        .font(.system(size: 12))
        .padding(.horizontal, 20).padding(.vertical, 9)
    }
}

struct TrackerLogRowView: View {
    let line: TrackerLogLine
    let toggle: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            Button(action: toggle) {
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 9, weight: .semibold)).foregroundStyle(.tertiary)
                        .rotationEffect(.degrees(line.detail != nil ? 90 : 0))
                        .frame(width: 10)
                    Text(line.time).font(.system(size: 11.5, design: .monospaced)).foregroundStyle(.secondary)
                    Text(line.title).font(.system(size: 12.5, weight: .medium)).lineLimit(1)
                    Text(line.host).font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(1)
                    Spacer(minLength: 8)
                    pill
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(line.help)
            .accessibilityLabel(line.accessibilityLabel)
            .accessibilityHint(line.accessibilityHint)
            Text(line.summary)
                .font(.system(size: 12))
                .foregroundStyle(line.error ? Color.red : .secondary)
                .lineLimit(line.detail != nil ? nil : 1)
                .textSelection(.enabled)
                .padding(.leading, 20)
            if let detail = line.detail {
                Text(detail)
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
        .background(line.error ? Color.red.opacity(0.06) : .clear)
    }

    private var pill: some View {
        HStack(spacing: 5) {
            if !line.transport.isEmpty { Text(line.transport) }
            Text(line.label)
        }
        .font(.system(size: 10.5, weight: .medium)).textCase(.uppercase)
        .foregroundStyle(line.error ? Color.red : .secondary)
        .padding(.horizontal, 7).padding(.vertical, 2)
        .background(Capsule().fill(.primary.opacity(0.07)))
    }
}
