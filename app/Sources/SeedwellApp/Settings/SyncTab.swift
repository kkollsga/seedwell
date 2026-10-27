import FinderSync
import SwiftUI

/// Settings › Folder Sync (SPEC 7G.5): this Mac's name, the sync port, how devices are found and reached, Internet
/// limits and where joined folders go. These are folder sync's own (`sync.toml`), separate from the torrent settings.
struct SyncTab: View {
    @Bindable var model: SettingsModel
    @State private var portError: String?
    @State private var finderOn = FinderStatusWriter.extensionEnabled
    @State private var shareOn = ShareMenu.enabled

    var body: some View {
        SettingsPage {
            if let s = model.sync {
                let b = Binding(get: { model.sync ?? s }, set: { model.sync = $0 })
                SettingsGroup(title: "This Mac") {
                    SettingsRow(label: "Device name", hint: "What your other devices and the people you share with see") {
                        DeviceNameField(name: b.deviceName, placeholder: s.defaultDeviceName)
                    }
                    SettingsRow(label: "Your name", hint: "Shown with the device name in chats. Every member sees the change.") {
                        DeviceNameField(name: b.userName, placeholder: "None", identifier: "sync-user-name")
                    }
                    SettingsRow(label: "Sync port", hint: portHint) {
                        SyncPortField(port: s.port) { port in portError = model.setSyncPort(port) } invalid: {
                            portError = "Use a port from 1024 to 65535."
                        }
                    }
                    if let r = model.syncReachability {
                        SettingsRow(label: "From the Internet", hint: r.summary) { ReachabilityLabel(state: r.state) }
                    }
                }
                SettingsGroup(title: "Finding devices") {
                    SettingsRow(label: "On the local network", hint: "Turning it on applies at the next start") { sw(b.lanDiscovery) }
                    SettingsRow(label: "Over the Internet", hint: "Through the DHT, on a topic only members can work out") { sw(b.dhtDiscovery) }
                }
                SettingsGroup(title: "Relaying") {
                    SettingsRow(label: "For my own devices", hint: "When two of your devices cannot reach each other, this Mac passes their encrypted data on") {
                        sw(b.relayOwnDevices)
                    }
                    SettingsRow(label: "For everyone in shared folders", hint: "Also between other people’s devices in folders you share. Relays never see the files.") {
                        sw(b.relayEveryone)
                    }
                }
                SettingsGroup(title: "Speed") {
                    SettingsRow(label: "Internet download limit", hint: "Folder sync only, separate from torrents. Empty is no limit.") { RateField(bytes: b.downLimit) }
                    SettingsRow(label: "Internet upload limit", hint: "The local network is never limited") { RateField(bytes: b.upLimit) }
                }
                SettingsGroup(title: "Folders") {
                    SettingsRow(label: "Joined folders go to", hint: "Each joined folder gets its own folder here") {
                        OptionalPathField(path: Binding(get: { b.wrappedValue.defaultLocation.isEmpty ? nil : b.wrappedValue.defaultLocation },
                                                        set: { b.wrappedValue.defaultLocation = $0 ?? "" }),
                                          placeholder: "~/Seedwell")
                    }
                }
                SettingsGroup(title: "Finder") {
                    SettingsRow(label: "Sync status in Finder", hint: finderOn
                                ? "Badges on synced folders and their files. Switched on and off in System Settings."
                                : "Badges on synced folders and their files. macOS asks you to switch it on in System Settings.") {
                        if finderOn {
                            HStack(spacing: 10) {
                                Label("On", systemImage: "checkmark.circle.fill").foregroundStyle(.secondary)
                                Button("Manage…") { FIFinderSyncController.showExtensionManagementInterface() }
                                    .help("Open the list of extensions in System Settings")
                            }
                        } else {
                            Button("Turn On…") { FinderSetup.show() }.help("Show how to switch on Finder badges")
                                .accessibilityIdentifier("finder-turn-on")
                        }
                    }
                    SettingsRow(label: "Seedwell in the Share menu", hint: "Right-click any folder, Share ▸ Seedwell: share it, or start syncing it") {
                        if shareOn {
                            HStack(spacing: 10) {
                                Label("On", systemImage: "checkmark.circle.fill").foregroundStyle(.secondary)
                                Button("Manage…") { ShareMenu.openSettings() }.help("Open the Share menu's list of extensions in System Settings")
                            }
                        } else {
                            Button("Turn On…") { FinderSetup.show() }.help("Show how to add Seedwell to the Share menu")
                                .accessibilityIdentifier("share-menu-turn-on")
                        }
                    }
                }
            }
        }
        .task { await model.watchSync() }
        .onAppear { model.reloadSync(); finderOn = FinderStatusWriter.extensionEnabled; shareOn = ShareMenu.enabled }
        // Switched in System Settings: seen when the app comes back.
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            finderOn = FinderStatusWriter.extensionEnabled
            shareOn = ShareMenu.enabled
        }
    }
}

/// The device name: empty is the computer's name (shown as the placeholder). Saved on Return or when it loses focus.
struct DeviceNameField: View {
    @Binding var name: String
    let placeholder: String
    var identifier = "sync-device-name"
    @State private var text = ""
    @FocusState private var focused: Bool

    var body: some View {
        TextField(placeholder, text: $text)
            .textFieldStyle(.roundedBorder).frame(width: 230)
            .focused($focused)
            .onAppear { text = name }
            .onChange(of: name) { if !focused { text = name } }
            .onChange(of: focused) { if !focused { commit() } }
            .onSubmit(commit)
            .accessibilityIdentifier(identifier)
    }

    private func commit() {
        let t = String(text.trimmingCharacters(in: .whitespaces).prefix(64))
        text = t
        if t != name { name = t }
    }
}

extension SyncTab {
    /// Why a port was refused, that sync is moving, or what the port is.
    private var portHint: String {
        if let e = portError { return e }
        if let p = model.syncPortMovingTo { return "Starting folder sync on port \(p)…" }
        return "Picked at the first start and kept. Separate from the torrent port."
    }
}

/// The sync port: always a number (sync keeps the port it has). Committed on Return or when it loses focus; an
/// empty field goes back to the port in use.
struct SyncPortField: View {
    let port: UInt16
    let commit: (UInt16) -> Void
    let invalid: () -> Void
    @State private var text = ""
    @FocusState private var focused: Bool

    var body: some View {
        TextField("Port", text: $text)
            .textFieldStyle(.roundedBorder).frame(width: 90)
            .monospacedDigit()
            .focused($focused)
            .disabled(port == 0)
            .onAppear { text = PortEntry.text(port) }
            .onChange(of: port) { if !focused { text = PortEntry.text(port) } }
            .onChange(of: focused) { if !focused { done() } }
            .onSubmit { done() }
            .accessibilityIdentifier("sync-port")
    }

    private func done() {
        switch PortEntry.parse(text) {
        case .automatic: text = PortEntry.text(port)
        case .manual(let v): if v != port { commit(v) }
        case .invalid: invalid(); text = PortEntry.text(port)
        }
    }
}
