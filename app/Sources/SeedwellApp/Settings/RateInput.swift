import SwiftUI

/// The unit menu of a speed limit. Decimal, like Finder and the rate columns (1 MB/s = 1,000,000 B/s).
enum RateUnit: String, CaseIterable, Identifiable {
    case mb = "MB/s"
    case kb = "KB/s"
    var id: String { rawValue }
    var factor: Double { self == .mb ? 1_000_000 : 1_000 }
}

/// Parsing and writing speed limits. Limits are stored in bytes per second, 0 = unlimited.
enum RateText {
    /// Highest limit the core accepts (1 TiB/s).
    static let maxBytes: UInt64 = 1 << 40

    /// Bytes per second for what was typed in `unit`. Empty is unlimited (0); a comma or a dot is the
    /// decimal mark; `nil` when the text is not a non-negative number.
    static func parse(_ text: String, unit: RateUnit) -> UInt64? {
        let t = text.trimmingCharacters(in: .whitespaces).replacingOccurrences(of: ",", with: ".")
        if t.isEmpty { return 0 }
        guard t.allSatisfy({ $0.isNumber || $0 == "." }), t.filter({ $0 == "." }).count <= 1,
              let v = Double(t), v.isFinite else { return nil }
        let bytes = (v * unit.factor).rounded()
        return bytes >= Double(maxBytes) ? maxBytes : UInt64(bytes)
    }

    /// The number as typed in `unit`: "2.5", "500", "" for unlimited.
    static func number(_ bytes: UInt64, unit: RateUnit) -> String {
        guard bytes != 0 else { return "" }
        var s = String(format: "%.3f", Double(bytes) / unit.factor)
        while s.hasSuffix("0") { s.removeLast() }
        if s.hasSuffix(".") { s.removeLast() }
        return s
    }

    /// The unit a stored value is shown in first: MB/s from 1 MB/s up, KB/s below.
    static func unit(for bytes: UInt64) -> RateUnit { bytes >= 1_000_000 || bytes == 0 ? .mb : .kb }
}

/// A speed limit: a number, a unit menu (MB/s or KB/s) and, left empty, "Unlimited". Applies while
/// typing (after a short pause) and at once on Return or when the field loses focus.
struct RateField: View {
    /// Bytes per second; 0 = unlimited.
    @Binding var bytes: UInt64
    var width: CGFloat = 76
    @State private var text = ""
    @State private var unit: RateUnit = .mb
    @State private var pending: Task<Void, Never>?
    @FocusState private var focused: Bool

    var body: some View {
        HStack(spacing: 4) {
            TextField("Unlimited", text: $text)
                .textFieldStyle(.roundedBorder)
                .multilineTextAlignment(.trailing)
                .frame(width: width)
                .foregroundStyle(RateText.parse(text, unit: unit) == nil ? Color.red : Color.primary)
                .focused($focused)
                .onSubmit(commit)
                .help("Leave empty for no limit")
            Picker("", selection: $unit) {
                ForEach(RateUnit.allCases) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.menu).labelsHidden().fixedSize()
            .help("Unit of the limit")
        }
        .onAppear {
            unit = RateText.unit(for: bytes)
            text = RateText.number(bytes, unit: unit)
        }
        .onChange(of: text) { schedule() }
        .onChange(of: unit) { commit() }
        .onChange(of: focused) { if !focused { commit() } }
        .onChange(of: bytes) { if !focused { text = RateText.number(bytes, unit: unit) } }
        .onDisappear { pending?.cancel() }
    }

    private func schedule() {
        pending?.cancel()
        guard focused else { return }
        pending = Task {
            try? await Task.sleep(for: .milliseconds(600))
            if !Task.isCancelled { commit() }
        }
    }

    private func commit() {
        pending?.cancel()
        // Typing something that is not a number keeps the old limit; leaving the field puts it back.
        guard let v = RateText.parse(text, unit: unit) else {
            if !focused { text = RateText.number(bytes, unit: unit) }
            return
        }
        // The text is just the stored value written out (appearing, rounding): nothing was edited.
        if text.trimmingCharacters(in: .whitespaces) == RateText.number(bytes, unit: unit) { return }
        if v != bytes { bytes = v }
        if !focused { text = RateText.number(v, unit: unit) }
    }
}
