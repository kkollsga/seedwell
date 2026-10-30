import SwiftUI

/// Settings > General > Updates. The same rows for either back end (see `UpdateCenter`): with the GitHub back end
/// every text and state is the core's `UpdatesModel`; Sparkle keeps its own switches and status.
struct UpdatesGroup: View {
    @Bindable var model: SettingsModel
    @Bindable private var center = UpdateCenter.shared

    var body: some View {
        if center.backend == .github, let m = center.model {
            SettingsGroup(title: "Updates") {
                SettingsRow(label: m.checkLabel, hint: m.checkHint) {
                    sw($model.app.updates.check).disabled(!m.switchesEnabled)
                }
                SettingsRow(label: m.installLabel, hint: m.installHint) {
                    sw($model.app.updates.autoInstall).disabled(!m.switchesEnabled || !model.app.updates.check)
                }
                SettingsRow(label: m.statusLabel, hint: m.status) {
                    HStack(spacing: 8) {
                        Button(m.checkNow.title) { center.checkNow() }.disabled(!m.checkNow.enabled).help(m.checkNow.help)
                        if let b = m.install {
                            Button(b.title) { center.install() }.disabled(!b.enabled).help(b.help)
                        }
                        if let url = m.notesUrl.flatMap(URL.init(string:)) { Link(m.notesTitle, destination: url) }
                        if let b = m.showLog { Button(b.title) { center.revealLog() }.buttonStyle(.link).help(b.help) }
                    }
                }
                SettingsRow(label: m.devLabel, hint: m.devHint) {
                    sw($model.app.updates.devBuilds)
                }
                if let b = m.installBuild {
                    SettingsRow(label: "Running \(m.version)", hint: m.watching.map { "Watching \($0)" }) {
                        HStack(spacing: 8) {
                            Button(b.title) { center.chooseBuild() }.disabled(!b.enabled).help(b.help)
                            if let s = m.stopWatching { Button(s.title) { center.stopWatching() }.help(s.help) }
                        }
                    }
                }
            }
        } else if center.backend == .sparkle {
            SettingsGroup(title: "Updates") {
                SettingsRow(label: "Check automatically", hint: "Once a day") {
                    sw(Binding(get: { center.checkAutomatically }, set: { center.checkAutomatically = $0 }))
                }
                SettingsRow(label: "Install automatically", hint: "When nothing is downloading") {
                    sw(Binding(get: { center.installAutomatically }, set: { center.installAutomatically = $0 }))
                        .disabled(!center.checkAutomatically)
                }
                SettingsRow(label: "Check for updates", hint: "Seedwell \(center.currentVersion)") {
                    Button("Check Now") { center.checkNow() }.disabled(!center.canCheck).help("Look for a newer version of Seedwell now")
                }
            }
        }
    }
}
