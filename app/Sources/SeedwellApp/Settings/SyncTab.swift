import FinderSync
import SwiftUI

/// Settings › Folder Sync (SPEC 7G.5): this Mac's name, the sync port, how devices are found and reached, Internet
/// limits and where joined folders go. These are folder sync's own (`sync.toml`), separate from the torrent settings.
struct SyncTab: View {
    @Bindable var model: SettingsModel
    @State private var finderOn = FinderStatusWriter.extensionEnabled

    var body: some View {
        SettingsPage {
            if let s = model.sync {
                let b = Binding(get: { model.sync ?? s }, set: { model.sync = $0 })
                SettingsGroup(title: "This Mac") {
                    SettingsRow(label: "Device name", hint: "What your other devices and the people you share with see") {
                        DeviceNameField(name: b.deviceName, placeholder: s.defaultDeviceName)
                    }
                    SettingsRow(label: "Sync port", hint: "Picked at the first start and kept. Separate from the torrent port.") {
                        Text(s.port == 0 ? "Not listening" : String(s.port)).monospacedDigit().foregroundStyle(.secondary)
                            .textSelection(.enabled)
                            .accessibilityIdentifier("sync-port")
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
                }
            }
        }
        .onAppear { model.reloadSync(); finderOn = FinderStatusWriter.extensionEnabled }
        // Switched in System Settings: seen when the app comes back.
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            finderOn = FinderStatusWriter.extensionEnabled
        }
    }
}

/// The device name: empty is the computer's name (shown as the placeholder). Saved on Return or when it loses focus.
struct DeviceNameField: View {
    @Binding var name: String
    let placeholder: String
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
            .accessibilityIdentifier("sync-device-name")
    }

    private func commit() {
        let t = String(text.trimmingCharacters(in: .whitespaces).prefix(64))
        text = t
        if t != name { name = t }
    }
}
