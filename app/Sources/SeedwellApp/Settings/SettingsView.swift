import SwiftUI

/// Which tab is showing, and which tracker profile to open in Rules (from the sidebar's "Edit Tracker…").
@MainActor @Observable final class SettingsNavigation {
    var tab: SettingsTab = .general
    var trackerToEdit: String?
    /// Opens a tracker's tab in the main window ("Show Torrents" of a hidden tracker); wired by the window controller.
    @ObservationIgnored var openTracker: (String) -> Void = { _ in }
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

    var body: some View {
        SettingsPage {
            UpdatesGroup(model: model)
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

/// What a typed listening port means.
enum PortEntry: Equatable {
    /// Empty field: a random port at every start.
    case automatic
    case manual(UInt16)
    /// Not a number, or outside 1024-65535.
    case invalid

    static func parse(_ text: String) -> PortEntry {
        let t = text.trimmingCharacters(in: .whitespaces)
        if t.isEmpty { return .automatic }
        guard let v = UInt16(t), v >= 1024 else { return .invalid }
        return .manual(v)
    }

    static func text(_ port: UInt16) -> String { port == 0 ? "" : String(port) }
}

/// The field for a manual listening port. Empty = Automatic. Committed on Return or when it loses focus.
struct PortField: View {
    @Binding var port: UInt16
    @Binding var invalid: Bool
    @State private var text = ""
    @FocusState private var focused: Bool

    var body: some View {
        TextField("Automatic", text: $text)
            .textFieldStyle(.roundedBorder).frame(width: 90)
            .focused($focused)
            .onAppear { text = PortEntry.text(port) }
            .onChange(of: port) { if !focused { text = PortEntry.text(port) } }
            .onChange(of: focused) { if !focused { commit() } }
            .onSubmit { commit() }
            .accessibilityIdentifier("listen-port-field")
    }

    private func commit() {
        switch PortEntry.parse(text) {
        case .automatic: port = 0; invalid = false
        case .manual(let v): port = v; invalid = false; text = String(v)
        case .invalid: invalid = true; text = PortEntry.text(port)
        }
    }
}

/// Whether peers on the Internet can reach the port, in one word (the row's hint says why).
struct ReachabilityLabel: View {
    let state: String

    /// Word, symbol and colour for a `ReachabilityInfo.state`.
    static func look(_ state: String) -> (String, String, Color) {
        switch state {
        case "open": ("Open", "checkmark.circle.fill", .green)
        case "mapped": ("Opened on the router", "arrow.down.circle", .secondary)
        case "checking": ("Checking", "ellipsis.circle", .secondary)
        case "closed": ("Closed", "xmark.circle.fill", .orange)
        default: ("Not known", "questionmark.circle", .secondary)
        }
    }

    var body: some View {
        let (word, symbol, color) = Self.look(state)
        Label(word, systemImage: symbol).font(.system(size: 12)).foregroundStyle(color)
            .accessibilityIdentifier("reachability")
    }
}

/// To the right of the port field: the port in use, or why it is another one than the manual port.
struct ActivePortLabel: View {
    let active: UInt16
    let manual: UInt16
    let fallback: PortFallbackInfo?

    /// Plain words for a manual port that could not be bound.
    static func conflict(_ f: PortFallbackInfo) -> String { "Port \(f.wanted) is in use; using \(f.using)" }

    var body: some View {
        if let f = fallback, f.wanted == manual, manual != 0 {
            Label(Self.conflict(f), systemImage: "exclamationmark.triangle.fill")
                .font(.system(size: 12)).foregroundStyle(Color.orange)
                .accessibilityIdentifier("port-fallback-warning")
        } else if active != 0 {
            Text("Active: \(String(active))").font(.system(size: 12)).foregroundStyle(.secondary)
                .accessibilityIdentifier("active-port")
        }
    }
}

struct NetworkTab: View {
    @Bindable var model: SettingsModel
    @State private var portInvalid = false

    var body: some View {
        let n = $model.app.network
        SettingsPage {
            SettingsGroup(title: "Connection") {
                SettingsRow(label: "Listening port", hint: portInvalid ? "Use a port from 1024 to 65535, or leave it empty." : "Leave empty for Automatic: a random port at every start.") {
                    HStack(spacing: 10) {
                        PortField(port: n.listenPort, invalid: $portInvalid)
                        ActivePortLabel(active: model.activePort, manual: model.app.network.listenPort, fallback: model.portFallback)
                    }
                }
                SettingsRow(label: "Port mapping", hint: "PCP, NAT-PMP or UPnP on your router") { sw(n.portMapping) }
                if let r = model.reachability {
                    SettingsRow(label: "From the Internet", hint: r.summary) { ReachabilityLabel(state: r.state) }
                }
                SettingsRow(label: "Allow trackers on local network", hint: "For a tracker on your LAN or VPN. Off by default: trackers must be on the Internet.") {
                    sw(Binding(get: { model.app.network.allowLanTrackers }, set: { model.app.network.allowLanTrackers = $0 }))
                }
            }
            SettingsGroup(title: "Peers") {
                SettingsRow(label: "DHT", hint: "Public torrents only; always off for private ones") { sw(n.dht) }
                SettingsRow(label: "Peer exchange (PEX)", hint: "Public torrents. Private trackers: off unless allowed on the tracker's page.") { sw(n.pex) }
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
        .onAppear { model.refreshPort() }
        .task { await model.watchReachability() }
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
