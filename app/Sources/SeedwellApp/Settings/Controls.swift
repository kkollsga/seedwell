import AppKit
import SwiftUI

// Controls still drawn outside the settings model (a tracker's folders and Info page). The speed limit's field, its units
// and how typed text is read are the core's (`settingsRateControl`, `settingsFieldRead`).

/// A folder path that may be unset (`nil`), with "Choose…" and a button back to the default. `placeholder` is shown
/// for an unset value.
struct OptionalPathField: View {
    @Binding var path: String?
    var placeholder = "Default"
    var width: CGFloat = 230

    var body: some View {
        HStack(spacing: 8) {
            TextField(placeholder, text: Binding(get: { path ?? "" }, set: { path = $0.isEmpty ? nil : $0 }))
                .textFieldStyle(.roundedBorder).frame(width: width)
                .help("A folder for this content type, or empty for the default")
            Button("Choose…") { if let p = FolderPicker.choose(start: path ?? "") { path = p } }.fixedSize()
                .help("Pick a folder")
            if path != nil {
                Button { path = nil } label: { Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary) }
                    .buttonStyle(.plain).help("Use the default")
            }
        }
    }
}

@MainActor enum FolderPicker {
    static func choose(start: String) -> String? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Choose"
        if !start.isEmpty {
            let expanded = (start as NSString).expandingTildeInPath
            if FileManager.default.fileExists(atPath: expanded) { panel.directoryURL = URL(fileURLWithPath: expanded) }
        }
        guard panel.runModal() == .OK, let url = panel.url else { return nil }
        return Notifier.abbreviate(url.path)
    }
}

/// The value a field of one of the core's controls sent: its text and unit.
private func typed(_ c: SettingsCommand) -> (SettingKey, String, String)? {
    guard case let .apply(.set(key, value)) = c else { return nil }
    switch value {
    case let .typed(text): return (key, text, "")
    case let .measure(text, unit): return (key, text, unit)
    default: return nil
    }
}

/// A speed limit: a number, its unit (MB/s or KB/s) and, left empty, "Unlimited". Applies while typing (after a short
/// pause) and at once on Return or when the field loses focus.
struct RateField: View {
    /// Bytes per second; 0 = unlimited.
    @Binding var bytes: UInt64
    var width: CGFloat = 76

    var body: some View {
        ModelControlView(control: settingsRateControl(bytes: bytes, width: UInt32(width))) { c in
            if case let (_, text, unit)? = typed(c), let v = settingsFieldRead(kind: .rate, text: text, unit: unit, current: bytes), v != bytes { bytes = v }
        }
    }
}
