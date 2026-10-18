import AppKit
import SwiftUI

/// `HH:MM`, committed on return or when focus leaves.
struct TimeField: View {
    @Binding var minute: UInt32
    @State private var text = ""
    @FocusState private var focused: Bool

    var body: some View {
        TextField("", text: $text)
            .textFieldStyle(.roundedBorder).frame(width: 58)
            .focused($focused)
            .onAppear { text = Self.format(minute) }
            .onChange(of: minute) { if !focused { text = Self.format(minute) } }
            .onSubmit(commit)
            .onChange(of: focused) { if !focused { commit() } }
    }

    private func commit() {
        let parts = text.split(separator: ":").compactMap { UInt32($0) }
        if parts.count == 2, parts[0] < 24, parts[1] < 60 { minute = parts[0] * 60 + parts[1] }
        text = Self.format(minute)
    }

    static func format(_ m: UInt32) -> String { String(format: "%02d:%02d", m / 60, m % 60) }
}

/// A folder path with a "Choose…" button. `placeholder` is shown for an unset (nil) value.
struct PathField: View {
    @Binding var path: String
    var width: CGFloat = 230

    var body: some View {
        HStack(spacing: 8) {
            TextField("", text: $path).textFieldStyle(.roundedBorder).frame(width: width)
                .help("The folder's path")
            Button("Choose…") { if let p = FolderPicker.choose(start: path) { path = p } }.fixedSize()
                .help("Pick a folder")
        }
    }
}

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

// MARK: Seeding goal

/// The choices of a seeding goal (SPEC §7.2), shared by the Default rule and the rule editor.
enum GoalTag: String, CaseIterable, Identifiable {
    case ratio = "Ratio", time = "Seed time", either = "Ratio or time", both = "Ratio and time"
    case forever = "Seed forever", stop = "Stop when finished"
    var id: String { rawValue }
}

extension GoalKind {
    var tag: GoalTag {
        switch self {
        case .ratio: .ratio
        case .time: .time
        case .ratioOrTime: .either
        case .ratioAndTime: .both
        case .forever: .forever
        case .stopWhenFinished: .stop
        }
    }
    var ratioValue: Double {
        switch self {
        case .ratio(let r), .ratioOrTime(let r, _), .ratioAndTime(let r, _): r
        default: 1.0
        }
    }
    var secsValue: UInt64 {
        switch self {
        case .time(let s), .ratioOrTime(_, let s), .ratioAndTime(_, let s): s
        default: 7 * 86_400
        }
    }
    func with(tag: GoalTag) -> GoalKind {
        let (r, s) = (ratioValue, secsValue)
        switch tag {
        case .ratio: return .ratio(ratio: r)
        case .time: return .time(secs: s)
        case .either: return .ratioOrTime(ratio: r, secs: s)
        case .both: return .ratioAndTime(ratio: r, secs: s)
        case .forever: return .forever
        case .stop: return .stopWhenFinished
        }
    }
    func with(ratio: Double) -> GoalKind {
        switch self {
        case .ratio: .ratio(ratio: ratio)
        case .ratioOrTime(_, let s): .ratioOrTime(ratio: ratio, secs: s)
        case .ratioAndTime(_, let s): .ratioAndTime(ratio: ratio, secs: s)
        default: self
        }
    }
    func with(secs: UInt64) -> GoalKind {
        switch self {
        case .time: .time(secs: secs)
        case .ratioOrTime(let r, _): .ratioOrTime(ratio: r, secs: secs)
        case .ratioAndTime(let r, _): .ratioAndTime(ratio: r, secs: secs)
        default: self
        }
    }

    /// "ratio 1.0 or 7 days": the summary line of a rule.
    var summary: String {
        func d(_ s: UInt64) -> String { s % 86_400 == 0 ? "\(s / 86_400) days" : "\(max(1, s / 3600)) h" }
        switch self {
        case .ratio(let r): return "ratio \(Fmt.goal(r))"
        case .time(let s): return d(s)
        case .ratioOrTime(let r, let s): return "ratio \(Fmt.goal(r)) or \(d(s))"
        case .ratioAndTime(let r, let s): return "ratio \(Fmt.goal(r)) and \(d(s))"
        case .forever: return "seed forever"
        case .stopWhenFinished: return "stop when done"
        }
    }
}

struct GoalEditor: View {
    @Binding var goal: GoalKind

    var body: some View {
        ViewThatFits(in: .horizontal) {
            HStack(spacing: 10) { picker; fields }
            VStack(alignment: .leading, spacing: 8) { picker; HStack(spacing: 10) { fields } }
        }
    }

    private var picker: some View {
        Picker("", selection: Binding(get: { goal.tag }, set: { goal = goal.with(tag: $0) })) {
            ForEach(GoalTag.allCases) { Text($0.rawValue).tag($0) }
        }
        .labelsHidden().fixedSize()
    }

    @ViewBuilder private var fields: some View {
        if [.ratio, .either, .both].contains(goal.tag) {
            Text("Ratio").foregroundStyle(.secondary)
            TextField("", value: Binding(get: { goal.ratioValue }, set: { goal = goal.with(ratio: max(0.1, $0)) }),
                      format: .number.locale(enUS).precision(.fractionLength(1)))
                .textFieldStyle(.roundedBorder).frame(width: 58)
        }
        if [.time, .either, .both].contains(goal.tag) {
            Text("Time").foregroundStyle(.secondary)
            DurationField(secs: Binding(get: { goal.secsValue }, set: { goal = goal.with(secs: $0) }))
        }
    }
}

/// A number with a unit menu: "72 hours", "7 days".
struct DurationField: View {
    @Binding var secs: UInt64
    @State private var days = true

    var body: some View {
        HStack(spacing: 6) {
            TextField("", value: Binding(get: { Double(secs) / unit }, set: { secs = UInt64(max(1, $0) * unit) }),
                      format: .number.locale(enUS).precision(.fractionLength(0...1)))
                .textFieldStyle(.roundedBorder).frame(width: 58)
            Picker("", selection: $days) {
                Text("hours").tag(false)
                Text("days").tag(true)
            }
            .labelsHidden().fixedSize()
        }
        .onAppear { days = secs % 86_400 == 0 && secs >= 86_400 }
    }

    private var unit: Double { days ? 86_400 : 3_600 }
}
