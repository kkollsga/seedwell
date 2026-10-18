import SwiftUI

/// Choose what to download (concept 10, SPEC §7A.6): structure groups with tri-state checkboxes,
/// "In library" badges, quick picks, a footer with size and free space, and a primary button that
/// names the choice. Nothing is downloaded or announced before it is confirmed.
struct ChooserView: View {
    @Bindable var model: ChooserModel
    var confirm: () -> Void = {}
    /// The flag is ⇧-Cancel: keep the `.torrent` file (SPEC 7A.6).
    var cancel: (_ keepFile: Bool) -> Void = { _ in }
    /// Cancel would move a `.torrent` file to the Trash, so ⇧ has something to keep.
    var hasSourceFile = false
    var changeCategory: (Category) -> Void = { _ in }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            categoryCard.padding(.top, 14)
            ruleCard.padding(.top, 8)
            if let notice = model.notice {
                Label {
                    Text(notice).font(.system(size: 13)).fixedSize(horizontal: false, vertical: true)
                } icon: {
                    Image(systemName: model.unsure ? "questionmark.circle.fill" : "checkmark.circle.fill")
                        .foregroundStyle(model.unsure ? Color.orange : Color.green)
                }
                .padding(.top, 12)
            }
            if model.quickPicks.count > 1 { quickPicks.padding(.top, 12) }
            list.padding(.top, 10)
            footer.padding(.top, 12)
        }
        .padding(22)
        .frame(width: 640)
        .background(.background)
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(model.origin.uppercased()).font(.system(size: 11, weight: .semibold)).tracking(0.5).foregroundStyle(.secondary)
            Text(model.name).font(.system(size: 17, weight: .semibold)).lineLimit(2)
            Text(model.summary).font(.system(size: 13)).foregroundStyle(.secondary)
        }
    }

    private var categoryCard: some View {
        HStack(spacing: 12) {
            Image(systemName: model.categorySymbol)
                .font(.system(size: 20)).frame(width: 46, height: 46)
                .background(RoundedRectangle(cornerRadius: 10).fill(.primary.opacity(0.08)))
            VStack(alignment: .leading, spacing: 2) {
                Text(model.categoryTitle).font(.system(size: 14, weight: .semibold)).lineLimit(1)
                Text(model.categoryDetail).font(.system(size: 12)).foregroundStyle(.secondary)
            }
            Spacer()
            Menu("Change") {
                ForEach(Category.all, id: \.self) { c in
                    Toggle(c.label, isOn: Binding(get: { c == model.category }, set: { _ in changeCategory(c) }))
                }
            }
            .menuStyle(.button).fixedSize()
            .help("Change what kind of content this is")
        }
        .padding(10)
        .background(RoundedRectangle(cornerRadius: 12).fill((model.unsure ? Color.orange : Color.accentColor).opacity(0.14)))
        .overlay(RoundedRectangle(cornerRadius: 12).stroke((model.unsure ? Color.orange : Color.accentColor).opacity(0.35), lineWidth: model.unsure ? 1 : 0.5))
    }

    /// Concept 6: which rule applies, where the files go, what happens after.
    @ViewBuilder private var ruleCard: some View {
        if !model.ruleName.isEmpty || !model.destination.isEmpty {
            VStack(alignment: .leading, spacing: 3) {
                if !model.ruleName.isEmpty {
                    (Text("Rule ").foregroundStyle(.secondary) + Text(model.ruleName).fontWeight(.semibold))
                }
                (Text("Saves to ").foregroundStyle(.secondary) + Text(model.destination))
                    .lineLimit(1).truncationMode(.middle)
                if !model.thenLine.isEmpty {
                    (Text("Then ").foregroundStyle(.secondary) + Text(model.thenLine))
                }
            }
            .font(.system(size: 13))
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14).padding(.vertical, 10)
            .background(RoundedRectangle(cornerRadius: 12).fill(.primary.opacity(0.05)))
            .overlay(RoundedRectangle(cornerRadius: 12).stroke(.primary.opacity(0.08), lineWidth: 0.5))
            .accessibilityElement(children: .combine)
        }
    }

    private var quickPicks: some View {
        HStack(spacing: 2) {
            ForEach(model.quickPicks) { p in
                Button { model.apply(p) } label: {
                    Text(p.rawValue).font(.system(size: 13))
                        .padding(.horizontal, 12).padding(.vertical, 5)
                        .background(RoundedRectangle(cornerRadius: 7).fill(model.pick == p ? Color.primary.opacity(0.16) : .clear))
                        .foregroundStyle(model.pick == p ? .primary : .secondary)
                }
                .buttonStyle(.plain)
                .help("Use the \(p.rawValue) preset for what to download")
            }
        }
        .padding(2)
        .background(RoundedRectangle(cornerRadius: 9).fill(.primary.opacity(0.06)))
    }

    private var list: some View {
        ScrollView {
            VStack(spacing: 0) {
                ForEach(Array(model.items.enumerated()), id: \.element.id) { n, item in
                    if n > 0 { Divider().opacity(0.5) }
                    row(item, level: 0)
                    if model.expanded.contains(item.id) {
                        ForEach(item.children) { child in row(child, level: 1) }
                    }
                }
            }
        }
        .frame(height: listHeight)
        .background(RoundedRectangle(cornerRadius: 12).fill(.primary.opacity(0.05)))
        .clipShape(RoundedRectangle(cornerRadius: 12))
    }

    /// A ScrollView has no height of its own: size it to its rows, at most ~7 of them.
    private var listHeight: CGFloat {
        let h = model.items.reduce(CGFloat(0)) { $0 + 41 + (model.expanded.contains($1.id) ? CGFloat($1.children.count) * 34 : 0) }
        return min(330, h + 4)
    }

    private func row(_ item: ChoiceItem, level: Int) -> some View {
        let state = model.state(item)
        let dim = item.inLibrary && state == .off
        return HStack(spacing: 10) {
            if level == 0 {
                FoldChevron(open: model.expanded.contains(item.id), size: 10, width: 28, height: 28,
                            help: model.expanded.contains(item.id) ? "Hide the contents" : "Show the contents") {
                    model.toggleExpanded(item)
                }
                .opacity(item.children.isEmpty ? 0 : 1).allowsHitTesting(!item.children.isEmpty)
            } else {
                Spacer().frame(width: 44)
            }
            TriCheck(state: state) { model.toggle(item) }
            // Only the title area ticks the row: the chevron and the box have their own buttons, and
            // a click that misses their glyphs must not fall through to this tap.
            HStack(spacing: 10) {
                Text(item.title).font(.system(size: 14))
                    .foregroundStyle(dim || item.isExtras ? .secondary : .primary)
                if !item.detail.isEmpty {
                    Text(item.detail).font(.system(size: 13)).foregroundStyle(.secondary)
                }
                if item.inLibrary {
                    Text(item.badge).font(.system(size: 12)).foregroundStyle(Color.green)
                        .padding(.horizontal, 8).padding(.vertical, 2)
                        .background(Capsule().fill(Color.green.opacity(0.16)))
                }
                Spacer()
                Text(Fmt.bytes(item.size)).font(.system(size: 13)).foregroundStyle(.secondary).monospacedDigit()
            }
            .frame(maxHeight: .infinity)
            .contentShape(Rectangle())
            .onTapGesture { model.toggle(item) }
        }
        .padding(.horizontal, 12).padding(.vertical, level == 0 ? 6 : 6)
        .frame(minHeight: level == 0 ? 40 : 34)
    }

    private var footer: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(model.totalsLine)
                    .font(.system(size: 13)).foregroundStyle(model.notEnoughSpace ? Color.orange : .secondary).monospacedDigit()
                if model.notEnoughSpace { Text("Not enough space").font(.system(size: 12)).foregroundStyle(Color.orange) }
                Spacer()
                if model.awaiting {
                    HeldButtons(hasSourceFile: hasSourceFile, canStart: model.canConfirm,
                                startHelp: "Start downloading the ticked files", cancel: cancel, start: confirm)
                        .fixedSize()
                } else {
                    Button("Cancel", role: .cancel) { cancel(false) }.keyboardShortcut(.cancelAction)
                        .help("Close without changing anything")
                    Button(model.primaryTitle, action: confirm)
                        .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction).disabled(!model.canConfirm)
                        .help("Download the files ticked now")
                }
            }
        }
    }
}

/// A checkbox that can also be partly on (some episodes of a season).
struct TriCheck: View {
    let state: TriState
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            ZStack {
                RoundedRectangle(cornerRadius: 5)
                    .fill(state == .off ? Color.clear : Color.accentColor)
                    .frame(width: 18, height: 18)
                RoundedRectangle(cornerRadius: 5)
                    .stroke(state == .off ? Color.primary.opacity(0.35) : Color.clear, lineWidth: 1)
                    .frame(width: 18, height: 18)
                switch state {
                case .on: Image(systemName: "checkmark").font(.system(size: 10, weight: .bold)).foregroundStyle(.white)
                case .mixed: Image(systemName: "minus").font(.system(size: 10, weight: .bold)).foregroundStyle(.white)
                case .off: EmptyView()
                }
            }
            // An unticked box is a clear fill and a hairline: without a shape the click that should
            // tick it falls through (hit-testing ignores clear pixels), so "select none, then pick one" did nothing.
            .frame(width: 22, height: 22)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(state == .on ? "Skip this" : "Download this")
        .accessibilityValue(state == .on ? "checked" : state == .mixed ? "partly checked" : "unchecked")
        .accessibilityAddTraits(.isButton)
    }
}

/// Opens chooser windows (SPEC §7A.6) and answers the watch folders' "ask first" requests.
@MainActor final class ChooserPresenter: NSObject, NSWindowDelegate {
    private var windows: [String: NSWindow] = [:]
    /// The Change ▸ category action of each open chooser (also driven by the QA harness).
    private var changers: [String: (Category) -> Void] = [:]

    /// QA only: the first open chooser window.
    var qaWindow: NSWindow? { windows.values.first }

    /// QA only: Change ▸ <category> in the first open chooser.
    func qaChange(to c: Category) { changers.values.first?(c) }

    /// A torrent that waits for a choice, or whose files the user wants to change.
    func show(id: String, commands: Commands?, reopen: Bool = false) {
        guard let commands else { return }
        if let w = windows[id] { NSApp.activate(); w.makeKeyAndOrderFront(nil); return }
        // The model is the notifier popover's too (SPEC 8.6): ticks made in one show in the other.
        guard let model = ChoiceStore.shared.model(for: id, reopen: reopen || !isAwaiting(id, commands)) else {
            commands.showMain(nil, id)
            return
        }
        present(model, id: id, commands: commands)
    }

    private func isAwaiting(_ id: String, _ commands: Commands) -> Bool {
        commands.core.snapshot().downloading.first { $0.id == id }?.awaitingChoice ?? false
    }

    func present(_ model: ChooserModel, id: String, commands: Commands) {
        // The pill's menu in the popover does the same: the engine re-plans, the shared model is rebuilt.
        let change: (Category) -> Void = { c in ChoiceStore.shared.changeCategory(id, to: c) }
        changers[id] = change
        let view = ChooserView(
            model: model,
            confirm: { [weak self] in
                do {
                    try ChoiceStore.shared.confirm(id)
                    self?.close(id)
                } catch { commands.alert("Couldn't start", "\(error)") }
            },
            cancel: { [weak self] keepFile in
                if model.awaiting {
                    let a = NSAlert()
                    a.messageText = "Remove “\(model.name)”?"
                    let source = ChoiceStore.shared.hasSource(id)
                    a.informativeText = "Nothing has been downloaded. It is removed from \(AppInfo.name)."
                        + (source ? (keepFile ? " Its .torrent file stays where it is." : " Its .torrent file moves to the Trash.") : "")
                    a.addButton(withTitle: "Remove")
                    a.addButton(withTitle: "Keep Waiting")
                    if a.runModal() == .alertFirstButtonReturn {
                        try? ChoiceStore.shared.cancel(id, keepFile: keepFile)
                        self?.close(id)
                    }
                } else {
                    self?.close(id)
                }
            },
            hasSourceFile: model.awaiting && ChoiceStore.shared.hasSource(id),
            changeCategory: change)
        let host = NSHostingController(rootView: view)
        let w = NSWindow(contentViewController: host)
        w.styleMask = [.titled, .closable, .fullSizeContentView]
        w.titleVisibility = .hidden
        w.titlebarAppearsTransparent = true
        w.isReleasedWhenClosed = false
        w.colorSpace = Windows.space
        w.delegate = self
        w.identifier = NSUserInterfaceItemIdentifier(id)
        w.center()
        windows[id] = w
        NSApp.activate()
        w.makeKeyAndOrderFront(nil)
    }

    func close(_ id: String) {
        windows[id]?.delegate = nil
        windows[id]?.contentViewController = nil
        windows[id]?.close()
        windows[id] = nil
        changers[id] = nil
        // A running torrent's Files chooser is not shared with the popover: forget it with the window.
        if !isShared(id) { ChoiceStore.shared.drop(id) }
        MemoryRelief.schedule()
    }

    private func isShared(_ id: String) -> Bool { ChoiceStore.shared.models[id]?.awaiting == true }

    func windowWillClose(_ notification: Notification) {
        guard let w = notification.object as? NSWindow, let id = w.identifier?.rawValue else { return }
        windows[id] = nil
        changers[id] = nil
        if !isShared(id) { ChoiceStore.shared.drop(id) }
        w.contentViewController = nil
        MemoryRelief.schedule()
    }

    /// A watch folder in "Ask first" mode found a torrent: ask before adding it.
    func showAddRequest(_ r: AddRequestInfo, commands: Commands) {
        let a = NSAlert()
        a.messageText = "Add “\(r.name)”?"
        a.informativeText = "\(r.category.label) · \(Fmt.bytes(r.totalBytes))" + (r.tracker.isEmpty ? "" : " · \(r.tracker)")
        a.addButton(withTitle: "Add")
        a.addButton(withTitle: "Don't Add")
        NSApp.activate()
        if a.runModal() == .alertFirstButtonReturn { commands.add(r.path) }
    }

    func closeAll() {
        for (_, w) in windows { w.delegate = nil; w.close() }
        windows = [:]
        changers = [:]
    }
}

/// The disclosure chevron of a fold: a real 28 x 28 hit area (a thin glyph alone misses most clicks, and
/// a miss would fall through to the row under it). Only folds or unfolds; the caller never ticks.
struct FoldChevron: View {
    let open: Bool
    var size: CGFloat = 10
    var width: CGFloat = 28
    var height: CGFloat = 28
    var help = ""
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Image(systemName: "chevron.right").font(.system(size: size, weight: .semibold))
                .rotationEffect(.degrees(open ? 90 : 0))
                .foregroundStyle(.secondary)
                .frame(width: width, height: height)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(help)
        .accessibilityLabel(open ? "Collapse" : "Expand")
    }
}
