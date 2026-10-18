import SwiftUI

/// Settings building blocks that give every tab the System Settings look of the concepts.
struct SettingsGroup<Content: View>: View {
    let title: String
    /// Two groups side by side: the boxes get the same height.
    var fillHeight = false
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(title.uppercased())
                .font(.system(size: 11, weight: .semibold)).tracking(0.4)
                .foregroundStyle(.secondary)
                .padding(.leading, 4)
            VStack(spacing: 0) {
                Group(subviews: content) { subviews in
                    ForEach(subviews.indices, id: \.self) { i in
                        if i > 0 { Divider().padding(.leading, 14) }
                        subviews[i]
                    }
                }
            }
            .frame(maxHeight: fillHeight ? .infinity : nil, alignment: .top)
            .background(RoundedRectangle(cornerRadius: 12).fill(.primary.opacity(0.055)))
            .overlay(RoundedRectangle(cornerRadius: 12).stroke(.primary.opacity(0.05), lineWidth: 0.5))
        }
    }
}

/// Narrow editors (rules, trackers) put a row's hint under its control instead of beside it.
private struct CompactRowsKey: EnvironmentKey { static let defaultValue = false }

extension EnvironmentValues {
    var compactRows: Bool {
        get { self[CompactRowsKey.self] }
        set { self[CompactRowsKey.self] = newValue }
    }
}

struct SettingsRow<Control: View>: View {
    let label: String
    var hint: String?
    @ViewBuilder var control: Control
    @Environment(\.compactRows) private var compact

    var body: some View {
        Group {
            if compact {
                HStack(alignment: .firstTextBaseline, spacing: 12) {
                    Text(label).frame(width: 118, alignment: .leading).lineLimit(2)
                    VStack(alignment: .leading, spacing: 4) {
                        control
                        if let hint {
                            Text(hint).font(.system(size: 11.5)).foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                    }
                    Spacer(minLength: 0)
                }
            } else {
                HStack(spacing: 12) {
                    Text(label).frame(width: 155, alignment: .leading).lineLimit(2)
                    control
                    Spacer(minLength: 12)
                    if let hint {
                        Text(hint)
                            .font(.system(size: 11.5)).foregroundStyle(.secondary)
                            .multilineTextAlignment(.trailing).frame(maxWidth: 260, alignment: .trailing)
                            .layoutPriority(-1)
                    }
                }
            }
        }
        .font(.system(size: 13))
        .padding(.horizontal, 14)
        .padding(.vertical, 9)
        .frame(minHeight: 40)
    }
}

/// A full-width row: title on the left, switch on the right.
struct ToggleRow: View {
    let title: String
    @Binding var isOn: Bool

    var body: some View {
        HStack {
            Text(title)
            Spacer(minLength: 12)
            Toggle("", isOn: $isOn).labelsHidden().toggleStyle(.switch)
                .help(isOn ? "On: \(title)" : "Off: \(title)")
        }
        .font(.system(size: 13))
        .padding(.horizontal, 14).padding(.vertical, 9)
        .frame(minHeight: 40)
    }
}

/// Segmented choice for small enums (Weekdays / Every day / Custom, Read-only / Control / Full).
struct Segmented<T: Hashable & CaseIterable & RawRepresentable>: View where T.RawValue == String, T.AllCases: RandomAccessCollection {
    @Binding var value: T

    var body: some View {
        Picker("", selection: $value) {
            ForEach(Array(T.allCases), id: \.self) { Text($0.rawValue).tag($0) }
        }
        .pickerStyle(.segmented).labelsHidden().fixedSize()
    }
}

struct SettingsPage<Content: View>: View {
    @ViewBuilder var content: Content

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 22) { content }
                .padding(.horizontal, 28).padding(.vertical, 22)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

/// Mono pill used for paths.
struct CodePill: View {
    let text: String
    var body: some View {
        Text(text).font(.system(size: 12.5, design: .monospaced)).lineLimit(1).fixedSize()
            .padding(.horizontal, 8).padding(.vertical, 3)
            .background(RoundedRectangle(cornerRadius: 6).stroke(.primary.opacity(0.18), lineWidth: 0.5))
    }
}
