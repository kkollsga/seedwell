import AppKit
import SwiftUI

// Controls still drawn outside the settings model (a tracker's folders and Info page). The speed limit's field, its units
// and how typed text is read are the core's (`settingsRateControl`, `settingsFieldRead`).

/// A folder path that may be unset (`nil`), with "Choose…" and a button back to the default; the field's prompt is
/// shown for an unset value. Each change goes to `set`.
struct OptionalPathField: View {
    /// The core's field (`TrackerFolderRow`): its folder, prompt and helps.
    let row: TrackerFolderRow
    var width: CGFloat = 230
    let set: (String?) -> Void

    var body: some View {
        HStack(spacing: 8) {
            TextField(row.placeholder, text: Binding(get: { row.path ?? "" }, set: { set($0.isEmpty ? nil : $0) }))
                .textFieldStyle(.roundedBorder).frame(width: width)
                .help(row.help)
            Button(row.chooseTitle) { Task { if let p = await FolderPicker.choose(start: row.path ?? "") { set(p) } } }.fixedSize()
                .help(row.chooseHelp)
            if row.path != nil {
                Button { set(nil) } label: { Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary) }
                    .buttonStyle(.plain).help(row.clearHelp)
            }
        }
    }
}

@MainActor enum FolderPicker {
    /// Asks for a folder, opening at `start` (or the nearest folder of it that exists: the core looks at the disk off
    /// the main thread and gives up on one that does not answer).
    static func choose(start: String) async -> String? {
        let folder = start.isEmpty ? nil : await folderPickerStart(path: start)
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Choose"
        if let folder { panel.directoryURL = URL(fileURLWithPath: folder) }
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
