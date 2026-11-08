import SwiftUI

// The generic Settings renderer: draws the core's settings model (crates/ffi/src/settings_view.rs) block by block, row
// by row, control by control. Every word, every row and what a change does are the core's; this draws them, keeps what
// is being typed into a field until it is committed, and hands the commands back (`act`).

/// What a control asks for: run by `SettingsModel.run`.
typealias SettingsAct = (SettingsCommand) -> Void

extension SettingsTab: CaseIterable, Identifiable {
    public static var allCases: [SettingsTab] { [.general, .transfers, .folders, .rules, .network, .folderSync, .devices, .agents, .advanced] }
    public var id: String { rawValue }

    /// The name QA and `--show-settings` use, and the snapshots' file names.
    var rawValue: String {
        switch self {
        case .general: "general"
        case .transfers: "transfers"
        case .folders: "folders"
        case .rules: "rules"
        case .network: "network"
        case .folderSync: "folderSync"
        case .devices: "devices"
        case .agents: "agents"
        case .advanced: "advanced"
        }
    }

    init?(rawValue: String) {
        guard let t = Self.allCases.first(where: { $0.rawValue == rawValue }) else { return nil }
        self = t
    }

    var symbol: String {
        switch self {
        case .general: "gearshape"
        case .transfers: "arrow.up.arrow.down"
        case .folders: "folder"
        case .rules: "list.bullet"
        case .network: "network"
        case .folderSync: "arrow.triangle.2.circlepath"
        case .devices: "laptopcomputer.and.iphone"
        case .agents: "sparkle"
        case .advanced: "command"
        }
    }
}

/// The blocks of a tab, top to bottom.
struct SettingsBlocksView: View {
    let blocks: [SettingsBlock]
    let model: SettingsModel
    let act: SettingsAct

    var body: some View {
        ForEach(Array(blocks.enumerated()), id: \.offset) { _, block in
            switch block {
            case .single(let group):
                ModelGroupView(group: group, act: act)
            case .pair(let left, let right):
                HStack(alignment: .top, spacing: 18) {
                    ModelGroupView(group: left, fillHeight: true, act: act)
                    ModelGroupView(group: right, fillHeight: true, act: act)
                }
            case .rendezvous:
                if let r = model.syncRendezvous {
                    RendezvousSection(view: r) { await model.rendezvous($0) }
                }
            case .torrentProxy(let title):
                if let proxy = model.proxy { ProxySettingsBox(title: title, center: proxy) }
            case .devices, .rulesEditor:
                EmptyView()
            }
        }
    }
}

/// A titled box of rows, with + and − under it for a list.
struct ModelGroupView: View {
    let group: SettingsGroup
    var fillHeight = false
    let act: SettingsAct

    var body: some View {
        if let b = group.listButtons {
            VStack(alignment: .leading, spacing: 8) {
                box
                ControlGroup {
                    Button { act(b.add.command) } label: { Image(systemName: "plus") }.help(b.add.help)
                        .disabled(!b.add.enabled)
                    Button { act(b.remove.command) } label: { Image(systemName: "minus") }.help(b.remove.help)
                        .disabled(!b.remove.enabled)
                }
                .fixedSize()
            }
        } else {
            box
        }
    }

    private var box: some View {
        SettingsBox(title: group.title, fillHeight: fillHeight) {
            ForEach(group.rows, id: \.id) { row in ModelRowView(row: row, act: act) }
        }
    }
}

/// One row, in its style.
struct ModelRowView: View {
    let row: SettingsRow
    let act: SettingsAct

    var body: some View {
        switch row.style {
        case .standard:
            SettingsLine(label: row.label, hint: row.hint) { ModelControls(controls: row.controls, act: act) }
                .modifier(Quiet(on: row.quiet))
        case .described:
            HStack(alignment: .center, spacing: 12) {
                VStack(alignment: .leading, spacing: 3) {
                    Text(row.label).font(Theme.body)
                    if let hint = row.hint {
                        Text(hint).font(Theme.caption).foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                Spacer(minLength: 12)
                HStack(spacing: 8) { ModelControls(controls: row.controls, act: act) }.fixedSize()
                    .accessibilityLabel(row.label)
            }
            .font(Theme.body)
            .padding(.horizontal, 14).padding(.vertical, 9)
            .frame(minHeight: 40)
        case .wide:
            HStack {
                if row.label.isEmpty {
                    // Controls across the row (an address and its button, Add Address and its hint, a lone button).
                    ModelControls(controls: row.controls, act: act)
                    Spacer(minLength: 0)
                    if let hint = row.hint { Text(hint).font(Theme.caption).foregroundStyle(.secondary) }
                } else {
                    Text(row.label)
                    Spacer(minLength: 12)
                    ModelControls(controls: row.controls, act: act)
                }
            }
            .font(Theme.body)
            .padding(.horizontal, 14).padding(.vertical, 9)
            .frame(minHeight: 40)
            // A quiet switch row is one that is not available yet: faded whole.
            .opacity(row.quiet ? 0.5 : 1)
        case .note:
            Text(row.label)
                .font(Theme.secondary).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 14).padding(.vertical, 9)
        case .empty:
            Text(row.label).font(Theme.secondary).foregroundStyle(.secondary).padding(14)
        case .warning:
            DifferentDisksWarning(text: row.label)
        case let .item(icon, detail, selected, select, menus):
            ModelItemRow(row: row, icon: icon, detail: detail, selected: selected, select: select, menus: menus, act: act)
        case let .activity(time, client, outcome):
            HStack(alignment: .firstTextBaseline, spacing: 16) {
                Text(time).foregroundStyle(.secondary).monospacedDigit().frame(width: 60, alignment: .leading)
                (Text(client).bold() + Text(" ") + Text(row.label) + (outcome.map { Text(" \($0)").foregroundStyle(.red) } ?? Text("")))
                    .lineLimit(2)
                Spacer()
            }
            .font(Theme.body)
            .padding(.horizontal, 14).padding(.vertical, 10)
        case let .disclosure(icon, expanded, toggle, toggleHelp, status, statusAccent, warning, children):
            VStack(spacing: 0) {
                HStack(spacing: 10) {
                    Button { act(toggle) } label: {
                        Image(systemName: "chevron.right").font(Theme.micro.weight(.semibold))
                            .rotationEffect(.degrees(expanded ? 90 : 0))
                            .foregroundStyle(.secondary).frame(width: 14)
                    }
                    .buttonStyle(.plain)
                    .help(toggleHelp)
                    Image(systemName: icon.symbol).frame(width: 22).foregroundStyle(.secondary)
                    Text(row.label).fontWeight(.semibold).frame(width: 82, alignment: .leading)
                    Spacer(minLength: 8)
                    if statusAccent {
                        Text(status).font(Theme.caption).foregroundStyle(Color.accentColor)
                    } else {
                        Text(status).font(Theme.secondary).foregroundStyle(.secondary)
                    }
                    if let warning {
                        Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(Color.orange).help(warning)
                    }
                }
                .font(Theme.body)
                .padding(.horizontal, 14).padding(.vertical, 8)
                if expanded {
                    VStack(spacing: 0) {
                        ForEach(children, id: \.id) { ModelRowView(row: $0, act: act) }
                    }
                    .padding(.leading, 28)
                    .background(Color.primary.opacity(0.03))
                }
            }
        }
    }
}

private struct Quiet: ViewModifier {
    let on: Bool
    func body(content: Content) -> some View {
        if on { content.foregroundStyle(.secondary) } else { content }
    }
}

extension SettingsIcon {
    var symbol: String {
        switch self {
        case .content(let c): c.symbol
        case .watchFolder: "tray.and.arrow.down.fill"
        }
    }
}

/// An item of a list (a watch folder): its image, the name over its line, the controls; click selects, right-click
/// shows its menus.
private struct ModelItemRow: View {
    let row: SettingsRow
    let icon: SettingsIcon
    let detail: String
    let selected: Bool
    let select: SettingsCommand?
    let menus: [SettingsMenu]
    let act: SettingsAct

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: icon.symbol)
                .font(.system(size: Theme.Icon.large)).foregroundStyle(Color.accentColor).frame(width: 30)
            VStack(alignment: .leading, spacing: 1) {
                Text(row.label).font(Theme.headline).lineLimit(1).truncationMode(.middle)
                Text(detail).font(Theme.secondary).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer()
            ModelControls(controls: row.controls, spacing: 12, act: act)
        }
        .padding(.horizontal, 14).padding(.vertical, 9)
        .background(selected ? Color.accentColor.opacity(0.14) : .clear)
        .contentShape(Rectangle())
        .onTapGesture { if let select { act(select) } }
        .contextMenu {
            ForEach(Array(menus.enumerated()), id: \.offset) { _, m in
                Menu(m.title) {
                    ForEach(Array(m.entries.enumerated()), id: \.offset) { _, e in
                        switch e {
                        case let .item(title, command, _): Button(title) { act(command) }
                        case .separator: Divider()
                        }
                    }
                }
            }
        }
    }
}

/// The orange explanation under a pair of locations on different disks (SPEC §7A.4).
struct DifferentDisksWarning: View {
    let text: String

    var body: some View {
        Label(text, systemImage: "exclamationmark.triangle.fill")
            .font(Theme.secondary)
            .foregroundStyle(Color.orange)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14).padding(.vertical, 9)
    }
}

/// A row's controls, left to right.
struct ModelControls: View {
    let controls: [SettingsControl]
    /// `nil`: 8, or 6 for a sentence of words and fields ("Stop seeding when less than [ ] uploaded in [ ] hours").
    var spacing: CGFloat?
    let act: SettingsAct

    private var sentence: Bool {
        guard controls.count >= 5, case .label? = controls.first else { return false }
        return true
    }

    var body: some View {
        WrappingLine(spacing: spacing ?? (sentence ? 6 : 8)) {
            // A control is itself while its place and its setting stay: another setting in its place starts afresh.
            ForEach(Array(controls.enumerated()), id: \.offset) { n, c in ModelControlView(control: c, act: act).id("\(n) \(c.key.map { "\($0)" } ?? "")") }
        }
    }
}

/// A row's controls on one line, or when that is too wide (the narrow rules editor, a Settings tab in a narrow
/// window), the first ones each on a line of their own and the rest on the last: the fewest lines that fit. A layout,
/// so each control is one view measured once at its own size. It was a `ViewThatFits` with a candidate per way to
/// wrap, each holding the row's controls again (text fields, pop-ups, time pickers), nested in each fallback at first:
/// Settings › Transfers took the main thread 350 ms to lay out each time it was shown, and still 250 ms flat.
struct WrappingLine: Layout {
    /// Between the controls of a line; `lineSpacing` between lines.
    var spacing: CGFloat = 8
    var lineSpacing: CGFloat = 8

    /// How many controls go on lines of their own before the rest share the last one, for controls of these widths in
    /// `width` (`nil`: as wide as they like, one line). Two controls or fewer always share one line.
    static func alone(widths: [CGFloat], spacing: CGFloat, width: CGFloat?) -> Int {
        guard let width, widths.count > 2 else { return 0 }
        for k in 0..<(widths.count - 1) {
            let rest = widths[k...].reduce(0, +) + spacing * CGFloat(widths.count - k - 1)
            if max(rest, widths[..<k].max() ?? 0) <= width { return k }
        }
        return widths.count - 2
    }

    func makeCache(subviews: Subviews) -> [CGSize] { subviews.map { $0.sizeThatFits(.unspecified) } }

    func updateCache(_ cache: inout [CGSize], subviews: Subviews) { cache = makeCache(subviews: subviews) }

    /// The controls' sizes in `width`: their own, but one wider than the row as narrow as it goes (a long field).
    private func sizes(_ ideal: [CGSize], _ subviews: Subviews, width: CGFloat?) -> [CGSize] {
        guard let width else { return ideal }
        return ideal.indices.map { i in
            ideal[i].width <= width ? ideal[i] : subviews[i].sizeThatFits(ProposedViewSize(width: width, height: nil))
        }
    }

    /// The lines: each a range of the controls.
    private func lines(_ sizes: [CGSize], width: CGFloat?) -> [Range<Int>] {
        let k = Self.alone(widths: sizes.map(\.width), spacing: spacing, width: width)
        return (0..<k).map { $0..<($0 + 1) } + (sizes.isEmpty ? [] : [k..<sizes.count])
    }

    private func size(of line: Range<Int>, _ sizes: [CGSize]) -> CGSize {
        CGSize(width: line.map { sizes[$0].width }.reduce(0, +) + spacing * CGFloat(max(line.count - 1, 0)),
               height: line.map { sizes[$0].height }.max() ?? 0)
    }

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout [CGSize]) -> CGSize {
        let sized = sizes(cache, subviews, width: proposal.width)
        let all = lines(sized, width: proposal.width).map { size(of: $0, sized) }
        return CGSize(width: all.map(\.width).max() ?? 0,
                      height: all.map(\.height).reduce(0, +) + lineSpacing * CGFloat(max(all.count - 1, 0)))
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout [CGSize]) {
        let sized = sizes(cache, subviews, width: bounds.width)
        var y = bounds.minY
        for line in lines(sized, width: bounds.width) {
            let h = size(of: line, sized).height
            var x = bounds.minX
            for i in line {
                // Centred in its line, as an HStack places them.
                subviews[i].place(at: CGPoint(x: x, y: y + (h - sized[i].height) / 2), proposal: ProposedViewSize(sized[i]))
                x += sized[i].width + spacing
            }
            y += h + lineSpacing
        }
    }
}

/// One control.
struct ModelControlView: View {
    let control: SettingsControl
    let act: SettingsAct

    var body: some View {
        switch control {
        case let .toggle(key, on, enabled, help, checkbox):
            let toggle = Toggle("", isOn: Binding(get: { on }, set: { act(.set(key, .flag(on: $0))) }))
                .labelsHidden()
                .help(help)
                .disabled(!enabled)
            if checkbox { toggle.toggleStyle(.checkbox) } else { toggle.toggleStyle(.switch) }
        case let .choice(key, style, options, selected, enabled, help):
            let picker = Picker("", selection: Binding(get: { selected }, set: { act(.set(key, .choice(id: $0))) })) {
                ForEach(options, id: \.id) { o in Text(o.title).tag(o.id).disabled(!o.enabled) }
            }
            .labelsHidden().fixedSize()
            .disabled(!enabled)
            Group {
                if style == .segmented { picker.pickerStyle(.segmented) } else { picker }
            }
            .modifier(OptionalHelp(help: help))
        case let .field(key, kind, text, placeholder, units, unit, width, trailing, live, enabled, help, identifier):
            ModelField(kind: kind, text: text, placeholder: placeholder, units: units, unit: unit, width: CGFloat(width), trailing: trailing,
                       live: live, help: help, identifier: identifier) { act(.set(key, $0)) }
                .disabled(!enabled)
        case let .folder(key, path, placeholder, width, clearable, choose, help):
            HStack(spacing: 8) {
                TextField(placeholder, text: Binding(get: { path }, set: { act(.set(key, .typed(text: $0))) }))
                    .textFieldStyle(.roundedBorder)
                    // Its width while there is room, down to 100 in a narrow window (the rules editor at the main window's
                    // minimum width): fixed, it made the page wider than the window.
                    .frame(minWidth: min(CGFloat(width), 100), idealWidth: CGFloat(width), maxWidth: CGFloat(width))
                    .help(help)
                Button(choose.title) { act(choose.command) }.fixedSize().help(choose.help)
                if clearable, !path.isEmpty {
                    Button { act(.set(key, .typed(text: ""))) } label: { Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary) }
                        .buttonStyle(.plain).help("Use the default")
                }
            }
        case let .button(b):
            ModelButton(button: b, act: act)
        case let .checklist(key, title, checks):
            Menu {
                ForEach(checks, id: \.id) { c in
                    Toggle(c.title, isOn: Binding(get: { c.checked }, set: { _ in act(.set(key, .choice(id: c.id))) }))
                }
            } label: {
                Text(title).lineLimit(1)
            }
            .menuStyle(.button).fixedSize()
        case let .chips(key, checks, helpOn, helpOff):
            HStack(spacing: 2) {
                ForEach(checks, id: \.id) { c in
                    Button { act(.set(key, .choice(id: c.id))) } label: {
                        Text(c.title).font(Theme.secondary)
                            .padding(.horizontal, 9).padding(.vertical, 4)
                            .background(RoundedRectangle(cornerRadius: 6).fill(c.checked ? Color.primary.opacity(0.16) : .clear))
                    }
                    .buttonStyle(.plain)
                    .help((c.checked ? helpOn : helpOff).replacingOccurrences(of: "{}", with: c.title))
                }
            }
            .padding(2).background(RoundedRectangle(cornerRadius: 8).fill(.primary.opacity(0.06)))
        case let .link(title, url):
            if let u = URL(string: url) { Link(title, destination: u) }
        case let .label(text, tone, symbol, small, monospacedDigits, identifier):
            ModelLabel(text: text, tone: tone, symbol: symbol, small: small, monospacedDigits: monospacedDigits)
                .modifier(OptionalIdentifier(identifier: identifier))
        }
    }
}

extension SettingsControl {
    /// The setting it edits.
    var key: SettingKey? {
        switch self {
        case let .toggle(key, _, _, _, _), let .choice(key, _, _, _, _, _), let .folder(key, _, _, _, _, _, _), let .checklist(key, _, _),
             let .chips(key, _, _, _):
            key
        case let .field(key, _, _, _, _, _, _, _, _, _, _, _):
            key
        case .button, .link, .label:
            nil
        }
    }
}

extension SettingsCommand {
    /// A control's new value.
    static func set(_ key: SettingKey, _ value: SettingsValue) -> SettingsCommand { .apply(change: .set(key: key, value: value)) }
}

struct ModelButton: View {
    let button: SettingsButton
    let act: SettingsAct

    var body: some View {
        Group {
            switch button.style {
            case .normal: Button(button.title) { act(button.command) }
            case .link: Button(button.title) { act(button.command) }.buttonStyle(.link)
            case .prominent: Button(button.title) { act(button.command) }.buttonStyle(.borderedProminent)
            case .add:
                Button { act(button.command) } label: { Label(button.title, systemImage: "plus") }
                    .buttonStyle(.plain).foregroundStyle(Color.accentColor)
            case .remove:
                Button { act(button.command) } label: { Image(systemName: "minus.circle").foregroundStyle(.secondary) }
                    .buttonStyle(.plain)
            }
        }
        .disabled(!button.enabled)
        .help(button.help)
        .modifier(OptionalIdentifier(identifier: button.identifier))
    }
}

extension SettingsTone {
    var color: Color {
        switch self {
        case .primary: .primary
        case .secondary: .secondary
        case .accent: .accentColor
        case .warning: .orange
        case .problem: .red
        case .good: .green
        }
    }
}

extension SettingsSymbol {
    var systemName: String {
        switch self {
        case .check: "checkmark.circle.fill"
        case .mapped: "arrow.down.circle"
        case .checking: "ellipsis.circle"
        case .closed: "xmark.circle.fill"
        case .unknown: "questionmark.circle"
        case .warning: "exclamationmark.triangle.fill"
        }
    }
}

struct ModelLabel: View {
    let text: String
    let tone: SettingsTone
    let symbol: SettingsSymbol?
    let small: Bool
    let monospacedDigits: Bool

    var body: some View {
        Group {
            if let symbol {
                Label(text, systemImage: symbol.systemName)
            } else {
                Text(text)
            }
        }
        .font(small ? Theme.secondary : Theme.body)
        .foregroundStyle(tone.color)
        .monospacedDigit(monospacedDigits)
    }
}

private extension View {
    @ViewBuilder func monospacedDigit(_ on: Bool) -> some View {
        if on { self.monospacedDigit() } else { self }
    }
}

/// Text the core would refuse, in red (otherwise the field's own colour, dimmed while it is disabled).
private struct Refused: ViewModifier {
    let on: Bool
    func body(content: Content) -> some View {
        if on { content.foregroundStyle(Color.red) } else { content }
    }
}

private struct OptionalHelp: ViewModifier {
    let help: String?
    func body(content: Content) -> some View {
        if let help { content.help(help) } else { content }
    }
}

private struct OptionalIdentifier: ViewModifier {
    let identifier: String?
    func body(content: Content) -> some View {
        if let identifier { content.accessibilityIdentifier(identifier) } else { content }
    }
}

/// A text field (and its unit menu): keeps what is typed while it has focus, draws text the core would refuse in red,
/// and sends it on Return, when it loses focus, when the unit changes, and (`live`) after a pause in typing. The text
/// comes back from the model when the value changes; a refused value puts the model's text back. A new unit converts the
/// number (the core's `settingsFieldConvert`: 2 hours become 0.1 days, not 2 days), so the value stays.
struct ModelField: View {
    let kind: SettingsFieldKind
    let text: String
    let placeholder: String
    let units: [String]
    let unit: String
    let width: CGFloat
    var trailing = false
    var live = false
    var help: String?
    var identifier: String?
    let send: (SettingsValue) -> Void
    @State private var typed = ""
    @State private var picked = ""
    @State private var pending: Task<Void, Never>?
    /// What the number was before a run of unit changes (its text and unit) and what it was converted to: hours, days
    /// and back to hours give the hours typed, not a number rounded twice.
    @State private var origin: (text: String, unit: String, converted: String)?
    /// The unit menu was set from the model (not by the user): nothing to convert.
    @State private var syncing = false
    @FocusState private var focused: Bool

    var body: some View {
        HStack(spacing: units.isEmpty ? 0 : 4) {
            TextField(placeholder, text: $typed)
                .textFieldStyle(.roundedBorder)
                .multilineTextAlignment(trailing ? .trailing : .leading)
                .font(kind == .address ? Theme.mono : nil)
                // Its width while there is room, down to 100 in a narrow window (a wide field, as the folder's above).
                .frame(minWidth: width > 0 ? min(width, 100) : nil, idealWidth: width > 0 ? width : nil, maxWidth: width > 0 ? width : .infinity)
                .modifier(Refused(on: !settingsFieldValid(kind: kind, text: typed, unit: picked)))
                .monospacedDigit(kind == .syncPort)
                .focused($focused)
                .onSubmit { commit() }
                .modifier(OptionalHelp(help: help))
                .modifier(OptionalIdentifier(identifier: identifier))
            if !units.isEmpty {
                Picker("", selection: $picked) {
                    ForEach(units, id: \.self) { Text($0).tag($0) }
                }
                .pickerStyle(.menu).labelsHidden().fixedSize()
                .help("Unit of the value")
            }
        }
        .onAppear { typed = text; picked = unit }
        .onChange(of: text) {
            guard !focused else { return }
            typed = text
            origin = nil
            if picked != unit { syncing = true; picked = unit }
        }
        .onChange(of: picked) { old, new in
            if syncing { syncing = false; return }
            guard !old.isEmpty, new != old else { return }
            let from = origin.flatMap { $0.converted == typed ? ($0.text, $0.unit) : nil } ?? (typed, old)
            let converted = settingsFieldConvert(kind: kind, text: from.0, from: from.1, to: new)
            origin = (from.0, from.1, converted)
            typed = converted
            commit(keepTyped: true)
        }
        .onChange(of: focused) { if !focused { commit() } }
        .onChange(of: typed) { schedule() }
        .onDisappear { pending?.cancel() }
    }

    private func schedule() {
        pending?.cancel()
        guard live, focused else { return }
        pending = Task {
            try? await Task.sleep(for: .milliseconds(600))
            if !Task.isCancelled { commit() }
        }
    }

    /// `keepTyped`: a unit change, whose converted number stays shown (the value did not change).
    private func commit(keepTyped: Bool = false) {
        pending?.cancel()
        send(units.isEmpty ? .typed(text: typed) : .measure(text: typed, unit: picked))
        // Refused, or the same value: show the model's text again once the field is left, in the model's unit (a number
        // converted to the unit picked stays as it is: it is the value in force).
        guard !focused, !keepTyped, origin?.converted != typed else { return }
        typed = text
        if picked != unit { syncing = true; picked = unit }
    }
}
