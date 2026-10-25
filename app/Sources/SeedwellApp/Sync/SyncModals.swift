import AppKit
import SwiftUI

// The add and edit modals of folder sync (SPEC 7G.5): small standalone windows, centred, in the chooser's style,
// with Cancel and the primary action at the bottom right.

/// The add modal's state: what is chosen or pasted, and Start.
@MainActor @Observable final class SyncAddModel {
    @ObservationIgnored let core: Core
    var form = SyncAddForm()
    var error: String?
    var busy = false
    /// The default place for the pasted key (`~/Seedwell/<name>`), offered first in the Location popup.
    private(set) var defaultLocation: String?

    init(core: Core, prefill: String? = nil) {
        self.core = core
        if let prefill { form.mode = .shared; setKey(prefill) }
    }

    func setKey(_ text: String) {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        let info = t.isEmpty ? nil : core.syncParseLink(text: t)
        defaultLocation = info == nil ? nil : core.syncJoinPath(keyOrLink: t)
        form.setKey(text, info: info, defaultLocation: defaultLocation)
        error = nil
    }

    func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Choose"
        panel.message = "Choose a folder to keep the same on your devices"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        form.folder = url.path
        error = nil
    }

    /// Where a shared folder goes: a parent folder; the folder itself is named after the key.
    func chooseLocation() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Choose"
        panel.message = "Choose where the shared folder goes"
        let start = (form.location as NSString).deletingLastPathComponent
        if FileManager.default.fileExists(atPath: start) { panel.directoryURL = URL(fileURLWithPath: start) }
        guard panel.runModal() == .OK, let url = panel.url else { return }
        form.chooseParent(url.path)
    }

    /// Start: shares the folder, or joins the pasted one. `done` gets the new folder.
    func start(done: @escaping (SyncFolder, SyncAddForm.Mode) -> Void) {
        guard form.canStart, !busy else { return }
        busy = true
        error = nil
        let (core, form) = (core, form)
        Task {
            let r: Result<SyncFolder, Error> = await Task.detached(priority: .userInitiated) {
                Result {
                    switch form.mode {
                    case .local: try core.syncAddFolder(path: form.folder)
                    case .shared: try core.syncJoin(keyOrLink: form.key.trimmingCharacters(in: .whitespacesAndNewlines), path: form.location)
                    }
                }
            }.value
            busy = false
            switch r {
            case .success(let f): done(f, form.mode)
            case .failure(let e): error = SyncText.message(e)
            }
        }
    }
}

/// The quiet section label of the modals ("FOLDER SYNC", "SHARE").
struct ModalCaption: View {
    let text: String
    var body: some View {
        Text(text.uppercased()).font(.system(size: 11, weight: .semibold)).tracking(0.5).foregroundStyle(.secondary)
    }
}

/// An inline error, in the modal's own words.
struct ModalError: View {
    let text: String
    var body: some View {
        Label { Text(text).fixedSize(horizontal: false, vertical: true) } icon: { Image(systemName: "exclamationmark.triangle.fill") }
            .font(.system(size: 12)).foregroundStyle(.red)
            .accessibilityIdentifier("sync-modal-error")
    }
}

/// Cancel and the primary action, right-aligned on the bottom line (the app's one place for them).
struct ModalButtons: View {
    let primary: String
    var enabled = true
    var busy = false
    var primaryHelp = ""
    let cancel: () -> Void
    let confirm: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            if busy { ProgressView().controlSize(.small) }
            Spacer(minLength: 0)
            Button("Cancel", role: .cancel, action: cancel).keyboardShortcut(.cancelAction).help("Close without changing anything")
            Button(primary, action: confirm).buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
                .disabled(!enabled || busy).help(primaryHelp)
        }
    }
}

struct SyncAddView: View {
    @Bindable var model: SyncAddModel
    var cancel: () -> Void = {}
    var done: (SyncFolder, SyncAddForm.Mode) -> Void = { _, _ in }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 3) {
                ModalCaption(text: "Folder Sync")
                Text("Add Folder").font(.system(size: 17, weight: .semibold))
            }
            Picker("", selection: Binding(get: { model.form.mode }, set: { model.form.mode = $0; model.error = nil })) {
                ForEach(SyncAddForm.Mode.allCases) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.segmented).labelsHidden()
            .padding(.top, 16)
            Group {
                switch model.form.mode {
                case .local: local
                case .shared: shared
                }
            }
            .padding(.top, 18)
            if let e = model.error { ModalError(text: e).padding(.top, 14) }
            ModalButtons(primary: "Start", enabled: model.form.canStart, busy: model.busy,
                         primaryHelp: model.form.mode == .local ? "Start syncing this folder" : "Join the shared folder",
                         cancel: cancel, confirm: { model.start(done: done) })
                .padding(.top, 22)
        }
        .padding(22)
        .frame(width: 480)
        .background(.background)
    }

    private var local: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                Text("Folder").frame(width: 82, alignment: .leading)
                Text(model.form.folder.isEmpty ? "No folder chosen" : Notifier.abbreviate(model.form.folder))
                    .foregroundStyle(model.form.folder.isEmpty ? .secondary : .primary)
                    .lineLimit(1).truncationMode(.middle)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Button("Choose…", action: model.chooseFolder).help("Pick the folder to sync")
            }
            Text("A new key is made for it. After Start you can copy it, or a link, to your other devices.")
                .font(.system(size: 12)).foregroundStyle(.secondary)
                .padding(.leading, 92)
                .fixedSize(horizontal: false, vertical: true)
        }
        .font(.system(size: 13))
    }

    private var shared: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Text("Key or link").frame(width: 82, alignment: .leading)
                VStack(alignment: .leading, spacing: 5) {
                    TextField("SWW-… or seedwell://sync/…", text: Binding(get: { model.form.key }, set: { model.setKey($0) }))
                        .textFieldStyle(.roundedBorder)
                        .accessibilityIdentifier("sync-key-field")
                    if !model.form.keyLine.isEmpty {
                        Label(model.form.keyLine, systemImage: model.form.keyUsable ? "checkmark.circle.fill" : "xmark.circle")
                            .font(.system(size: 12))
                            .foregroundStyle(model.form.keyUsable ? Color.secondary : Color.orange)
                            .accessibilityIdentifier("sync-key-line")
                    }
                }
            }
            HStack(spacing: 10) {
                Text("Location").frame(width: 82, alignment: .leading)
                Picker("", selection: Binding(get: { model.form.location }, set: { v in
                    if v == Self.choose { model.chooseLocation() } else { model.form.location = v; model.form.locationChosen = v != model.defaultLocation }
                })) {
                    if locations.isEmpty { Text("Paste a key first").tag("") }
                    ForEach(locations, id: \.self) { p in Text(Notifier.abbreviate(p)).tag(p) }
                    if !locations.isEmpty { Divider() }
                    Text("Choose…").tag(Self.choose)
                }
                .labelsHidden()
                .disabled(!model.form.keyUsable)
                .help("Where the shared folder goes on this Mac")
            }
        }
        .font(.system(size: 13))
    }

    private static let choose = "\u{1}choose"

    /// The popup's places: the default for this key, and the one the user chose.
    private var locations: [String] {
        var out: [String] = []
        if let d = model.defaultLocation { out.append(d) }
        if !model.form.location.isEmpty, !out.contains(model.form.location) { out.append(model.form.location) }
        return out
    }
}

/// The edit modal's state. Name and location apply on Save; copying a key acts at once.
@MainActor @Observable final class SyncEditModel {
    @ObservationIgnored let core: Core
    let id: String
    private(set) var folder: SyncFolder?
    var name = ""
    /// The new parent folder chosen with Move… (the folder keeps its name).
    var newParent: String?
    var paused = false
    var lanOnly = false
    var optionsOpen = false
    private(set) var shares: SyncShares?
    var error: String?
    var busy = false
    /// "Read & write key" etc., shown briefly after a copy.
    var copied: String?
    @ObservationIgnored private var copiedTask: Task<Void, Never>?

    init(core: Core, id: String) {
        self.core = core
        self.id = id
        reload()
    }

    func reload() {
        folder = core.syncSnapshot().folders.first { $0.id == id }
        if let f = folder {
            name = f.name
            paused = f.state == .paused
            lanOnly = f.lanOnly
        }
        shares = try? core.syncShares(id: id)
    }

    var parent: String { newParent ?? ((folder?.path ?? "") as NSString).deletingLastPathComponent }
    var target: String { (parent as NSString).appendingPathComponent(name.trimmingCharacters(in: .whitespaces)) }
    var changed: Bool { folder.map { target != $0.path || paused != ($0.state == .paused) || lanOnly != $0.lanOnly } ?? false }

    nonisolated static func nameProblem(_ name: String) -> String? {
        let n = name.trimmingCharacters(in: .whitespaces)
        if n.isEmpty { return "Give the folder a name." }
        if n.contains("/") || n.contains(":") { return "A folder name cannot contain “/” or “:”." }
        if n.hasPrefix(".") { return "A folder name cannot start with a dot." }
        return nil
    }

    func move() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "Move Here"
        panel.message = "Choose where “\(name)” goes. It keeps its name."
        if FileManager.default.fileExists(atPath: parent) { panel.directoryURL = URL(fileURLWithPath: parent) }
        guard panel.runModal() == .OK, let url = panel.url else { return }
        newParent = url.path
        error = nil
    }

    func copy(_ text: String?, what: String, actions: SyncActions) {
        guard let text else { return }
        actions.copy(text)
        copied = what
        copiedTask?.cancel()
        copiedTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(2))
            if !Task.isCancelled { self?.copied = nil }
        }
    }

    func rescan() {
        do { try core.syncRescan(id: id) } catch { self.error = SyncText.message(error) }
    }

    func save(done: @escaping () -> Void) {
        guard let f = folder, !busy else { return }
        if let p = Self.nameProblem(name) { error = p; return }
        let (core, id, target, newName, moved) = (core, id, target, name.trimmingCharacters(in: .whitespaces), newParent != nil)
        let pause = paused != (f.state == .paused) ? Optional(paused) : nil
        let lan = lanOnly != f.lanOnly ? Optional(lanOnly) : nil
        let place = target != f.path
        busy = true
        error = nil
        Task {
            let r: Result<Void, Error> = await Task.detached(priority: .userInitiated) {
                Result {
                    if place, moved { _ = try core.syncMove(id: id, path: target) }
                    else if place { _ = try core.syncRename(id: id, name: newName) }
                    if let pause { try core.syncPause(id: id, paused: pause) }
                    if let lan { try core.syncSetLanOnly(id: id, lanOnly: lan) }
                }
            }.value
            busy = false
            switch r {
            case .success: done()
            case .failure(let e): error = SyncText.message(e); reload()
            }
        }
    }
}

struct SyncEditView: View {
    @Bindable var model: SyncEditModel
    let actions: SyncActions
    var cancel: () -> Void = {}
    var done: () -> Void = {}

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 3) {
                ModalCaption(text: "Folder Sync")
                Text(model.folder?.name ?? "Folder").font(.system(size: 17, weight: .semibold)).lineLimit(1).truncationMode(.middle)
                if let f = model.folder {
                    Text("\(SyncText.access(f.access)) · \(SyncText.subtitle(f))").font(.system(size: 13)).foregroundStyle(.secondary)
                }
            }
            if model.folder == nil {
                Text("This folder is no longer synced on this Mac.").foregroundStyle(.secondary).padding(.top, 16)
            } else {
                form.padding(.top, 18)
            }
            if let e = model.error { ModalError(text: e).padding(.top, 14) }
            ModalButtons(primary: "Save", enabled: model.folder != nil, busy: model.busy, primaryHelp: "Apply the new name, place and options",
                         cancel: cancel, confirm: { model.changed ? model.save(done: done) : done() })
                .padding(.top, 22)
        }
        .padding(22)
        .frame(width: 500)
        .background(.background)
    }

    private var form: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 10) {
                Text("Name").frame(width: 82, alignment: .leading)
                TextField("", text: $model.name).textFieldStyle(.roundedBorder)
                    .help("Renames the folder on this Mac only. Other devices keep their own name.")
                    .accessibilityIdentifier("sync-name-field")
            }
            HStack(spacing: 10) {
                Text("Location").frame(width: 82, alignment: .leading)
                Text(Notifier.abbreviate(model.parent))
                    .foregroundStyle(model.newParent == nil ? .secondary : .primary)
                    .lineLimit(1).truncationMode(.middle)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .help(model.target)
                Button("Move…", action: model.move).help("Choose another folder for it on this Mac; it keeps its name")
            }
            Divider().padding(.vertical, 2)
            VStack(alignment: .leading, spacing: 8) {
                HStack {
                    ModalCaption(text: "Share")
                    Spacer()
                    if let c = model.copied {
                        Label("Copied \(c)", systemImage: "checkmark").font(.system(size: 11.5)).foregroundStyle(.secondary)
                            .transition(.opacity)
                    }
                }
                if let s = model.shares {
                    if s.readWrite != nil {
                        shareRow("Read & write", note: "Can change files", key: s.readWrite, link: s.readWriteLink)
                    }
                    if s.readOnly != nil {
                        shareRow("Read only", note: "Receives changes, never sends its own", key: s.readOnly, link: s.readOnlyLink)
                    }
                    Text("Anyone with a key or link can sync this folder. Send it only to people and devices you trust.")
                        .font(.system(size: 11.5)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
            }
            .animation(.easeOut(duration: 0.15), value: model.copied)
            Divider().padding(.vertical, 2)
            DisclosureGroup("Options", isExpanded: $model.optionsOpen) {
                VStack(alignment: .leading, spacing: 10) {
                    Toggle("Pause syncing", isOn: $model.paused)
                        .toggleStyle(.switch).controlSize(.small)
                        .help("Stop sending and receiving changes for this folder until you resume")
                    Toggle("Only sync on the local network", isOn: $model.lanOnly)
                        .toggleStyle(.switch).controlSize(.small)
                        .help("Sync this folder only with devices on the same network as this Mac, never over the Internet")
                    HStack(spacing: 10) {
                        Button("Rescan Now", action: model.rescan).help("Look at every file of the folder again now")
                        Text("Changes are seen within seconds; a full scan also runs every hour.")
                            .font(.system(size: 11.5)).foregroundStyle(.secondary)
                    }
                }
                .padding(.top, 10).padding(.leading, 4)
            }
        }
        .font(.system(size: 13))
    }

    private func shareRow(_ title: String, note: String, key: String?, link: String?) -> some View {
        HStack(spacing: 10) {
            VStack(alignment: .leading, spacing: 1) {
                Text(title)
                Text(note).font(.system(size: 11.5)).foregroundStyle(.secondary)
            }
            Spacer()
            Button("Copy Key") { model.copy(key, what: "\(title) key", actions: actions) }.help("Copy the \(title) key")
            Button("Copy Link") { model.copy(link, what: "\(title) link", actions: actions) }.help("Copy a seedwell:// link with the \(title) key")
        }
    }
}

/// The add and edit windows: one add window, one edit window per folder. The menu closes before they open.
@MainActor final class SyncWindows: NSObject, NSWindowDelegate {
    private var add: (NSWindow, SyncAddModel)?
    private var edits: [String: NSWindow] = [:]
    private var finder: NSWindow?

    var qaAddModel: SyncAddModel? { add?.1 }
    var qaWindow: NSWindow? { add?.0 ?? edits.values.first }

    func showAdd(prefill: String?, actions: SyncActions) {
        if let (w, m) = add {
            if let prefill { m.form.mode = .shared; m.setKey(prefill) }
            NSApp.activate(); w.makeKeyAndOrderFront(nil)
            return
        }
        let model = SyncAddModel(core: actions.core, prefill: prefill ?? actions.pastedKey())
        let view = SyncAddView(model: model, cancel: { [weak self] in self?.closeAdd() }, done: { [weak self] f, mode in
            self?.closeAdd()
            // Its keys are at hand: a folder shared from this Mac opens in the edit modal.
            if mode == .local { self?.showEdit(id: f.id, actions: actions) }
        })
        let w = makeWindow(NSHostingController(rootView: view), id: "sync-add")
        add = (w, model)
        NSApp.activate()
        w.makeKeyAndOrderFront(nil)
    }

    func showEdit(id: String, actions: SyncActions) {
        if let w = edits[id] { NSApp.activate(); w.makeKeyAndOrderFront(nil); return }
        let model = SyncEditModel(core: actions.core, id: id)
        let view = SyncEditView(model: model, actions: actions, cancel: { [weak self] in self?.closeEdit(id) }, done: { [weak self] in self?.closeEdit(id) })
        let w = makeWindow(NSHostingController(rootView: view), id: "sync-edit-\(id)")
        edits[id] = w
        NSApp.activate()
        w.makeKeyAndOrderFront(nil)
    }

    /// Turning on the Finder extension.
    func showFinderSetup() {
        if let w = finder { NSApp.activate(); w.makeKeyAndOrderFront(nil); return }
        let view = FinderSetupView(model: FinderSetupModel(), cancel: { [weak self] in self?.closeFinderSetup() })
        let w = makeWindow(NSHostingController(rootView: view), id: "finder-setup")
        finder = w
        NSApp.activate()
        w.makeKeyAndOrderFront(nil)
    }

    func closeFinderSetup() {
        guard let w = finder else { return }
        finder = nil
        w.delegate = nil
        w.contentViewController = nil
        w.close()
    }

    private func makeWindow<V: View>(_ host: NSHostingController<V>, id: String) -> NSWindow {
        host.sizingOptions = .preferredContentSize
        let w = NSWindow(contentViewController: host)
        w.styleMask = [.titled, .closable, .fullSizeContentView]
        w.titleVisibility = .hidden
        w.titlebarAppearsTransparent = true
        w.isReleasedWhenClosed = false
        w.colorSpace = Windows.space
        w.delegate = self
        w.identifier = NSUserInterfaceItemIdentifier(id)
        w.center()
        return w
    }

    func closeAdd() {
        guard let (w, _) = add else { return }
        add = nil
        w.delegate = nil
        w.contentViewController = nil
        w.close()
    }

    func closeEdit(_ id: String) {
        guard let w = edits.removeValue(forKey: id) else { return }
        w.delegate = nil
        w.contentViewController = nil
        w.close()
    }

    func closeAll() {
        closeAdd()
        closeFinderSetup()
        for id in Array(edits.keys) { closeEdit(id) }
    }

    func windowWillClose(_ notification: Notification) {
        guard let w = notification.object as? NSWindow else { return }
        if add?.0 === w { add = nil }
        if finder === w { finder = nil }
        if let id = edits.first(where: { $0.value === w })?.key { edits[id] = nil }
        w.delegate = nil
        DispatchQueue.main.async { w.contentViewController = nil }
    }
}
