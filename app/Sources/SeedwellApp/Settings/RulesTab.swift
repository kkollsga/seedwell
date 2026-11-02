import SwiftUI

// MARK: Rules and tracker profiles (concept 5, SPEC §7 and §7E)

/// Settings › Rules: the core's rules editor model (`rulesEditorModel`) drawn as a list on the left and the editor on
/// the right. Every word, row and rule is the core's; this draws them, runs the saves it asks for and opens the
/// tracker's tab or folder editor.
struct RulesTab: View {
    @Bindable var model: SettingsModel
    @Bindable var nav: SettingsNavigation

    var body: some View {
        let m = model.rulesPage()
        VStack(spacing: 0) {
            Picker("", selection: Binding(get: { m.selected }, set: { model.rules(.section(section: $0 == "trackers" ? .trackers : .rules)) })) {
                ForEach(m.sections, id: \.id) { Text($0.title).tag($0.id) }
            }
            .pickerStyle(.segmented).labelsHidden().fixedSize()
            .padding(.top, 14).padding(.bottom, 4)
            HStack(alignment: .top, spacing: 18) {
                list(m).frame(width: 280)
                ScrollView {
                    editor(m).padding(.bottom, 12).environment(\.compactRows, true)
                }
            }
            .padding(.horizontal, 24).padding(.top, 8).padding(.bottom, 16)
        }
        .frame(minHeight: 640)
        .onAppear {
            applyNav()
            model.recountRules()
        }
        .onChange(of: nav.trackerToEdit) { applyNav() }
        .onChange(of: nav.newTrackerHost) { applyNav() }
    }

    /// "Edit Tracker…" and "Create tracker profile…" from elsewhere in the app.
    private func applyNav() {
        if let host = nav.newTrackerHost {
            nav.newTrackerHost = nil
            model.rules(.newTracker(host: host))
        } else if let id = nav.trackerToEdit {
            nav.trackerToEdit = nil
            model.rules(.editTracker(id: id))
        }
    }

    /// The editor's controls change the draft; a folder picked goes there too; Show Torrents opens the tracker's tab.
    private func act(_ c: SettingsCommand) {
        switch c {
        case let .apply(.set(key, value)): model.rules(.set(key: key, value: value))
        case let .chooseFolder(key, start):
            if let p = FolderPicker.choose(start: start) { model.rules(.set(key: key, value: .typed(text: p))) }
        case .showTrackerTorrents(let id): nav.openTracker(id)
        default: model.run(c)
        }
    }

    // MARK: List

    private func list(_ m: RulesEditorModel) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(m.note).font(.system(size: 12.5)).foregroundStyle(.secondary)
            VStack(spacing: 0) {
                if let empty = m.empty {
                    Text(empty).foregroundStyle(.secondary).padding(14).frame(maxWidth: .infinity)
                }
                if m.reorderable {
                    List(selection: Binding(get: { m.rows.first(where: \.selected)?.id },
                                            set: { id in if let r = m.rows.first(where: { $0.id == id }) { act(r.select) } })) {
                        ForEach(m.rows, id: \.id) { r in
                            row(r).tag(r.id).listRowSeparator(.visible)
                        }
                        .onMove { from, to in
                            if let f = from.first { model.rules(.moveRule(from: UInt32(f), to: UInt32(to))) }
                        }
                    }
                    .listStyle(.plain)
                    .scrollDisabled(true)
                    .frame(height: CGFloat(m.rows.count) * 60 + 2)
                    Divider()
                } else {
                    ForEach(m.rows, id: \.id) { r in
                        row(r).padding(.horizontal, 2)
                            .background(r.selected ? Color.accentColor.opacity(0.28) : .clear)
                            .contentShape(Rectangle())
                            .onTapGesture { act(r.select) }
                        Divider()
                    }
                }
                if let d = m.defaultRow {
                    row(d)
                        .padding(.horizontal, 8)
                        .background(d.selected ? Color.accentColor.opacity(0.28) : .clear)
                        .contentShape(Rectangle())
                        .onTapGesture { act(d.select) }
                }
            }
            .background(RoundedRectangle(cornerRadius: 10).fill(.primary.opacity(0.055)))
            .clipShape(RoundedRectangle(cornerRadius: 10))
            ControlGroup {
                Button { act(m.listButtons.add.command) } label: { Image(systemName: "plus") }.help(m.listButtons.add.help)
                Button { act(m.listButtons.remove.command) } label: { Image(systemName: "minus") }
                    .disabled(!m.listButtons.remove.enabled).help(m.listButtons.remove.help)
            }
            .fixedSize()
        }
    }

    @ViewBuilder private func row(_ r: RulesListRow) -> some View {
        if let badge = r.badge {
            HStack(spacing: 10) {
                Text(badge)
                    .font(.system(size: 11, weight: .semibold)).monospacedDigit()
                    .frame(width: 22, height: 22)
                    .background(Circle().fill(.primary.opacity(0.1)))
                VStack(alignment: .leading, spacing: 1) {
                    Text(r.title).font(.system(size: 13.5, weight: .semibold)).lineLimit(1)
                    Text(r.subtitle).font(.system(size: 11.5)).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer(minLength: 4)
                if let count = r.count { Text("\(count)").font(.system(size: 12)).foregroundStyle(.secondary).monospacedDigit() }
            }
            .opacity(r.enabled ? 1 : 0.5)
            .frame(height: 52)
        } else {
            HStack(spacing: 10) {
                Image(systemName: r.new ? "plus.circle" : "dot.radiowaves.left.and.right").frame(width: 22)
                    .foregroundStyle(r.new ? Color.primary : .secondary)
                VStack(alignment: .leading, spacing: 1) {
                    Text(r.title).font(.system(size: 13.5, weight: .semibold)).lineLimit(1)
                    if !r.subtitle.isEmpty {
                        Text(r.subtitle).font(.system(size: 11.5)).foregroundStyle(.secondary).lineLimit(1)
                    }
                }
                Spacer()
            }
            .padding(.horizontal, 8).frame(height: 52)
        }
    }

    // MARK: Editor

    private func editor(_ m: RulesEditorModel) -> some View {
        VStack(alignment: .leading, spacing: 18) {
            if let placeholder = m.placeholder {
                Text(placeholder).font(.system(size: 12.5)).foregroundStyle(.secondary).padding(.top, 8)
            }
            ForEach(m.groups, id: \.id) { g in
                ModelGroupView(group: g, act: act)
                if g.id == m.foldersAfter, let profile = m.foldersProfile {
                    SettingsBox(title: m.foldersTitle) {
                        TrackerFoldersEditor(profile: profile, model: model)
                            .padding(.horizontal, 14).padding(.vertical, 9)
                    }
                }
            }
            if let f = m.footer { footer(f) }
        }
    }

    private func footer(_ f: RulesFooter) -> some View {
        HStack(alignment: .center, spacing: 12) {
            Image(systemName: "info.circle").foregroundStyle(Color.accentColor)
            f.runs.reduce(Text("")) { t, r in t + (r.bold ? Text(r.text).bold() : Text(r.text)) }
                .foregroundStyle(f.problem ? Color.red : .primary)
                .font(.system(size: 12.5)).frame(maxWidth: .infinity, alignment: .leading)
            ModelButton(button: f.revert, act: act)
            ModelButton(button: f.save, act: act)
        }
        .padding(12)
        .background(RoundedRectangle(cornerRadius: 10).fill(Color.accentColor.opacity(0.12)))
    }
}

/// A tracker's own folders, with the different-disk warnings of what the editor has: the core looks at the disks (which
/// may be slow or gone) off the main thread whenever the folders change.
private struct TrackerFoldersEditor: View {
    let profile: ProfileRecord
    let model: SettingsModel
    @State private var warnings: [VolumeWarning] = []

    var body: some View {
        TrackerLocationEditor(record: Binding(get: { profile }, set: { model.rules(.trackerFolders(profile: $0)) }), warnings: warnings)
            .task(id: profile) {
                let core = model.core
                warnings = await core.trackerVolumeWarnings(profile: profile)
            }
    }
}
