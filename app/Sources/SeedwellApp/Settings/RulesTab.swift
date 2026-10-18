import SwiftUI

// MARK: Rules and tracker profiles (concept 5, SPEC §7 and §7E)

private enum RulesSection: String, CaseIterable, Identifiable {
    case rules = "Rules", trackers = "Trackers"
    var id: String { rawValue }
}

private let defaultRuleID = "\u{0}default"

struct RulesTab: View {
    @Bindable var model: SettingsModel
    @Bindable var nav: SettingsNavigation
    @State private var section: RulesSection = .rules

    var body: some View {
        VStack(spacing: 0) {
            Picker("", selection: $section) {
                ForEach(RulesSection.allCases) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.segmented).labelsHidden().fixedSize()
            .padding(.top, 14).padding(.bottom, 4)
            switch section {
            case .rules: RulesEditor(model: model)
            case .trackers: TrackersEditor(model: model, nav: nav)
            }
        }
        .frame(minHeight: 640)
        .onAppear { if nav.trackerToEdit != nil || nav.newTrackerHost != nil { section = .trackers } }
        .onChange(of: nav.trackerToEdit) { if nav.trackerToEdit != nil { section = .trackers } }
        .onChange(of: nav.newTrackerHost) { if nav.newTrackerHost != nil { section = .trackers } }
    }
}

// MARK: Rules

private struct RulesEditor: View {
    @Bindable var model: SettingsModel
    @State private var selected: String = defaultRuleID
    @State private var draft = RuleRecord.blank()
    @State private var draftDefault: DefaultRuleRecord?
    @State private var preview = RulePreview(affected: 0, changed: 0, changeNote: "")
    @State private var counts: [String: Int] = [:]

    private var isDefault: Bool { selected == defaultRuleID }
    private var saved: RuleRecord? { model.rulesDoc.rules.first { $0.id == selected } }
    private var dirty: Bool {
        isDefault ? draftDefault != model.rulesDoc.defaultRule : draft != saved
    }

    var body: some View {
        HStack(alignment: .top, spacing: 18) {
            list.frame(width: 280)
            ScrollView {
                editor.padding(.bottom, 12).environment(\.compactRows, true)
            }
        }
        .padding(.horizontal, 24).padding(.top, 8).padding(.bottom, 16)
        .onAppear { load(); recount() }
        .onChange(of: selected) { load(); recount() }
        .onChange(of: model.rulesDoc) { if !dirty { load() }; recount() }
        .onChange(of: draft) { refreshPreview() }
        .onChange(of: draftDefault) { refreshPreview() }
    }

    // MARK: List

    private var list: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Checked top to bottom. The first match wins.").font(.system(size: 12.5)).foregroundStyle(.secondary)
            VStack(spacing: 0) {
                List(selection: Binding(get: { Optional(selected) }, set: { if let v = $0 { selected = v } })) {
                    ForEach(Array(model.rulesDoc.rules.enumerated()), id: \.element.id) { n, r in
                        ruleRow(number: n + 1, title: r.name, subtitle: Self.summary(r), count: counts[r.id], enabled: r.enabled)
                            .tag(r.id)
                            .listRowSeparator(.visible)
                    }
                    .onMove { from, to in
                        var doc = model.rulesDoc
                        doc.rules.move(fromOffsets: from, toOffset: to)
                        model.saveRules(doc)
                    }
                }
                .listStyle(.plain)
                .scrollDisabled(true)
                .frame(height: CGFloat(model.rulesDoc.rules.count) * 60 + 2)
                Divider()
                ruleRow(number: nil, title: "Default", subtitle: "everything else · \(model.rulesDoc.defaultRule.goal.summary)", count: counts[defaultRuleID], enabled: true)
                    .padding(.horizontal, 8)
                    .background(selected == defaultRuleID ? Color.accentColor.opacity(0.28) : .clear)
                    .contentShape(Rectangle())
                    .onTapGesture { selected = defaultRuleID }
            }
            .background(RoundedRectangle(cornerRadius: 10).fill(.primary.opacity(0.055)))
            .clipShape(RoundedRectangle(cornerRadius: 10))
            ControlGroup {
                Button { addRule() } label: { Image(systemName: "plus") }.help("Add a rule")
                Button { removeSelected() } label: { Image(systemName: "minus") }.disabled(isDefault).help("Delete the rule")
            }
            .fixedSize()
        }
    }

    private func ruleRow(number: Int?, title: String, subtitle: String, count: Int?, enabled: Bool) -> some View {
        HStack(spacing: 10) {
            Text(number.map(String.init) ?? "·")
                .font(.system(size: 11, weight: .semibold)).monospacedDigit()
                .frame(width: 22, height: 22)
                .background(Circle().fill(.primary.opacity(0.1)))
            VStack(alignment: .leading, spacing: 1) {
                Text(title).font(.system(size: 13.5, weight: .semibold)).lineLimit(1)
                Text(subtitle).font(.system(size: 11.5)).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer(minLength: 4)
            if let count { Text("\(count)").font(.system(size: 12)).foregroundStyle(.secondary).monospacedDigit() }
        }
        .opacity(enabled ? 1 : 0.5)
        .frame(height: 52)
    }

    static func summary(_ r: RuleRecord) -> String {
        var parts: [String] = []
        if !r.trackerIds.isEmpty { parts.append(r.trackerIds.joined(separator: ", ")) }
        if r.privateMatch != .any { parts.append(r.privateMatch == .privateOnly ? "private" : "public") }
        if !r.categories.isEmpty { parts.append(r.categories.map(\.label).joined(separator: "/")) }
        if parts.isEmpty { parts.append("any torrent") }
        if let g = r.goal { parts.append(g.summary) }
        if let m = r.minSeedSecs, m > 0 { parts.append("≥ \(m / 3600) h") }
        if let b = r.idleBelowBytes, b > 0 { parts.append("< " + IdleText.summary(bytes: b, hours: r.idleHours)) }
        return parts.joined(separator: " · ")
    }

    // MARK: Editor

    @ViewBuilder private var editor: some View {
        VStack(alignment: .leading, spacing: 18) {
            if isDefault, draftDefault != nil {
                defaultEditor
            } else {
                matchSection
                finishSection
                locationSection
            }
            footer
        }
    }

    private var matchSection: some View {
        SettingsGroup(title: "Match") {
            SettingsRow(label: "Name") { TextField("", text: $draft.name).textFieldStyle(.roundedBorder).frame(width: 250) }
            SettingsRow(label: "Tracker", hint: "Torrents of these trackers. None ticked = any.") {
                Menu {
                    ForEach(model.profiles, id: \.id) { p in
                        Toggle(p.name, isOn: trackerBinding(p.id))
                    }
                    Toggle("Public (no tracker profile)", isOn: trackerBinding("public"))
                } label: {
                    Text(draft.trackerIds.isEmpty ? "Any" : draft.trackerIds.map(trackerName).joined(separator: ", "))
                        .lineLimit(1)
                }
                .menuStyle(.button).fixedSize()
            }
            SettingsRow(label: "Content type") {
                HStack(spacing: 2) {
                    chip("Any", on: draft.categories.isEmpty) { draft.categories = [] }
                    ForEach(Category.all, id: \.self) { c in
                        chip(c.label, on: draft.categories.contains(c)) {
                            if draft.categories.contains(c) { draft.categories.removeAll { $0 == c } } else { draft.categories.append(c) }
                        }
                    }
                }
                .padding(2).background(RoundedRectangle(cornerRadius: 8).fill(.primary.opacity(0.06)))
            }
            SettingsRow(label: "Private torrents") {
                Picker("", selection: $draft.privateMatch) {
                    Text("Any").tag(PrivateMatch.any)
                    Text("Private only").tag(PrivateMatch.privateOnly)
                    Text("Public only").tag(PrivateMatch.publicOnly)
                }
                .labelsHidden().fixedSize()
            }
            SettingsRow(label: "Enabled") { Toggle("", isOn: $draft.enabled).labelsHidden().toggleStyle(.switch).help("Turn this rule on or off") }
        }
    }

    private func chip(_ title: String, on: Bool, _ action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(title).font(.system(size: 12.5))
                .padding(.horizontal, 9).padding(.vertical, 4)
                .background(RoundedRectangle(cornerRadius: 6).fill(on ? Color.primary.opacity(0.16) : .clear))
        }
        .buttonStyle(.plain)
        .help(on ? "Stop matching \(title)" : "Match \(title)")
    }

    private var finishSection: some View {
        SettingsGroup(title: "When the download finishes") {
            SettingsRow(label: "Goal", hint: "Same as Default = this rule changes nothing here") {
                HStack(spacing: 10) {
                    Picker("", selection: Binding(get: { draft.goal == nil ? 0 : 1 }, set: { draft.goal = $0 == 0 ? nil : (draft.goal ?? model.rulesDoc.defaultRule.goal) })) {
                        Text("Same as Default").tag(0)
                        Text("Custom").tag(1)
                    }
                    .labelsHidden().fixedSize()
                    if draft.goal != nil { GoalEditor(goal: Binding(get: { draft.goal ?? .forever }, set: { draft.goal = $0 })) }
                }
            }
            SettingsRow(label: "Minimum seed time", hint: "Never stops earlier, even if the ratio is met. Hit-and-run protection.") {
                HStack(spacing: 10) {
                    Toggle("", isOn: Binding(get: { draft.minSeedSecs != nil }, set: { draft.minSeedSecs = $0 ? 72 * 3600 : nil })).labelsHidden()
                        .help("Keep seeding for a minimum time before the goal can stop it")
                    if draft.minSeedSecs != nil {
                        DurationField(secs: Binding(get: { draft.minSeedSecs ?? 72 * 3600 }, set: { draft.minSeedSecs = $0 }))
                    } else {
                        Text("None").foregroundStyle(.secondary)
                    }
                }
            }
            SettingsRow(label: "Inactive seeds", hint: "Stops a seed that uploads less than this in the last hours of seeding. Never before the minimum seed time.") {
                HStack(spacing: 10) {
                    Picker("", selection: Binding(
                        get: { draft.idleBelowBytes == nil ? 0 : (draft.idleBelowBytes == 0 ? 1 : 2) },
                        set: { draft.idleBelowBytes = $0 == 0 ? nil : ($0 == 1 ? 0 : (draft.idleBelowBytes.flatMap { $0 > 0 ? $0 : nil } ?? 1_000_000_000)) })) {
                        Text("Same as Settings").tag(0)
                        Text("Off").tag(1)
                        Text("Custom").tag(2)
                    }
                    .labelsHidden().fixedSize()
                    if let b = draft.idleBelowBytes, b > 0 {
                        IdleStopField(bytes: Binding(get: { draft.idleBelowBytes ?? 0 }, set: { draft.idleBelowBytes = $0 }), hours: $draft.idleHours, prefix: "Less than")
                    }
                }
            }
            SettingsRow(label: "At goal") {
                Picker("", selection: Binding(get: { draft.onGoal }, set: { draft.onGoal = $0 })) {
                    Text("Same as Default").tag(OnGoalAction?.none)
                    Text("Stop, keep in list").tag(OnGoalAction?.some(.stopKeep))
                    Text("Remove, keep files").tag(OnGoalAction?.some(.removeKeepFiles))
                }
                .labelsHidden().fixedSize()
            }
        }
    }

    private var locationSection: some View {
        SettingsGroup(title: "Priority & location") {
            SettingsRow(label: "Priority", hint: "Jumps the queue, gets bandwidth and upload slots first.") {
                Picker("", selection: Binding(get: { draft.priority }, set: { draft.priority = $0 })) {
                    Text("Default").tag(Priority?.none)
                    Text("Low").tag(Priority?.some(.low))
                    Text("Normal").tag(Priority?.some(.normal))
                    Text("★ High").tag(Priority?.some(.high))
                }
                .pickerStyle(.segmented).labelsHidden().fixedSize()
            }
            SettingsRow(label: "Files go to", hint: "A folder. Empty = the Finished location (or the content type's).") {
                OptionalPathField(path: $draft.finishedLocation, placeholder: "Default", width: 250)
            }
            SettingsRow(label: "Upload limit") { OptionalLimit(bytes: $draft.upLimit) }
        }
    }

    private var defaultEditor: some View {
        let d = Binding(get: { draftDefault ?? model.rulesDoc.defaultRule }, set: { draftDefault = $0 })
        return VStack(alignment: .leading, spacing: 18) {
            SettingsGroup(title: "Default rule") {
                SettingsRow(label: "Applies to", hint: "Every torrent no other rule matches. Cannot be deleted or moved.") { Text("Everything else") }
            }
            SettingsGroup(title: "When the download finishes") {
                SettingsRow(label: "Goal") { GoalEditor(goal: d.goal) }
                SettingsRow(label: "Minimum seed time", hint: "Hit-and-run protection.") {
                    HStack(spacing: 10) {
                        Toggle("", isOn: Binding(get: { d.wrappedValue.minSeedSecs != nil }, set: { d.wrappedValue.minSeedSecs = $0 ? 72 * 3600 : nil })).labelsHidden()
                            .help("Keep seeding for a minimum time before the goal can stop it")
                        if d.wrappedValue.minSeedSecs != nil {
                            DurationField(secs: Binding(get: { d.wrappedValue.minSeedSecs ?? 72 * 3600 }, set: { d.wrappedValue.minSeedSecs = $0 }))
                        } else {
                            Text("None").foregroundStyle(.secondary)
                        }
                    }
                }
                SettingsRow(label: "At goal") {
                    Picker("", selection: d.onGoal) {
                        Text("Stop, keep in list").tag(OnGoalAction.stopKeep)
                        Text("Remove, keep files").tag(OnGoalAction.removeKeepFiles)
                    }
                    .labelsHidden().fixedSize()
                }
            }
            SettingsGroup(title: "Priority & location") {
                SettingsRow(label: "Priority") {
                    Picker("", selection: d.priority) {
                        Text("Low").tag(Priority.low)
                        Text("Normal").tag(Priority.normal)
                        Text("★ High").tag(Priority.high)
                    }
                    .pickerStyle(.segmented).labelsHidden().fixedSize()
                }
                SettingsRow(label: "Files go to", hint: "A folder. Empty = the Finished location (or the content type's).") {
                    OptionalPathField(path: d.finishedLocation, placeholder: "Default", width: 250)
                }
                SettingsRow(label: "Upload limit") { OptionalLimit(bytes: d.upLimit, allowDefault: false) }
            }
        }
    }

    private var footer: some View {
        HStack(alignment: .center, spacing: 12) {
            Image(systemName: "info.circle").foregroundStyle(Color.accentColor)
            footerText.font(.system(size: 12.5)).frame(maxWidth: .infinity, alignment: .leading)
            Button("Revert") { load() }.disabled(!dirty)
                .help("Discard unsaved changes to this rule")
            Button("Save Rule") { save() }.buttonStyle(.borderedProminent).disabled(!dirty)
                .help("Save this rule")
        }
        .padding(12)
        .background(RoundedRectangle(cornerRadius: 10).fill(Color.accentColor.opacity(0.12)))
    }

    private var footerText: Text {
        if !model.problems.isEmpty { return Text(model.problems.joined(separator: " ")).foregroundStyle(.red) }
        if isDefault {
            return Text("Applies to ") + Text("\(preview.affected) torrents").bold() + Text(".")
                + (preview.changed > 0 ? Text(" Saving changes \(preview.changed).") : Text(""))
        }
        var t = Text("Applies to ") + Text("\(preview.affected) torrent\(preview.affected == 1 ? "" : "s")").bold() + Text(".")
        if preview.changed > 0 {
            t = t + Text(" Saving changes ") + Text("\(preview.changed)").bold()
            if !preview.changeNote.isEmpty { t = t + Text(": \(preview.changeNote)") }
            t = t + Text(".")
        }
        return t
    }

    // MARK: Actions

    private func trackerName(_ id: String) -> String {
        id == "public" ? "Public" : (model.profiles.first { $0.id == id }?.name ?? id)
    }

    private func trackerBinding(_ id: String) -> Binding<Bool> {
        Binding(get: { draft.trackerIds.contains(id) }, set: { on in
            if on { if !draft.trackerIds.contains(id) { draft.trackerIds.append(id) } } else { draft.trackerIds.removeAll { $0 == id } }
        })
    }

    private func load() {
        model.clearProblems()
        if isDefault {
            draftDefault = model.rulesDoc.defaultRule
        } else if let r = saved {
            draft = r
            draftDefault = nil
        } else {
            selected = defaultRuleID
            draftDefault = model.rulesDoc.defaultRule
        }
        refreshPreview()
    }

    private func docWithDraft() -> RulesDoc {
        var doc = model.rulesDoc
        if isDefault, let d = draftDefault { doc.defaultRule = d }
        else if let i = doc.rules.firstIndex(where: { $0.id == draft.id }) { doc.rules[i] = draft }
        return doc
    }

    private func refreshPreview() {
        preview = model.core.rulePreview(doc: docWithDraft(), ruleId: isDefault ? "*" : draft.id)
    }

    private func recount() {
        var c: [String: Int] = [:]
        for r in model.rulesDoc.rules { c[r.id] = Int(model.core.rulePreview(doc: model.rulesDoc, ruleId: r.id).affected) }
        c[defaultRuleID] = Int(model.core.rulePreview(doc: model.rulesDoc, ruleId: "*").affected)
        counts = c
    }

    private func save() {
        let doc = docWithDraft()
        if model.saveRules(doc) { load() }
    }

    private func addRule() {
        var doc = model.rulesDoc
        var r = RuleRecord.blank()
        r.name = "New rule"
        doc.rules.append(r)
        guard model.saveRules(doc), let last = model.rulesDoc.rules.last else { return }
        selected = last.id
    }

    private func removeSelected() {
        guard !isDefault else { return }
        var doc = model.rulesDoc
        doc.rules.removeAll { $0.id == selected }
        if model.saveRules(doc) { selected = defaultRuleID }
    }
}

extension RuleRecord {
    static func blank() -> RuleRecord {
        RuleRecord(id: "", name: "", enabled: true, trackerIds: [], privateMatch: .any, categories: [], goal: nil,
                   minSeedSecs: nil, priority: nil, onGoal: nil, finishedLocation: nil, upLimit: nil,
                   idleBelowBytes: nil, idleHours: IdleText.defaultHours)
    }
}

/// Upload limit of a rule: Default (nil), Unlimited (0) or a number of MB/s.
struct OptionalLimit: View {
    @Binding var bytes: UInt64?
    var allowDefault = true

    var body: some View {
        HStack(spacing: 8) {
            Picker("", selection: Binding(get: { bytes == nil ? (allowDefault ? 0 : 1) : (bytes == 0 ? 1 : 2) }, set: { v in
                switch v {
                case 0: bytes = nil
                case 1: bytes = allowDefault ? 0 : nil
                default: bytes = bytes.flatMap { $0 == 0 ? nil : $0 } ?? 1_000_000
                }
            })) {
                if allowDefault { Text("Default").tag(0) }
                Text("Unlimited").tag(1)
                Text("Limit").tag(2)
            }
            .labelsHidden().fixedSize()
            if let b = bytes, b > 0 {
                TextField("", value: Binding(get: { Double(b) / 1_000_000 }, set: { bytes = UInt64(max(0.01, $0) * 1_000_000) }), format: .number.locale(enUS))
                    .textFieldStyle(.roundedBorder).frame(width: 72)
                Text("MB/s")
            }
        }
    }
}

// MARK: Trackers (SPEC §7E.1)

private struct TrackersEditor: View {
    @Bindable var model: SettingsModel
    @Bindable var nav: SettingsNavigation
    @State private var selected: String?
    @State private var draft = ProfileRecord.blank()
    @State private var isNew = false

    private var saved: ProfileRecord? { model.profiles.first { $0.id == selected } }
    private var dirty: Bool { isNew || draft != saved }

    var body: some View {
        HStack(alignment: .top, spacing: 18) {
            list.frame(width: 280)
            ScrollView {
                if selected != nil || isNew { editor.padding(.bottom, 12).environment(\.compactRows, true) }
                else {
                    Text("Select a tracker, or add one with +. A tracker is a name and the announce addresses that belong to it. Torrents are matched by their announce URLs; passkeys are never stored.")
                        .font(.system(size: 12.5)).foregroundStyle(.secondary).padding(.top, 8)
                }
            }
        }
        .padding(.horizontal, 24).padding(.top, 8).padding(.bottom, 16)
        .onAppear { applyNav(); if selected == nil && !isNew { selected = model.profiles.first?.id; load() } }
        .onChange(of: selected) { if !isNew { load() } }
        .onChange(of: nav.trackerToEdit) { applyNav() }
        .onChange(of: nav.newTrackerHost) { applyNav() }
    }

    private func applyNav() {
        if let host = nav.newTrackerHost {
            startNew()
            draft.name = host
            draft.urls = [host]
            nav.newTrackerHost = nil
            return
        }
        if let id = nav.trackerToEdit {
            isNew = false
            selected = id
            nav.trackerToEdit = nil
            load()
        }
    }

    private var list: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Torrents match a tracker by their announce addresses.").font(.system(size: 12.5)).foregroundStyle(.secondary)
            VStack(spacing: 0) {
                if model.profiles.isEmpty && !isNew {
                    Text("No trackers yet").foregroundStyle(.secondary).padding(14).frame(maxWidth: .infinity)
                }
                ForEach(model.profiles, id: \.id) { p in
                    HStack(spacing: 10) {
                        Image(systemName: "dot.radiowaves.left.and.right").frame(width: 22).foregroundStyle(.secondary)
                        VStack(alignment: .leading, spacing: 1) {
                            Text(p.name).font(.system(size: 13.5, weight: .semibold)).lineLimit(1)
                            Text(p.urls.first ?? "").font(.system(size: 11.5)).foregroundStyle(.secondary).lineLimit(1)
                        }
                        Spacer()
                    }
                    .padding(.horizontal, 10).frame(height: 52)
                    .background(selected == p.id && !isNew ? Color.accentColor.opacity(0.28) : .clear)
                    .contentShape(Rectangle())
                    .onTapGesture { isNew = false; selected = p.id; load() }
                    Divider()
                }
                if isNew {
                    HStack { Image(systemName: "plus.circle").frame(width: 22); Text("New tracker").font(.system(size: 13.5, weight: .semibold)); Spacer() }
                        .padding(.horizontal, 10).frame(height: 52).background(Color.accentColor.opacity(0.28))
                }
            }
            .background(RoundedRectangle(cornerRadius: 10).fill(.primary.opacity(0.055)))
            .clipShape(RoundedRectangle(cornerRadius: 10))
            ControlGroup {
                Button { startNew() } label: { Image(systemName: "plus") }.help("Add a tracker")
                Button { remove() } label: { Image(systemName: "minus") }.disabled(selected == nil || isNew).help("Delete the tracker")
            }
            .fixedSize()
        }
    }

    private var editor: some View {
        VStack(alignment: .leading, spacing: 18) {
            SettingsGroup(title: "Tracker") {
                SettingsRow(label: "Name") { TextField("", text: $draft.name).textFieldStyle(.roundedBorder).frame(width: 260) }
                SettingsRow(label: "Rule", hint: "Seeding behaviour for this tracker's torrents.") {
                    Picker("", selection: Binding(get: { draft.rule ?? "" }, set: { draft.rule = $0.isEmpty ? nil : $0 })) {
                        Text("By the rules' order").tag("")
                        ForEach(model.rulesDoc.rules, id: \.id) { Text($0.name).tag($0.id) }
                    }
                    .labelsHidden().fixedSize()
                }
            }
            SettingsGroup(title: "Download location") {
                TrackerLocationEditor(record: $draft, warnings: model.trackerVolumeWarnings(draft))
                    .padding(.horizontal, 14).padding(.vertical, 9)
            }
            SettingsGroup(title: "Hidden") {
                ToggleRow(title: "Hide this tracker", isOn: $draft.hidden)
                if draft.hidden {
                    ToggleRow(title: "Hide only what is sharing", isOn: $draft.hideOnlySeeding)
                }
                Text(draft.hidden
                     ? (draft.hideOnlySeeding
                        ? "Finished torrents of this tracker are left out of Sharing and the menu bar; downloads show as usual."
                        : "Its torrents are left out of Downloads, Sharing and the menu bar, and the tracker is not in the sidebar.")
                     : "Hide a tracker to keep its torrents out of the lists and the menu bar. Nothing else changes.")
                    .font(.system(size: 12)).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal, 14).padding(.vertical, 9)
                if !isNew, saved != nil {
                    HStack {
                        Button("Show Torrents") { nav.openTracker(draft.id) }
                            .help("Open this tracker's tab with all of its torrents")
                        Spacer()
                    }
                    .padding(.horizontal, 14).padding(.vertical, 9)
                }
            }
            SettingsGroup(title: "Announce addresses") {
                ForEach(draft.urls.indices, id: \.self) { i in
                    HStack(spacing: 8) {
                        TextField("https://tracker.example.org/announce", text: $draft.urls[i])
                            .textFieldStyle(.roundedBorder).font(.system(size: 12.5, design: .monospaced))
                        Button { draft.urls.remove(at: i) } label: { Image(systemName: "minus.circle").foregroundStyle(.secondary) }
                            .buttonStyle(.plain).disabled(draft.urls.count <= 1)
                            .help("Remove this address")
                    }
                    .padding(.horizontal, 14).padding(.vertical, 7)
                }
                HStack {
                    Button { draft.urls.append("") } label: { Label("Add Address", systemImage: "plus") }.buttonStyle(.plain).foregroundStyle(Color.accentColor)
                        .help("Add another announce address for this tracker")
                    Spacer()
                    Text("A host, *.host, or a full URL. Passkeys are dropped.").font(.system(size: 11.5)).foregroundStyle(.secondary)
                }
                .padding(.horizontal, 14).padding(.vertical, 9)
            }
            HStack(alignment: .center, spacing: 12) {
                Image(systemName: "info.circle").foregroundStyle(Color.accentColor)
                Group {
                    if model.problems.isEmpty {
                        Text("Passkeys in announce URLs are never stored or shown.")
                    } else {
                        Text(model.problems.joined(separator: " ")).foregroundStyle(.red)
                    }
                }
                .font(.system(size: 12.5)).frame(maxWidth: .infinity, alignment: .leading)
                Button("Revert") { isNew = false; load() }.disabled(!dirty)
                    .help("Discard unsaved changes to this tracker")
                Button("Save Tracker") { save() }.buttonStyle(.borderedProminent).disabled(!dirty || draft.name.trimmingCharacters(in: .whitespaces).isEmpty)
                    .help("Save this tracker profile")
            }
            .padding(12)
            .background(RoundedRectangle(cornerRadius: 10).fill(Color.accentColor.opacity(0.12)))
        }
    }

    private func load() {
        model.clearProblems()
        if let s = saved { draft = s } else if !isNew { draft = .blank() }
    }

    private func startNew() {
        draft = .blank()
        draft.name = ""
        draft.urls = [""]
        isNew = true
        selected = nil
    }

    private func save() {
        var list = model.profiles
        if isNew { list.append(draft) } else if let i = list.firstIndex(where: { $0.id == draft.id }) { list[i] = draft }
        if model.saveProfiles(list) {
            if isNew { selected = model.profiles.last?.id }
            isNew = false
            load()
        }
    }

    private func remove() {
        guard let id = selected else { return }
        let a = NSAlert()
        a.messageText = "Delete “\(saved?.name ?? id)”?"
        a.informativeText = "Its torrents move to Public or Other private. Statistics history is kept."
        a.addButton(withTitle: "Delete")
        a.addButton(withTitle: "Cancel")
        guard a.runModal() == .alertFirstButtonReturn else { return }
        if model.saveProfiles(model.profiles.filter { $0.id != id }) { selected = model.profiles.first?.id; load() }
    }
}

extension ProfileRecord {
    static func blank() -> ProfileRecord {
        ProfileRecord(id: "", name: "", urls: [""], rule: nil, color: nil, icon: nil, downLimit: nil, upLimit: nil,
                      incompleteDir: nil, finishedDir: nil, categories: [], hidden: false, hideOnlySeeding: false)
    }
}
