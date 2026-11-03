import SwiftUI

/// Settings building blocks that give every tab the System Settings look of the concepts.
struct SettingsBox<Content: View>: View {
    let title: String
    /// Two groups side by side: the boxes get the same height.
    var fillHeight = false
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(title.uppercased())
                .font(Theme.caption.weight(.semibold)).tracking(0.4)
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

struct SettingsLine<Control: View>: View {
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
                            Text(hint).font(Theme.caption).foregroundStyle(.secondary)
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
                            .font(Theme.caption).foregroundStyle(.secondary)
                            .multilineTextAlignment(.trailing).frame(maxWidth: 260, alignment: .trailing)
                            .layoutPriority(-1)
                    }
                }
            }
        }
        .font(Theme.body)
        .padding(.horizontal, 14)
        .padding(.vertical, 9)
        .frame(minHeight: 40)
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
        Text(text).font(Theme.mono).lineLimit(1).fixedSize()
            .padding(.horizontal, 8).padding(.vertical, 3)
            .background(RoundedRectangle(cornerRadius: 6).stroke(.primary.opacity(0.18), lineWidth: 0.5))
    }
}
