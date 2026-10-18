import SwiftUI

/// Settings > General > Updates. The same rows for either back end (see `UpdateCenter`).
struct UpdatesGroup: View {
    @Bindable var model: SettingsModel
    @Bindable private var center = UpdateCenter.shared

    var body: some View {
        SettingsGroup(title: "Updates") {
            SettingsRow(label: "Check automatically", hint: center.releaseBuild || center.backend == .sparkle ? "Once a day" : "Development build: checks only when you ask") {
                sw(checkBinding).disabled(!center.switchesActive)
            }
            SettingsRow(label: "Install automatically", hint: "When nothing is downloading") {
                sw(installBinding).disabled(!center.switchesActive || !checkBinding.wrappedValue)
            }
            HStack(spacing: 8) {
                statusView
                Spacer(minLength: 12)
                Button("Check Now") { center.checkNow() }
                    .disabled(!center.canCheck)
                    .help("Look for a newer version of Seedwell now")
                if let v = center.availableVersion {
                    Button("Update to \(v)") { center.install() }
                        .disabled(!center.canCheck)
                        .help("Download, check and install Seedwell \(v), then reopen it")
                }
            }
        }
    }

    private var checkBinding: Binding<Bool> {
        center.backend == .sparkle
            ? Binding(get: { center.checkAutomatically }, set: { center.checkAutomatically = $0 })
            : $model.app.updates.check
    }

    private var installBinding: Binding<Bool> {
        center.backend == .sparkle
            ? Binding(get: { center.installAutomatically }, set: { center.installAutomatically = $0 })
            : $model.app.updates.autoInstall
    }

    @ViewBuilder private var statusView: some View {
        let v = center.currentVersion
        switch center.status {
        case .idle:
            Text(center.backend == .sparkle ? "Seedwell \(v)" : "Seedwell \(v) · not checked yet").foregroundStyle(.secondary)
        case .checking:
            Text("Checking…").foregroundStyle(.secondary)
        case .upToDate(let at):
            Text(at.map { "Seedwell \(v) is up to date · checked \(Self.ago($0))" } ?? "Seedwell \(v) is up to date").foregroundStyle(.secondary)
        case .available(let ver, let notes):
            HStack(spacing: 8) {
                Text("Seedwell \(ver) is available")
                if let notes { Link("Release Notes", destination: notes) }
            }
        case .noReleases:
            Text("No releases yet").foregroundStyle(.secondary)
        case .checkFailed(let why):
            Text("Couldn't check: \(why)").foregroundStyle(.secondary)
        case .installing(let ver):
            Text("Installing \(ver)…").foregroundStyle(.secondary)
        case .installFailed(let ver):
            HStack(spacing: 8) {
                Text("Update to \(ver) failed")
                Text("·").foregroundStyle(.secondary)
                Button("Show Log") { center.revealLog() }.buttonStyle(.link)
            }
        case .updated(let ver):
            Text("Updated to \(ver)").foregroundStyle(.secondary)
        }
    }

    static func ago(_ date: Date) -> String {
        let f = RelativeDateTimeFormatter()
        f.locale = enUS
        f.unitsStyle = .full
        return f.localizedString(for: date, relativeTo: Date())
    }
}
