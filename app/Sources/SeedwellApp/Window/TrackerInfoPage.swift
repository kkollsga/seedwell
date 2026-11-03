import SwiftUI

/// The Info page of a tracker (a view in the main window), drawn from the core's page (`trackerInfoPage`): interesting
/// numbers, then the tracker's settings and addresses. A favorite's addresses are edited here with a pen, an x and a
/// plus; an automatic tracker's are read-only.
struct TrackerInfoPage: View {
    @Bindable var model: LibraryModel
    let tracker: TrackerSummary
    let commands: Commands
    /// The tracker as last read (`trackerInfoRead`, off the main thread every `refreshMs`).
    @State private var read: TrackerInfoRead?

    private var page: TrackerInfoPageModel {
        let e = commands.favorites
        _ = e.view  // redrawn when the edit changes
        let rates = model.view.trackerRates
        return e.editor.trackerInfoPage(input: TrackerInfoInput(
            tracker: tracker, read: read, down: rates?.down ?? 0, up: rates?.up ?? 0,
            nowUnixS: UInt64(max(0, Date().timeIntervalSince1970)), host: hostWindowsAll()))
    }

    var body: some View {
        let p = page
        let e = commands.favorites
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                Button { model.trackerInfo = false } label: {
                    Label(p.back.title, systemImage: "chevron.left").font(Theme.secondary)
                }
                .buttonStyle(.plain).foregroundStyle(Color.accentColor)
                .help(p.back.help)

                HStack(spacing: 8) {
                    if p.favorite { Image(systemName: "star.fill").foregroundStyle(Color.accentColor.opacity(0.85)) }
                    Text(p.title).font(Theme.title).lineLimit(1)
                    Text(p.subtitle).font(Theme.body).foregroundStyle(.tertiary)
                }

                if let q = p.removal {
                    FavoriteRemoveStrip(question: q, confirm: { e.run(tracker.id, .confirmRemoval) }, cancel: { e.run(tracker.id, .cancelRemoval) })
                }

                if let h = p.header { TrackerHeader(model: h).padding(.horizontal, -10) }
                facts(p)
                if let x = p.pex { peerExchange(x) }
                speedLimits(p.limits)
                downloadLocation(p.location)
                addresses(p.addresses)
            }
            .padding(.horizontal, 22).padding(.vertical, 16)
            .frame(maxWidth: 640, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .task(id: tracker.id) {
            while !Task.isCancelled {
                await load()
                try? await Task.sleep(for: .milliseconds(Int(p.refreshMs)))
            }
        }
        .onChange(of: model.snapshot.trackers.map(\.count)) { _, _ in Task { await load() } }
        .onChange(of: tracker.id) { _, _ in commands.favorites.editor.revertFolders() }
    }

    private func load() async {
        let (core, t) = (commands.core, tracker)
        let r = await core.trackerInfoRead(tracker: t)
        commands.favorites.reload()
        read = r
    }

    // MARK: Sections

    private func section(_ t: String) -> some View {
        Text(t).font(Theme.caption.weight(.semibold)).foregroundStyle(.tertiary).textCase(.uppercase)
    }

    private func quiet(_ t: String) -> some View {
        Text(t).font(Theme.caption).foregroundStyle(.tertiary).fixedSize(horizontal: false, vertical: true)
    }

    private func facts(_ p: TrackerInfoPageModel) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            section(p.nowTitle)
            ForEach(Array(p.now.enumerated()), id: \.offset) { _, f in
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Text(f.label).font(Theme.secondary).foregroundStyle(.secondary).frame(width: 120, alignment: .leading)
                    Text(f.value).font(Theme.body).foregroundStyle(f.error ? Color.red : Color.primary).lineLimit(2)
                }
            }
        }
    }

    private func peerExchange(_ x: TrackerInfoPex) -> some View {
        let on = Binding<Bool>(get: { x.on }, set: { allow in
            read?.pexAllowed = allow
            commands.favorites.run(tracker.id, .setPex(allow: allow))
        })
        return VStack(alignment: .leading, spacing: 7) {
            section(x.title)
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Text(x.label).font(Theme.body)
                Spacer(minLength: 12)
                Toggle("", isOn: on).labelsHidden().toggleStyle(.switch)
                    .disabled(!x.enabled)
                    .help(x.help)
            }
            Text(x.note).font(Theme.caption).foregroundStyle(x.noteIsReason ? .secondary : .tertiary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// One cap for the tracker's torrents together (SPEC 7E.4), for a favorite; an automatic tracker says how to get one.
    private func speedLimits(_ l: TrackerInfoLimits) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            section(l.title)
            ForEach(Array(l.rows.enumerated()), id: \.offset) { _, row in
                HStack(alignment: .firstTextBaseline, spacing: 10) {
                    Text(row.label).font(Theme.secondary).foregroundStyle(.secondary).frame(width: 120, alignment: .leading)
                    RateField(bytes: limit(l, row.direction))
                    Text(row.line).font(Theme.secondary).monospacedDigit().foregroundStyle(.secondary)
                }
            }
            quiet(l.note)
        }
    }

    /// A limit's field; a change saves both (the other as shown).
    private func limit(_ l: TrackerInfoLimits, _ direction: RateDirection) -> Binding<UInt64> {
        let value = { (d: RateDirection) in l.rows.first { $0.direction == d }?.limit ?? 0 }
        return Binding(get: { value(direction) }, set: { new in
            let down = direction == .download ? new : value(.download)
            let up = direction == .upload ? new : value(.upload)
            commands.favorites.setLimits(tracker.id, down: down, up: up)
        })
    }

    /// The favorite's own folders (SPEC 7A.4); an automatic tracker says how to get them.
    private func downloadLocation(_ l: TrackerInfoLocation) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            section(l.title)
            if let rec = l.record {
                let draft = Binding<ProfileRecord>(get: { rec }, set: { commands.favorites.setFolderDraft(tracker.id, $0) })
                TrackerLocationEditor(record: draft, warnings: l.warnings)
                if !l.buttons.isEmpty {
                    HStack(spacing: 8) {
                        Spacer()
                        ForEach(Array(l.buttons.enumerated()), id: \.offset) { _, b in
                            if b.prominent {
                                Button(b.title) { commands.favorites.run(tracker.id, b.command) }.buttonStyle(.borderedProminent).help(b.help)
                            } else {
                                Button(b.title) { commands.favorites.run(tracker.id, b.command) }.help(b.help)
                            }
                        }
                    }
                }
                if let h = l.hint { Text(h).font(Theme.caption).foregroundStyle(.red) }
            }
            if let n = l.note { quiet(n) }
        }
        .id(tracker.id)
    }

    private func addresses(_ a: TrackerInfoAddresses) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            section(a.title)
            ForEach(Array(a.rows.enumerated()), id: \.offset) { _, row in
                if let f = row.field {
                    field(f)
                } else {
                    HStack(spacing: 6) {
                        Text(row.text).font(Theme.mono).foregroundStyle(.secondary)
                            .lineLimit(1).truncationMode(.middle)
                            .textSelection(.enabled)
                        Spacer(minLength: 8)
                        ForEach(Array(row.buttons.enumerated()), id: \.offset) { _, b in symbolButton(b) }
                    }
                    .frame(minHeight: 22)
                }
            }
            if let f = a.adding { field(f) }
            if let b = a.add { symbolButton(b) }
            if let n = a.note { Text(n).font(Theme.caption).foregroundStyle(.tertiary) }
        }
    }

    private func symbolButton(_ b: TrackerInfoButton) -> some View {
        let symbol = switch b.symbol {
        case .edit: "pencil"
        case .remove: "xmark"
        case .back: "chevron.left"
        case .add, nil: "plus"
        }
        return RowIconButton(symbol: symbol, help: b.help) { commands.favorites.run(tracker.id, b.command) }
    }

    private func field(_ f: TrackerInfoField) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            InlineField(editing: commands.favorites, target: f.target, prompt: f.prompt, font: Theme.mono)
            if let h = f.hint { Text(h).font(Theme.caption).foregroundStyle(.red).padding(.leading, 4) }
        }
    }
}

/// A small borderless symbol button with hover text and a soft highlight.
struct RowIconButton: View {
    let symbol: String
    let help: String
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: Theme.Icon.small, weight: .medium))
                .foregroundStyle(hovering ? Color.primary : Color.secondary)
                .frame(width: 22, height: 20)
                .background(RoundedRectangle(cornerRadius: 5, style: .continuous).fill(Color.primary.opacity(hovering ? 0.10 : 0)))
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .help(help)
        .accessibilityLabel(help)
    }
}
