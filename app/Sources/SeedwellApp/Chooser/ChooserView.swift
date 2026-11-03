import SwiftUI

/// Choose what to download (concept 10, SPEC §7A.6), as the core lays it out (`Chooser.view`): structure groups with
/// tri-state checkboxes, "In library" badges, quick picks, a footer with size and free space, and Cancel and Start.
/// Nothing is downloaded or announced before it is confirmed. Every tick goes to the shared chooser in the core.
struct ChooserView: View {
    let chooser: Chooser
    /// Cancel would move a `.torrent` file to the Trash, so ⇧ has something to keep.
    var hasSourceFile = false
    var confirm: () -> Void = {}
    /// The flag is ⇧-Cancel: keep the `.torrent` file (SPEC 7A.6).
    var cancel: (_ keepFile: Bool) -> Void = { _ in }
    var changeCategory: (Category) -> Void = { _ in }

    var body: some View {
        // Read again whenever a chooser changed (here, in the notifier or in the window's list).
        let _ = ChoiceStore.shared.revision
        let page = chooser.view(hasSourceFile: hasSourceFile)
        VStack(alignment: .leading, spacing: 0) {
            header(page)
            categoryCard(page).padding(.top, 14)
            ruleCard(page).padding(.top, 8)
            if let notice = page.notice {
                Label {
                    Text(notice).font(Theme.body).fixedSize(horizontal: false, vertical: true)
                } icon: {
                    Image(systemName: page.unsure ? "questionmark.circle.fill" : "checkmark.circle.fill")
                        .foregroundStyle(page.unsure ? Color.orange : Color.green)
                }
                .padding(.top, 12)
            }
            if !page.picks.isEmpty { quickPicks(page).padding(.top, 12) }
            list(page).padding(.top, 10)
            footer(page).padding(.top, 12)
        }
        .padding(22)
        .frame(width: 640)
        .background(.background)
    }

    private func edit(_ body: (Chooser) -> Void) {
        ChoiceStore.shared.edit { body(chooser) }
    }

    private func header(_ page: ChooserPage) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(page.origin.uppercased()).font(Theme.caption.weight(.semibold)).tracking(0.5).foregroundStyle(.secondary)
            Text(page.name).font(Theme.title).lineLimit(2)
            Text(page.summary).font(Theme.body).foregroundStyle(.secondary)
        }
    }

    private func categoryCard(_ page: ChooserPage) -> some View {
        HStack(spacing: 12) {
            Image(systemName: page.category.symbol)
                .font(.system(size: Theme.Icon.large)).frame(width: 46, height: 46)
                .background(RoundedRectangle(cornerRadius: 10).fill(.primary.opacity(0.08)))
            VStack(alignment: .leading, spacing: 2) {
                Text(page.categoryTitle).font(Theme.heading).lineLimit(1)
                Text(page.categoryDetail).font(Theme.secondary).foregroundStyle(.secondary)
            }
            Spacer()
            Menu(page.changeTitle) {
                ForEach(page.categories, id: \.title) { c in
                    Toggle(c.title, isOn: Binding(get: { c.checked }, set: { _ in changeCategory(c.category) }))
                }
            }
            .menuStyle(.button).fixedSize()
            .help(page.changeHelp)
        }
        .padding(10)
        .background(RoundedRectangle(cornerRadius: 12).fill((page.unsure ? Color.orange : Color.accentColor).opacity(0.14)))
        .overlay(RoundedRectangle(cornerRadius: 12).stroke((page.unsure ? Color.orange : Color.accentColor).opacity(0.35), lineWidth: page.unsure ? 1 : 0.5))
    }

    /// Concept 6: which rule applies, where the files go, what happens after.
    @ViewBuilder private func ruleCard(_ page: ChooserPage) -> some View {
        if !page.ruleLines.isEmpty {
            VStack(alignment: .leading, spacing: 3) {
                ForEach(page.ruleLines, id: \.label) { line in
                    (Text(line.label).foregroundStyle(.secondary) + Text(line.value).fontWeight(line.emphasized ? .semibold : nil))
                        .lineLimit(line.emphasized ? nil : 1).truncationMode(.middle)
                }
            }
            .font(Theme.body)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14).padding(.vertical, 10)
            .background(RoundedRectangle(cornerRadius: 12).fill(.primary.opacity(0.05)))
            .overlay(RoundedRectangle(cornerRadius: 12).stroke(.primary.opacity(0.08), lineWidth: 0.5))
            .accessibilityElement(children: .combine)
        }
    }

    private func quickPicks(_ page: ChooserPage) -> some View {
        HStack(spacing: 2) {
            ForEach(page.picks, id: \.title) { p in
                Button { edit { $0.applyPick(pick: p.pick) } } label: {
                    Text(p.title).font(Theme.body)
                        .padding(.horizontal, 12).padding(.vertical, 5)
                        .background(RoundedRectangle(cornerRadius: 7).fill(p.selected ? Color.primary.opacity(0.16) : .clear))
                        .foregroundStyle(p.selected ? .primary : .secondary)
                }
                .buttonStyle(.plain)
                .help(p.help)
            }
        }
        .padding(2)
        .background(RoundedRectangle(cornerRadius: 9).fill(.primary.opacity(0.06)))
    }

    private func list(_ page: ChooserPage) -> some View {
        ScrollView {
            VStack(spacing: 0) {
                ForEach(Array(page.rows.enumerated()), id: \.element.id) { n, item in
                    if n > 0, item.level == 0 { Divider().opacity(0.5) }
                    row(item)
                }
            }
        }
        .frame(height: listHeight(page))
        .background(RoundedRectangle(cornerRadius: 12).fill(.primary.opacity(0.05)))
        .clipShape(RoundedRectangle(cornerRadius: 12))
    }

    /// A ScrollView has no height of its own: size it to its rows, at most ~7 of them.
    private func listHeight(_ page: ChooserPage) -> CGFloat {
        let h = page.rows.reduce(CGFloat(0)) { $0 + ($1.level == 0 ? 41 : 34) }
        return min(330, h + 4)
    }

    private func row(_ item: ChooserRow) -> some View {
        HStack(spacing: 10) {
            if item.level == 0 {
                FoldChevron(open: item.open, size: Theme.Size.micro, width: 28, height: 28, help: item.foldHelp, label: item.foldLabel) {
                    edit { $0.toggleFolded(id: item.id) }
                }
                .opacity(item.foldable ? 1 : 0).allowsHitTesting(item.foldable)
            } else {
                Spacer().frame(width: 44)
            }
            TriCheck(state: item.check, help: item.checkHelp, value: item.checkValue) { edit { $0.toggle(id: item.id) } }
            // Only the title area ticks the row: the chevron and the box have their own buttons, and
            // a click that misses their glyphs must not fall through to this tap.
            HStack(spacing: 10) {
                Text(item.title).font(Theme.body)
                    .foregroundStyle(item.dimmed ? .secondary : .primary)
                if !item.detail.isEmpty {
                    Text(item.detail).font(Theme.body).foregroundStyle(.secondary)
                }
                if let badge = item.badge {
                    Text(badge).font(Theme.secondary).foregroundStyle(Color.green)
                        .padding(.horizontal, 8).padding(.vertical, 2)
                        .background(Capsule().fill(Color.green.opacity(0.16)))
                }
                Spacer()
                Text(item.size).font(Theme.body).foregroundStyle(.secondary).monospacedDigit()
            }
            .frame(maxHeight: .infinity)
            .contentShape(Rectangle())
            .onTapGesture { edit { $0.toggle(id: item.id) } }
        }
        .padding(.horizontal, 12).padding(.vertical, 6)
        .frame(minHeight: item.level == 0 ? 40 : 34)
    }

    private func footer(_ page: ChooserPage) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(page.totalsLine)
                    .font(Theme.body).foregroundStyle(page.notEnoughSpace ? Color.orange : .secondary).monospacedDigit()
                if page.notEnoughSpace { Text(page.spaceWarning).font(Theme.secondary).foregroundStyle(Color.orange) }
                Spacer()
                if page.awaiting {
                    HeldButtons(actions: page.buttons, cancel: cancel, start: confirm)
                        .fixedSize()
                } else {
                    Button(page.buttons.cancel.title, role: .cancel) { cancel(false) }.keyboardShortcut(.cancelAction)
                        .help(page.buttons.cancel.help)
                    if let primary = page.buttons.start {
                        Button(primary.title, action: confirm)
                            .buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction).disabled(!primary.enabled)
                            .help(primary.help)
                    }
                }
            }
        }
    }
}

/// A checkbox that can also be partly on (some episodes of a season), with the core's tooltip and screen reader value.
struct TriCheck: View {
    let state: FileCheck
    var help = ""
    var value = ""
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
                case .on: Image(systemName: "checkmark").font(Theme.micro.weight(.bold)).foregroundStyle(.white)
                case .mixed: Image(systemName: "minus").font(Theme.micro.weight(.bold)).foregroundStyle(.white)
                case .off: EmptyView()
                }
            }
            // An unticked box is a clear fill and a hairline: without a shape the click that should
            // tick it falls through (hit-testing ignores clear pixels), so "select none, then pick one" did nothing.
            .frame(width: 22, height: 22)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(help)
        .accessibilityValue(value)
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
        // The chooser is the notifier popover's too (SPEC 8.6): ticks made in one show in the other. The engine works it
        // out (a look at the library, the free space) off the main thread; the window opens when it is there.
        let reopen = reopen || !isAwaiting(id, commands)
        Task { [weak self] in
            let hasSource = await ChoiceStore.shared.hasSourceFile(id)
            guard let self, windows[id] == nil else { return }
            guard let chooser = await ChoiceStore.shared.load(id, reopen: reopen) else {
                commands.showMain(nil, id)
                return
            }
            guard windows[id] == nil else { return }
            present(chooser, id: id, commands: commands, hasSource: hasSource)
        }
    }

    private func isAwaiting(_ id: String, _ commands: Commands) -> Bool {
        commands.core.snapshot().allDownloading.first { $0.id == id }?.awaitingChoice ?? false
    }

    func present(_ chooser: Chooser, id: String, commands: Commands, hasSource: Bool? = nil) {
        let store = ChoiceStore.shared
        // The pill's menu in the popover does the same: the engine re-plans, the shared chooser is rebuilt.
        let change: (Category) -> Void = { c in store.changeCategory(id, to: c) }
        changers[id] = change
        let awaiting = chooser.awaiting()
        let view = ChooserView(
            chooser: chooser,
            hasSourceFile: awaiting && (hasSource ?? store.hasSource(id)),
            confirm: {
                // The window closes once the engine took the ticks (`didResolve`); a start it refused is said in an alert
                // and the window stays.
                store.confirm(id) { error in commands.alert("Couldn't start", "\(error)") }
            },
            cancel: { [weak self] keepFile in
                if chooser.awaiting() {
                    let name = chooser.view(hasSourceFile: false).name
                    let q = cancelQuestion(name: name, hasSourceFile: store.hasSource(id), keepFile: keepFile)
                    let a = NSAlert()
                    a.messageText = q.title
                    a.informativeText = q.text
                    a.addButton(withTitle: q.removeTitle)
                    a.addButton(withTitle: q.keepTitle)
                    if a.runModal() == .alertFirstButtonReturn {
                        store.cancel(id, keepFile: keepFile)
                        self?.close(id)
                    }
                } else {
                    self?.close(id)
                }
            },
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

    private func isShared(_ id: String) -> Bool { ChoiceStore.shared.loaded(id)?.awaiting() == true }

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
        let p = addRequestPrompt(request: r)
        let a = Self.addRequestAlert(p)
        NSApp.activate()
        if a.runModal() == .alertFirstButtonReturn { commands.add(p.path) }
    }

    static func addRequestAlert(_ p: AddRequestPrompt) -> NSAlert {
        let a = NSAlert()
        a.messageText = p.title
        a.informativeText = p.text
        a.addButton(withTitle: p.addTitle)
        a.addButton(withTitle: p.dontAddTitle)
        return a
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
    var size: CGFloat = Theme.Size.micro
    var width: CGFloat = 28
    var height: CGFloat = 28
    var help = ""
    /// What a screen reader calls it ("Collapse" / "Expand", the core's words).
    let label: String
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
        .accessibilityLabel(label)
    }
}
