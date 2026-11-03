import SwiftUI

/// Which tab is showing, and which tracker profile to open in Rules (from the sidebar's "Edit Tracker…").
@MainActor @Observable final class SettingsNavigation {
    var tab: SettingsTab = .general
    var trackerToEdit: String?
    /// Opens a tracker's tab in the main window ("Show Torrents" of a hidden tracker); wired by the window controller.
    @ObservationIgnored var openTracker: (String) -> Void = { _ in }
    /// "Create tracker profile…" for an unknown private tracker: its host, to pre-fill the editor.
    var newTrackerHost: String?
    /// A link to pair a device that opened the app: Settings › Devices opens Pair with a Link… with it.
    var pairLink: String?
    /// The tracker log viewer (Settings > Advanced > Developer) is open.
    var showTrackerLog = false
    @ObservationIgnored private var trackerLogModel: TrackerLogModel?

    /// The viewer's state lives as long as the app, so filters survive closing the sheet (and QA can reach it).
    func logModel(core: Core) -> TrackerLogModel {
        if let m = trackerLogModel { return m }
        let m = TrackerLogModel(core: core)
        trackerLogModel = m
        return m
    }
}

/// The Settings window: the tab bar, the tab (the core's model, drawn by `SettingsBlocksView`; Devices and Rules have
/// views of their own), the version line and the problems banner.
struct SettingsView: View {
    @Bindable var model: SettingsModel
    @Bindable var nav: SettingsNavigation

    init(model: SettingsModel, nav: SettingsNavigation = SettingsNavigation()) {
        self.model = model
        self.nav = nav
    }

    var body: some View {
        let page = model.page(nav.tab)
        VStack(spacing: 0) {
            tabBar(page.tabs)
            Divider()
            switch page.tab {
            case .devices: DevicesTab(core: model.core, nav: nav)
            case .rules: RulesTab(model: model, nav: nav)
            default:
                SettingsPage { SettingsBlocksView(blocks: page.blocks, model: model, act: act) }
            }
            if let footer = page.footer { versionFooter(footer) }
            if let b = page.banner { banner(b) }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .windowBackgroundColor))
        .onDisappear { model.save() }
        .environment(\.locale, Locale(identifier: "en_US"))
        .focusEffectDisabled()
        .task(id: page.tab) {
            model.opened(page.tab)
            while page.refreshMs > 0, !Task.isCancelled {
                await model.refreshLive(page.tab)
                try? await Task.sleep(for: .milliseconds(Int(page.refreshMs)))
            }
        }
        // Switched in System Settings: seen when the app comes back.
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in model.refreshFinder() }
        .sheet(isPresented: $nav.showTrackerLog) {
            TrackerLogView(model: nav.logModel(core: model.core), core: model.core, isPresented: $nav.showTrackerLog)
        }
    }

    /// The tracker log viewer is a sheet of this window; everything else is the model's.
    private func act(_ command: SettingsCommand) {
        if case .viewTrackerLog = command { nav.showTrackerLog = true } else { model.run(command) }
    }

    /// Like the About line of System Settings: one quiet, centred, selectable line ("Seedwell 0.0.1 (build 58)").
    private func versionFooter(_ line: String) -> some View {
        Text(line)
            .font(Theme.caption).foregroundStyle(.secondary)
            .textSelection(.enabled)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 8)
            .overlay(alignment: .top) { Divider() }
    }

    private func banner(_ b: SettingsBanner) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(Array(b.lines.enumerated()), id: \.offset) { _, line in
                switch line.tone {
                case .problem: Label(line.text, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red)
                case .restart: Label(line.text, systemImage: "arrow.clockwise").foregroundStyle(.orange)
                }
            }
        }
        .font(Theme.secondary)
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 28).padding(.vertical, 10)
        .background(.bar)
    }

    private func tabBar(_ tabs: [SettingsTabItem]) -> some View {
        HStack(spacing: 4) {
            ForEach(tabs, id: \.tab) { t in
                Button { nav.tab = t.tab } label: {
                    VStack(spacing: 4) {
                        Image(systemName: t.tab.symbol).font(.system(size: Theme.Icon.large)).frame(height: 22)
                        Text(t.title).font(Theme.caption)
                    }
                    .foregroundStyle(nav.tab == t.tab ? Color.accentColor : .secondary)
                    .frame(width: 72, height: 52)
                    .background(RoundedRectangle(cornerRadius: 10).fill(nav.tab == t.tab ? Color.primary.opacity(0.08) : .clear))
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help(t.help)
                .accessibilityLabel(t.title)
            }
        }
        .padding(.top, 10)
        .padding(.bottom, 8)
        .frame(maxWidth: .infinity)
    }
}

/// The UI is English-only for now, so numbers use US formatting regardless of the system locale.
let enUS = Locale(identifier: "en_US")
