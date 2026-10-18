import SwiftUI

/// The unit menu of the inactive-seed threshold. Decimal, like Finder (1 GB = 1,000,000,000 B).
enum SizeUnit: String, CaseIterable, Identifiable {
    case mb = "MB"
    case gb = "GB"
    case tb = "TB"
    var id: String { rawValue }
    var factor: Double { switch self { case .mb: 1e6; case .gb: 1e9; case .tb: 1e12 } }
}

/// Parsing and writing the "stop when less than X uploaded in N hours" condition (SPEC §7.5). The
/// threshold is stored in bytes and the window in hours; an empty or zero threshold means off.
enum IdleText {
    static let defaultHours: UInt32 = 150
    static let maxHours: UInt32 = 8760
    static let maxBytes: UInt64 = 1_000_000_000_000_000

    /// Bytes for what was typed in `unit`. Empty is off (0); a comma or a dot is the decimal mark;
    /// `nil` when the text is not a non-negative number.
    static func parse(_ text: String, unit: SizeUnit) -> UInt64? {
        let t = text.trimmingCharacters(in: .whitespaces).replacingOccurrences(of: ",", with: ".")
        if t.isEmpty { return 0 }
        guard t.allSatisfy({ $0.isNumber || $0 == "." }), t.filter({ $0 == "." }).count <= 1,
              let v = Double(t), v.isFinite else { return nil }
        let bytes = (v * unit.factor).rounded()
        return bytes >= Double(maxBytes) ? maxBytes : UInt64(bytes)
    }

    /// The number as typed in `unit`: "1", "0.5", "" for off.
    static func number(_ bytes: UInt64, unit: SizeUnit) -> String {
        guard bytes != 0 else { return "" }
        var s = String(format: "%.3f", Double(bytes) / unit.factor)
        while s.hasSuffix("0") { s.removeLast() }
        if s.hasSuffix(".") { s.removeLast() }
        return s
    }

    /// The unit a stored value is shown in first: TB from 1 TB, GB from 100 MB (and for off), else MB.
    static func unit(for bytes: UInt64) -> SizeUnit {
        if bytes >= 1_000_000_000_000 { return .tb }
        return bytes >= 100_000_000 || bytes == 0 ? .gb : .mb
    }

    /// Whole hours, 1 ... 8760; `nil` for anything else (empty included).
    static func parseHours(_ text: String) -> UInt32? {
        guard let h = UInt32(text.trimmingCharacters(in: .whitespaces)), (1...maxHours).contains(h) else { return nil }
        return h
    }

    /// "1 GB in 150 h" (a summary for lists).
    static func summary(bytes: UInt64, hours: UInt32) -> String {
        let u = unit(for: bytes)
        return "\(number(bytes, unit: u)) \(u.rawValue) in \(hours) h"
    }
}

/// "Stop seeding when less than [ 1 ] [GB] uploaded in [ 150 ] hours". Empty threshold = off. Hours
/// count seeding time only (paused or not running does not count).
struct IdleStopField: View {
    /// Threshold in bytes; 0 = off.
    @Binding var bytes: UInt64
    @Binding var hours: UInt32
    var prefix: String? = "Stop seeding when less than"
    @State private var text = ""
    @State private var hoursText = ""
    @State private var unit: SizeUnit = .gb
    @FocusState private var focusedSize: Bool
    @FocusState private var focusedHours: Bool

    var body: some View {
        HStack(spacing: 6) {
            if let prefix { Text(prefix) }
            TextField("Off", text: $text)
                .textFieldStyle(.roundedBorder).multilineTextAlignment(.trailing).frame(width: 62)
                .foregroundStyle(IdleText.parse(text, unit: unit) == nil ? Color.red : Color.primary)
                .focused($focusedSize)
                .onSubmit(commit)
                .help("Leave empty to keep seeding however little is uploaded")
            Picker("", selection: $unit) {
                ForEach(SizeUnit.allCases) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.menu).labelsHidden().fixedSize()
            Text("uploaded in")
            TextField("150", text: $hoursText)
                .textFieldStyle(.roundedBorder).multilineTextAlignment(.trailing).frame(width: 54)
                .foregroundStyle(IdleText.parseHours(hoursText) == nil ? Color.red : Color.primary)
                .focused($focusedHours)
                .onSubmit(commit)
                .help("Hours spent seeding. Time the torrent is paused or the app is closed does not count.")
            Text("hours")
        }
        .onAppear(perform: load)
        .onChange(of: unit) { commit() }
        .onChange(of: focusedSize) { if !focusedSize { commit() } }
        .onChange(of: focusedHours) { if !focusedHours { commit() } }
        .onChange(of: bytes) { if !focusedSize { load() } }
        .onChange(of: hours) { if !focusedHours { hoursText = String(hours) } }
    }

    private func load() {
        unit = IdleText.unit(for: bytes)
        text = IdleText.number(bytes, unit: unit)
        hoursText = String(hours)
    }

    private func commit() {
        if let v = IdleText.parse(text, unit: unit),
           text.trimmingCharacters(in: .whitespaces) != IdleText.number(bytes, unit: unit) || v != bytes {
            if v != bytes { bytes = v }
        }
        if let h = IdleText.parseHours(hoursText), h != hours { hours = h }
        if !focusedSize { text = IdleText.number(bytes, unit: unit) }
        if !focusedHours { hoursText = String(hours) }
    }
}
