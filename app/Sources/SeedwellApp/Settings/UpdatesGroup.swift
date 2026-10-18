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
            SettingsRow(label: "Check for updates", hint: statusText) {
                HStack(spacing: 8) {
                    Button("Check Now") { center.checkNow() }
                        .disabled(!center.canCheck)
                        .help("Look for a newer version of Seedwell now")
                    if let v = center.availableVersion {
                        Button("Update to \(v)") { center.install() }
                            .disabled(!center.canCheck)
                            .help("Download, check and install Seedwell \(v), then reopen it")
                    }
                    if case .available(_, let notes?) = center.status { Link("Release Notes", destination: notes) }
                    if case .installFailed = center.status { Button("Show Log") { center.revealLog() }.buttonStyle(.link) }
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

    /// The status line beside the buttons, in the row's secondary text.
    private var statusText: String {
        let v = center.currentVersion
        switch center.status {
        case .idle: return center.backend == .sparkle ? "Seedwell \(v)" : "Seedwell \(v) · not checked yet"
        case .checking: return "Checking…"
        case .upToDate(let at): return at.map { "Seedwell \(v) is up to date · checked \(Self.ago($0))" } ?? "Seedwell \(v) is up to date"
        case .available(let ver, _): return "Seedwell \(ver) is available"
        case .noReleases: return "No releases yet"
        case .checkFailed(let why): return "Couldn't check: \(why)"
        case .installing(let ver): return "Installing \(ver)…"
        case .installFailed(let ver): return "Update to \(ver) failed"
        case .updated(let ver): return "Updated to \(ver)"
        }
    }

    static func ago(_ date: Date) -> String {
        let f = RelativeDateTimeFormatter()
        f.locale = enUS
        f.unitsStyle = .full
        return f.localizedString(for: date, relativeTo: Date())
    }
}
