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
                SettingsLineLayout {
                    Text(label).frame(width: 155, alignment: .leading).lineLimit(2)
                    control
                    if let hint {
                        Text(hint)
                            .font(Theme.caption).foregroundStyle(.secondary)
                            .multilineTextAlignment(.trailing).frame(maxWidth: 260, alignment: .trailing)
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

/// A Settings row: the label, the controls after it, and the hint at the right edge, at most 260 points wide. When the
/// row is too narrow to leave the hint `hintMin` (the main window at its 820-point minimum), the controls take the
/// width after the label (wrapping, `WrappingLine`) and the hint goes under them, still at the right edge: side by side,
/// it was squeezed to a letter per line. Subviews: the label, the controls, and the hint if there is one.
struct SettingsLineLayout: Layout {
    var spacing: CGFloat = 12
    /// Between the controls and the hint beside them, and under them.
    var hintGap: CGFloat = 24
    var hintMin: CGFloat = 180
    var hintMax: CGFloat = 260
    var hintUnder: CGFloat = 4

    private struct Plan {
        var label: CGSize
        var controls: CGSize
        var hint: CGSize
        /// The hint under the controls.
        var under: Bool
        /// The controls (and the hint) under the label: a control that neither wraps nor narrows (a wide segmented
        /// control) is wider than the room beside the label.
        var belowLabel = false
        var size: CGSize
    }

    private func plan(_ width: CGFloat?, _ subviews: Subviews) -> Plan {
        let label = subviews[0].sizeThatFits(.unspecified)
        let ideal = subviews[1].sizeThatFits(.unspecified)
        // Measured at the column's width only when their own width does not fit beside the label (rarely: the
        // usual row costs nothing more).
        if let width, ideal.width > width - label.width - spacing + 0.5,
           subviews[1].sizeThatFits(ProposedViewSize(width: max(0, width - label.width - spacing), height: nil)).width
            > width - label.width - spacing + 0.5 {
            let controls = subviews[1].sizeThatFits(ProposedViewSize(width: width, height: nil))
            let hint = subviews.count > 2 ? subviews[2].sizeThatFits(ProposedViewSize(width: min(width, hintMax), height: nil)) : .zero
            let height = label.height + hintUnder * 2 + controls.height + (subviews.count > 2 ? hintUnder + hint.height : 0)
            return Plan(label: label, controls: controls, hint: hint, under: true, belowLabel: true, size: CGSize(width: width, height: height))
        }
        guard subviews.count > 2 else {
            let controls = width.map { subviews[1].sizeThatFits(ProposedViewSize(width: max(0, $0 - label.width - spacing), height: nil)) } ?? ideal
            let w = label.width + spacing + controls.width
            return Plan(label: label, controls: controls, hint: .zero, under: false,
                        size: CGSize(width: max(width ?? w, w), height: max(label.height, controls.height)))
        }
        let beside = label.width + spacing + ideal.width + hintGap
        if let width, width - beside < hintMin {
            let column = max(0, width - label.width - spacing)
            let controls = subviews[1].sizeThatFits(ProposedViewSize(width: column, height: nil))
            let hint = subviews[2].sizeThatFits(ProposedViewSize(width: min(column, hintMax), height: nil))
            return Plan(label: label, controls: controls, hint: hint, under: true,
                        size: CGSize(width: width, height: max(label.height, controls.height + hintUnder + hint.height)))
        }
        let room = width.map { min(hintMax, $0 - beside) } ?? hintMax
        let hint = subviews[2].sizeThatFits(ProposedViewSize(width: room, height: nil))
        return Plan(label: label, controls: ideal, hint: hint, under: false,
                    size: CGSize(width: width ?? beside + hint.width, height: max(label.height, ideal.height, hint.height)))
    }

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        plan(proposal.width, subviews).size
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        let p = plan(bounds.width, subviews)
        if p.belowLabel {
            subviews[0].place(at: bounds.origin, anchor: .topLeading, proposal: ProposedViewSize(p.label))
            let y = bounds.minY + p.label.height + hintUnder * 2
            subviews[1].place(at: CGPoint(x: bounds.minX, y: y), anchor: .topLeading, proposal: ProposedViewSize(p.controls))
            if subviews.count > 2 {
                subviews[2].place(at: CGPoint(x: bounds.maxX, y: y + p.controls.height + hintUnder), anchor: .topTrailing,
                                  proposal: ProposedViewSize(p.hint))
            }
            return
        }
        let midY = bounds.midY
        subviews[0].place(at: CGPoint(x: bounds.minX, y: midY), anchor: .leading, proposal: ProposedViewSize(p.label))
        let x = bounds.minX + p.label.width + spacing
        guard subviews.count > 2 else {
            subviews[1].place(at: CGPoint(x: x, y: midY), anchor: .leading, proposal: ProposedViewSize(p.controls))
            return
        }
        if p.under {
            let top = midY - (p.controls.height + hintUnder + p.hint.height) / 2
            subviews[1].place(at: CGPoint(x: x, y: top), anchor: .topLeading, proposal: ProposedViewSize(p.controls))
            subviews[2].place(at: CGPoint(x: bounds.maxX, y: top + p.controls.height + hintUnder), anchor: .topTrailing,
                              proposal: ProposedViewSize(p.hint))
        } else {
            subviews[1].place(at: CGPoint(x: x, y: midY), anchor: .leading, proposal: ProposedViewSize(p.controls))
            subviews[2].place(at: CGPoint(x: bounds.maxX, y: midY), anchor: .trailing, proposal: ProposedViewSize(p.hint))
        }
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
