import SwiftUI

/// Which tab is showing, and which tracker profile to open in Rules (from the sidebar's "Edit Tracker…").
@MainActor @Observable final class SettingsNavigation {
    var tab: SettingsTab = .transfers
    var trackerToEdit: String?
    /// "Create tracker profile…" for an unknown private tracker: its host, to pre-fill the editor.
    var newTrackerHost: String?
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

struct SettingsView: View {
    @Bindable var model: SettingsModel
    @Bindable var nav: SettingsNavigation

    init(model: SettingsModel, nav: SettingsNavigation = SettingsNavigation()) {
        self.model = model
        self.nav = nav
    }

    var body: some View {
        VStack(spacing: 0) {
            tabBar
            Divider()
            switch nav.tab {
            case .general: GeneralTab(model: model)
            case .transfers: TransfersTab(model: model)
            case .folders: FoldersTab(model: model)
            case .rules: RulesTab(model: model, nav: nav)
            case .network: NetworkTab(model: model)
            case .agents: AgentsTab(model: model)
            case .advanced: AdvancedTab(model: model, nav: nav)
            }
            if nav.tab == .general { versionFooter }
            if !model.problems.isEmpty || !model.restartRequired.isEmpty { banner }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .windowBackgroundColor))
        .onDisappear { model.save() }
        .environment(\.locale, Locale(identifier: "en_US"))
        .focusEffectDisabled()
    }

    /// Like the About line of System Settings: one quiet, centred, selectable line ("Seedwell 0.0.1 (build 58)").
    private var versionFooter: some View {
        Text(AppInfo.versionLine)
            .font(.system(size: 11.5)).foregroundStyle(.secondary)
            .textSelection(.enabled)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 8)
            .overlay(alignment: .top) { Divider() }
    }

    private var banner: some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(model.problems, id: \.self) { p in
                Label(p, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red)
            }
            if !model.restartRequired.isEmpty {
                Label("Restart \(AppInfo.name) to apply: \(model.restartRequired.joined(separator: ", "))", systemImage: "arrow.clockwise")
                    .foregroundStyle(.orange)
            }
        }
        .font(.system(size: 12))
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 28).padding(.vertical, 10)
        .background(.bar)
    }

    private var tabBar: some View {
        HStack(spacing: 4) {
            ForEach(SettingsTab.allCases) { t in
                Button { nav.tab = t } label: {
                    VStack(spacing: 4) {
                        Image(systemName: t.symbol).font(.system(size: 17, weight: .regular)).frame(height: 22)
                        Text(t.title).font(.system(size: 11.5))
                    }
                    .foregroundStyle(nav.tab == t ? Color.accentColor : .secondary)
                    .frame(width: 72, height: 52)
                    .background(RoundedRectangle(cornerRadius: 10).fill(nav.tab == t ? Color.primary.opacity(0.08) : .clear))
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Open the \(t.title) settings")
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

/// A switch without a label, as in System Settings rows.
@MainActor func sw(_ on: Binding<Bool>) -> some View { Toggle("", isOn: on).labelsHidden().toggleStyle(.switch).help(on.wrappedValue ? "On, click to turn off" : "Off, click to turn on") }

// MARK: General

struct GeneralTab: View {
    @Bindable var model: SettingsModel
    @Bindable private var updates = UpdateSettings.shared

    var body: some View {
        SettingsPage {
            SettingsGroup(title: "Startup") {
                SettingsRow(label: "Launch at login") { sw($model.app.general.launchAtLogin) }
            }
            SettingsGroup(title: "Notifications") {
                SettingsRow(label: "Download finished") { sw($model.app.general.notifyFinished) }
                SettingsRow(label: "Errors") { sw($model.app.general.notifyErrors) }
                SettingsRow(label: "Seeding goal reached") { sw($model.app.general.notifyGoal) }
            }
            SettingsGroup(title: "Playback") {
                SettingsRow(label: "Play videos with", hint: "Automatic tries IINA, VLC, Infuse, then QuickTime Player (MP4 only)") {
                    PlayWithPicker(selection: $model.app.general.playWith)
                }
            }
            SettingsGroup(title: "Power") {
                SettingsRow(label: "Prevent sleep while downloading") { sw($model.app.general.preventSleepWhileDownloading) }
                SettingsRow(label: "On battery", hint: "Applies the Limit Speed settings, or pauses downloads") {
                    Picker("", selection: $model.app.general.onBattery) {
                        Text("Keep going").tag(OnBatteryMode.keepGoing)
                        Text("Limit Speed").tag(OnBatteryMode.limit)
                        Text("Pause downloads").tag(OnBatteryMode.pause)
                    }
                    .pickerStyle(.segmented).labelsHidden().fixedSize()
                }
            }
            SettingsGroup(title: "Updates") {
                SettingsRow(label: "Check automatically", hint: updates.configured ? nil : "Updates are not set up in this build") {
                    sw($updates.automaticChecks).disabled(!updates.configured)
                }
                SettingsRow(label: "Check for updates") {
                    Button("Check Now") { updates.checkNow() }.disabled(!updates.configured)
                        .help("Look for a newer version of Seedwell now")
                }
            }
        }
    }
}

/// The "Play videos with" pop-up: Automatic, then every installed player. A player that cannot run on this
/// Mac (an Intel-only VLC without Rosetta) stays in the list, disabled, marked "(needs update)".
struct PlayWithPicker: View {
    @Binding var selection: String
    @State private var players: [InstalledPlayer] = []

    var body: some View {
        Picker("", selection: $selection) {
            Text("Automatic").tag("auto")
            ForEach(players) { p in
                Text(p.menuTitle).tag(p.id).disabled(!p.runs)
            }
            // A player the setting names but this Mac no longer has: keep it visible instead of a blank pop-up.
            if selection != "auto", !players.contains(where: { $0.id == selection }) {
                Text("\(KnownPlayers.spec(forID: selection)?.name ?? selection) (not installed)").tag(selection).disabled(true)
            }
        }
        .labelsHidden().fixedSize()
        .onAppear { players = PlayerDirectory.shared.installed(maxAge: 0) }
    }
}

// MARK: Transfers (concept 8)

struct TransfersTab: View {
    @Bindable var model: SettingsModel

    var body: some View {
        let t = $model.app.transfers
        SettingsPage {
            SettingsGroup(title: "Speed") {
                SettingsRow(label: "Download limit", hint: "Shared by torrents and direct downloads. Empty is no limit.") { RateField(bytes: t.downloadLimit) }
                SettingsRow(label: "Upload limit", hint: "A favorite tracker can have its own, in its Info page") { RateField(bytes: t.uploadLimit) }
                SettingsRow(label: "Limit Speed (alt.)", hint: "Toggle with ⌥⌘L or Transfers ▸ Limit Speed") {
                    HStack(spacing: 8) {
                        Text("↓")
                        RateField(bytes: t.altDownloadLimit, width: 58)
                        Text("↑")
                        RateField(bytes: t.altUploadLimit, width: 58)
                    }
                }
                SettingsRow(label: "Schedule", hint: "Limit Speed turns on in this window") {
                    HStack(spacing: 10) {
                        sw(t.scheduleEnabled)
                        Picker("", selection: Binding(get: { dayChoice(t.scheduleDays.wrappedValue) }, set: { t.scheduleDays.wrappedValue = $0.days })) {
                            ForEach(DayChoice.allCases) { Text($0.rawValue).tag($0) }
                        }
                        .pickerStyle(.segmented).labelsHidden().fixedSize().disabled(!t.scheduleEnabled.wrappedValue)
                        HStack(spacing: 6) {
                            TimeField(minute: t.scheduleFromMinute)
                            Text("–").foregroundStyle(.secondary)
                            TimeField(minute: t.scheduleToMinute)
                        }
                        .disabled(!t.scheduleEnabled.wrappedValue)
                    }
                }
            }
            SettingsGroup(title: "Queue") {
                SettingsRow(label: "Active downloads", hint: "Torrents and direct downloads together.\nThe rest wait in Up Next.") {
                    TextField("", value: t.maxActiveDownloads, format: .number).textFieldStyle(.roundedBorder).frame(width: 62)
                }
                SettingsRow(label: "Connections per host", hint: "Direct downloads. Polite to servers.") {
                    TextField("", value: t.maxConnectionsPerHost, format: .number).textFieldStyle(.roundedBorder).frame(width: 62)
                }
                SettingsRow(label: "Wait before starting", hint: "A new torrent is held, silent, while you can look. 0 starts at once.") {
                    HStack(spacing: 8) {
                        TextField("", value: t.startDelaySecs, format: .number).textFieldStyle(.roundedBorder).frame(width: 62)
                        Text("s")
                    }
                }
                SettingsRow(label: "Ask before adding") {
                    Picker("", selection: t.askBeforeAdding) {
                        Text("Never").tag(AskMode.never)
                        Text("Multi-file torrents only").tag(AskMode.multiFile)
                        Text("Always").tag(AskMode.always)
                    }
                    .labelsHidden().fixedSize()
                }
                SettingsRow(label: "Ask above", hint: "Torrents larger than this show the chooser first. 0 = never.") {
                    HStack(spacing: 8) {
                        TextField("", value: t.largeTorrentGb, format: .number).textFieldStyle(.roundedBorder).frame(width: 62)
                        Text("GB")
                    }
                }
            }
            SettingsGroup(title: "Seeding") {
                SettingsRow(label: "Default goal", hint: "The Default rule. Tracker rules override it.") {
                    GoalEditor(goal: Binding(get: { model.rulesDoc.defaultRule.goal }, set: { model.rulesDoc.defaultRule.goal = $0; model.saveRules() }))
                }
                SettingsRow(label: "When a goal is reached") {
                    Picker("", selection: Binding(get: { model.rulesDoc.defaultRule.onGoal }, set: { model.rulesDoc.defaultRule.onGoal = $0; model.saveRules() })) {
                        Text("Stop, keep in list").tag(OnGoalAction.stopKeep)
                        Text("Remove, keep files").tag(OnGoalAction.removeKeepFiles)
                    }
                    .labelsHidden().fixedSize()
                }
                SettingsRow(label: "Private torrents", hint: "Hit-and-run protection. A tracker rule’s own minimum wins.") {
                    HStack(spacing: 8) {
                        Text("Seed at least")
                        PrivateFloorPicker(secs: t.privateMinSeedSecs)
                    }
                }
                SettingsRow(label: "Inactive seeds", hint: "Only hours spent seeding count. Empty = off.") {
                    IdleStopField(bytes: t.idleMinUploadBytes, hours: t.idleWindowHours)
                }
            }
        }
    }

    enum DayChoice: String, CaseIterable, Identifiable {
        case weekdays = "Weekdays", weekends = "Weekends", everyDay = "Every day"
        var id: String { rawValue }
        var days: ScheduleDays { switch self { case .weekdays: .weekdays; case .weekends: .weekends; case .everyDay: .everyDay } }
    }

    private func dayChoice(_ d: ScheduleDays) -> DayChoice {
        switch d {
        case .weekends: .weekends
        case .everyDay: .everyDay
        default: .weekdays
        }
    }
}

// MARK: Network

/// One line under the listening port on the first launch: Transmission is installed and uses a
/// port the router may already forward. A suggestion, not a dialog.
struct TransmissionPortSuggestion: View {
    let port: UInt16
    let use: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "arrow.triangle.2.circlepath").foregroundStyle(.secondary)
            Text("Transmission uses port \(String(port))").foregroundStyle(.secondary)
            Button("Use it", action: use).buttonStyle(.link)
                .help("Use Transmission's port for Seedwell")
            Spacer(minLength: 0)
        }
        .font(.system(size: 12.5))
        .padding(.horizontal, 14).padding(.vertical, 8)
        .accessibilityElement(children: .combine)
    }
}

struct NetworkTab: View {
    @Bindable var model: SettingsModel

    var body: some View {
        let n = $model.app.network
        SettingsPage {
            SettingsGroup(title: "Connection") {
                SettingsRow(label: "Listening port", hint: "Random on first launch") {
                    TextField("", value: n.listenPort, format: .number.grouping(.never)).textFieldStyle(.roundedBorder).frame(width: 80)
                }
                if let port = model.transmissionPortOffer {
                    TransmissionPortSuggestion(port: port) { model.useTransmissionPort() }
                }
                if let f = model.portFallback, f.wanted == model.app.network.listenPort {
                    PortFallbackWarning(message: f.message)
                }
                SettingsRow(label: "Port mapping", hint: "PCP, NAT-PMP or UPnP on your router") { sw(n.portMapping) }
                SettingsRow(label: "Allow trackers on local network", hint: "For a tracker on your LAN or VPN. Off by default: trackers must be on the Internet.") {
                    sw(Binding(get: { model.app.network.allowLanTrackers }, set: { model.app.network.allowLanTrackers = $0 }))
                }
            }
            SettingsGroup(title: "Peers") {
                SettingsRow(label: "DHT", hint: "Public torrents only; always off for private ones") { sw(n.dht) }
                SettingsRow(label: "Peer exchange", hint: "Public torrents only") { sw(n.pex) }
                SettingsRow(label: "Encryption", hint: "Peer connections. Require refuses peers that cannot encrypt.") {
                    Picker("", selection: n.encryption) {
                        Text("Off (plain only)").tag(EncryptionSetting.off)
                        Text("Prefer encrypted").tag(EncryptionSetting.prefer)
                        Text("Require encrypted").tag(EncryptionSetting.require)
                    }
                    .labelsHidden().fixedSize()
                }
                SettingsRow(label: "uTP", hint: "Peers over UDP. Turning it on applies after a restart.") { sw(n.utp) }
                SettingsRow(label: "Connections", hint: "Across all torrents, and per torrent") {
                    HStack(spacing: 8) {
                        TextField("", value: n.maxConnections, format: .number).textFieldStyle(.roundedBorder).frame(width: 62)
                        Text("/")
                        TextField("", value: n.maxConnectionsPerTorrent, format: .number).textFieldStyle(.roundedBorder).frame(width: 62)
                    }
                }
            }
        }
    }
}

// MARK: Advanced

struct AdvancedTab: View {
    @Bindable var model: SettingsModel
    @Bindable var nav: SettingsNavigation

    var body: some View {
        let a = $model.app.advanced
        SettingsPage {
            SettingsGroup(title: "Disk") {
                SettingsRow(label: "Write cache", hint: "Applies after a restart") {
                    Picker("", selection: a.writeCacheMib) {
                        ForEach([16, 32, 64, 128, 256], id: \.self) { Text("\($0) MB").tag(UInt32($0)) }
                    }
                    .labelsHidden().fixedSize()
                }
                SettingsRow(label: "Preallocation") {
                    Picker("", selection: a.preallocation) {
                        Text("Sparse files").tag(PreallocationMode.sparse)
                        Text("Reserve full size").tag(PreallocationMode.full)
                    }
                    .labelsHidden().fixedSize()
                }
                SettingsRow(label: "Verify on completion") { sw(a.verifyOnCompletion) }
            }
            SettingsGroup(title: "Diagnostics") {
                SettingsRow(label: "Engine", hint: "Since launch. Wake-ups are what idle costs.") {
                    let d = model.core.diagnostics()
                    Text("\(d.connections) connections · \(d.wakeups) wake-ups · \(d.rechecks) rechecks · \(d.hashFailures) bad pieces")
                        .foregroundStyle(.secondary).monospacedDigit()
                }
            }
            DeveloperGroup(model: model, nav: nav)
            SettingsGroup(title: "Files") {
                SettingsRow(label: "State and settings", hint: "config.toml, rules.toml, trackers.toml and state.redb") {
                    Button("Show in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: model.core.stateDir())])
                    }
                    .help("Reveal the settings and state folder in Finder")
                }
            }
        }
    }
}

/// Under the port field: the configured port is taken by another app, another one is used for now.
struct PortFallbackWarning: View {
    let message: String

    var body: some View {
        Label(message, systemImage: "exclamationmark.triangle.fill")
            .font(.system(size: 12))
            .foregroundStyle(Color.orange)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14).padding(.bottom, 9)
            .accessibilityIdentifier("port-fallback-warning")
    }
}


/// "Private torrents: seed at least [10 days]": the hit-and-run floor (SPEC §7, §7B.4a). Off and the
/// usual choices; a custom value from `config.toml` is shown as it is.
struct PrivateFloorPicker: View {
    @Binding var secs: UInt64
    static let day: UInt64 = 86_400
    static let days: [UInt64] = [1, 3, 7, 10, 14, 30]

    static func label(_ secs: UInt64) -> String {
        if secs == 0 { return "Off" }
        if secs % day == 0 { let d = secs / day; return d == 1 ? "1 day" : "\(d) days" }
        if secs % 3600 == 0 { let h = secs / 3600; return h == 1 ? "1 hour" : "\(h) hours" }
        return "\(max(1, secs / 60)) min"
    }

    var body: some View {
        let standard = [UInt64(0)] + Self.days.map { $0 * Self.day }
        Picker("", selection: $secs) {
            ForEach(standard, id: \.self) { Text(Self.label($0)).tag($0) }
            if !standard.contains(secs) { Text(Self.label(secs)).tag(secs) }
        }
        .labelsHidden().fixedSize()
        .help("How long a private torrent keeps seeding before its goal can stop it. Off lets the goal decide alone. Stopping or removing by hand always works.")
    }
}
