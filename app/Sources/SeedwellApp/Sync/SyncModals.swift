import AppKit
import SwiftUI

// The add and edit modals of folder sync (SPEC 7G.5): small standalone windows, centred, in the chooser's style,
// with Cancel and the primary action at the bottom right.

/// The add modal's state: what is chosen or pasted, and Start. The form, its rules and every word are the core's
/// (`Core::sync_add_*`, trs-ffi `sync_dialogs.rs`).
@MainActor @Observable final class SyncAddModel {
    @ObservationIgnored let core: Core
    var form: SyncAddForm
    var error: String?
    var busy = false

    /// `prefill`: a pasted link (Shared with me).
    init(core: Core, prefill: String? = nil) {
        self.core = core
        form = core.syncAddForm(prefill: prefill)
    }

    /// The dialog as the core draws it from the form.
    var window: SyncAddWindowModel { core.syncAddWindow(form: form, features: hostFeaturesAll()) }

    func setKey(_ text: String) {
        form = core.syncAddSetLink(form: form, text: text)
        error = nil
    }

    /// The link field's text. SwiftUI hands the field's text back while it is being edited (in layout passes, and when
    /// editing ends) also when it did not change: only a change is a new link. Taken as one, it cleared the error of
    /// a failed Start as soon as the field lost focus.
    func editKey(_ text: String) {
        if text != form.key { setKey(text) }
    }

    /// A place picked in the Location popup.
    func setLocation(_ path: String) { form = syncAddSetLocation(form: form, location: path) }

    private func pick(_ p: SyncFolderPicker) -> String? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = p.prompt
        panel.message = p.message
        if let start = p.start { panel.directoryURL = URL(fileURLWithPath: start) }
        guard panel.runModal() == .OK, let url = panel.url else { return nil }
        return url.path
    }

    func chooseFolder() {
        guard let path = pick(window.folderPicker) else { return }
        form.folder = path
        error = nil
    }

    /// Where a shared folder goes: a parent folder; the folder itself is named after the link's name.
    func chooseLocation() {
        guard let path = pick(window.locationPicker) else { return }
        form = syncAddChooseParent(form: form, parent: path)
    }

    /// Start: shares the folder, or joins the pasted one. `done` gets the new folder and whether its Share dialog opens
    /// next.
    func start(done: @escaping (SyncFolder, Bool) -> Void) {
        let w = window
        guard w.canStart, !busy else { return }
        busy = true
        error = nil
        let (core, form) = (core, form)
        Task {
            let r: Result<SyncFolder, Error> = await catching { try await core.syncAddStart(form: form) }
            busy = false
            switch r {
            case .success(let f): done(f, w.shareAfterStart)
            case .failure(let e): error = SyncText.message(e)
            }
        }
    }
}

/// The quiet section label of the modals ("FOLDER SYNC", "SHARE").
struct ModalCaption: View {
    let text: String
    var body: some View {
        Text(text.uppercased()).font(Theme.caption.weight(.semibold)).tracking(0.5).foregroundStyle(.secondary)
    }
}

/// An inline error, in the modal's own words.
struct ModalError: View {
    let text: String
    var body: some View {
        Label { Text(text).fixedSize(horizontal: false, vertical: true) } icon: { Image(systemName: "exclamationmark.triangle.fill") }
            .font(Theme.secondary).foregroundStyle(.red)
            .accessibilityIdentifier("sync-modal-error")
    }
}

/// Cancel and the primary action, right-aligned on the bottom line (the app's one place for them).
struct ModalButtons: View {
    let primary: String
    var enabled = true
    var busy = false
    var primaryHelp = ""
    var cancelTitle = "Cancel"
    var cancelHelp = "Close without changing anything"
    let cancel: () -> Void
    let confirm: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            if busy { ProgressView().controlSize(.small) }
            Spacer(minLength: 0)
            Button(cancelTitle, role: .cancel, action: cancel).keyboardShortcut(.cancelAction).help(cancelHelp)
            Button(primary, action: confirm).buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
                .disabled(!enabled || busy).help(primaryHelp)
        }
    }
}

struct SyncAddView: View {
    @Bindable var model: SyncAddModel
    var cancel: () -> Void = {}
    var done: (SyncFolder, Bool) -> Void = { _, _ in }

    var body: some View {
        let w = model.window
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 3) {
                ModalCaption(text: w.caption)
                Text(w.title).font(Theme.title)
            }
            Picker("", selection: Binding(get: { model.form.mode }, set: { model.form.mode = $0; model.error = nil })) {
                ForEach(w.modes, id: \.mode) { Text($0.title).tag($0.mode) }
            }
            .pickerStyle(.segmented).labelsHidden()
            .padding(.top, 16)
            Group {
                switch model.form.mode {
                case .local: local(w)
                case .shared: shared(w)
                }
            }
            .padding(.top, 18)
            if let label = w.onDemandLabel { onDemand(w, label: label).padding(.top, 14) }
            if let follow = w.follow {
                FollowToggle(line: follow, isOn: $model.form.allDevices, readOnly: $model.form.readOnly).padding(.top, 10).padding(.leading, 92)
            }
            if let e = model.error { ModalError(text: e).padding(.top, 14) }
            ModalButtons(primary: w.confirmTitle, enabled: w.canStart, busy: model.busy, primaryHelp: w.confirmHelp,
                         cancelTitle: w.cancelTitle, cancelHelp: w.cancelHelp, cancel: cancel, confirm: { model.start(done: done) })
                .padding(.top, 22)
        }
        .padding(22)
        .frame(width: 480)
        .background(.background)
    }

    private func local(_ w: SyncAddWindowModel) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                Text(w.folderLabel).frame(width: 82, alignment: .leading)
                Text(w.folderText)
                    .foregroundStyle(w.folderChosen ? .primary : .secondary)
                    .lineLimit(1).truncationMode(.middle)
                    .frame(maxWidth: .infinity, alignment: .leading)
                Button(w.chooseTitle, action: model.chooseFolder).help(w.chooseHelp)
            }
            Text(w.localNote)
                .font(Theme.secondary).foregroundStyle(.secondary)
                .padding(.leading, 92)
                .fixedSize(horizontal: false, vertical: true)
        }
        .font(Theme.body)
    }

    private func shared(_ w: SyncAddWindowModel) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Text(w.linkLabel).frame(width: 82, alignment: .leading)
                VStack(alignment: .leading, spacing: 5) {
                    TextField(w.linkPlaceholder, text: Binding(get: { model.form.key }, set: { model.editKey($0) }))
                        .textFieldStyle(.roundedBorder)
                        .accessibilityIdentifier("sync-key-field")
                    if !w.linkLine.isEmpty {
                        Label(w.linkLine, systemImage: w.linkOk ? "checkmark.circle.fill" : "xmark.circle")
                            .font(Theme.secondary)
                            .foregroundStyle(w.linkOk ? Color.secondary : Color.orange)
                            .accessibilityIdentifier("sync-key-line")
                    }
                }
            }
            HStack(spacing: 10) {
                Text(w.locationLabel).frame(width: 82, alignment: .leading)
                Picker("", selection: Binding(get: { w.location }, set: { v in
                    if v == Self.choose { model.chooseLocation() } else { model.setLocation(v) }
                })) {
                    if w.locations.isEmpty { Text(w.locationPlaceholder).tag("") }
                    ForEach(w.locations, id: \.path) { Text($0.title).tag($0.path) }
                    if !w.locations.isEmpty { Divider() }
                    Text(w.locationChoose).tag(Self.choose)
                }
                .labelsHidden()
                .disabled(!w.locationEnabled)
                .help(w.locationHelp)
            }
            if let note = w.locationNote {
                Text(note).font(Theme.secondary).foregroundStyle(.secondary)
                    .padding(.leading, 92)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("sync-location-note")
            }
        }
        .font(Theme.body)
    }

    /// Files on demand (SPEC 7G.11), and what it means for this way in.
    private func onDemand(_ w: SyncAddWindowModel, label: String) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            Toggle(label, isOn: $model.form.onDemand)
                .toggleStyle(.checkbox).help(w.onDemandHelp ?? "")
                .accessibilityIdentifier("sync-on-demand")
            if let note = w.onDemandNote {
                Text(note)
                    .font(Theme.secondary).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("sync-on-demand-note")
            }
        }
        .font(Theme.body)
        .padding(.leading, 92)
    }

    private static let choose = "\u{1}choose"
}

/// The edit modal's state. Name, location and options apply on Save. The form, its rules and every word are the
/// core's (`Core::sync_edit_*`, trs-ffi `sync_dialogs.rs`).
@MainActor @Observable final class SyncEditModel {
    @ObservationIgnored let core: Core
    let id: String
    var form: SyncEditForm
    /// The Finder's side of on-demand folders (its domain goes first when switching back to full).
    @ObservationIgnored var fileProvider: FileProviderHost?
    var error: String?
    var busy = false

    init(core: Core, id: String) {
        self.core = core
        self.id = id
        form = core.syncEditForm(id: id)
    }

    func reload() { form = core.syncEditForm(id: id) }

    var folder: SyncFolder? { form.folder }
    var name: String { get { form.name } set { form.name = newValue } }
    var paused: Bool { get { form.paused } set { form.paused = newValue } }
    var lanOnly: Bool { get { form.lanOnly } set { form.lanOnly = newValue } }
    var onDemand: Bool { get { form.onDemand } set { form.onDemand = newValue } }
    var allDevices: Bool { get { form.allDevices } set { form.allDevices = newValue } }
    var readOnly: Bool { get { form.readOnly } set { form.readOnly = newValue } }
    var optionsOpen: Bool { get { form.optionsOpen } set { form.optionsOpen = newValue } }

    /// The dialog as the core draws it for what the Mac has (the Share dialog, on-demand folders).
    var window: SyncEditWindowModel { syncEditWindow(form: form, host: hostWindowsAll(), features: hostFeaturesAll()) }
    var changed: Bool { window.changed }
    /// What Files on demand means as it is set now (`nil` for a full folder that stays full).
    var onDemandNote: String? { window.onDemandNote }

    func move() {
        let p = window.movePicker
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = p.prompt
        panel.message = p.message
        if let start = p.start { panel.directoryURL = URL(fileURLWithPath: start) }
        guard panel.runModal() == .OK, let url = panel.url else { return }
        form = syncEditChooseParent(form: form, parent: url.path)
        error = nil
    }

    func rescan() {
        let (core, id) = (core, id)
        Task { if case .failure(let error) = await catching({ try await core.syncRescan(id: id) }) { self.error = SyncText.message(error) } }
    }

    func save(done: @escaping () -> Void) {
        guard folder != nil, !busy else { return }
        let (core, form, fp) = (core, form, fileProvider)
        let turnsOnDemand = form.onDemand && form.folder?.onDemand == false
        busy = true
        error = nil
        Task {
            // (Back to full: the core has the Finder's domain removed first, so the place is an ordinary folder again.)
            let r: Result<SyncFolder, Error> = await catching { try await core.syncEditSave(form: form) }
            if turnsOnDemand, case .success(let now) = r, now.onDemand { await fp?.added(now) }
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
        let w = model.window
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 3) {
                ModalCaption(text: w.caption)
                Text(w.title).font(Theme.title).lineLimit(1).truncationMode(.middle)
                if let s = w.subtitle {
                    Text(s).font(Theme.body).foregroundStyle(.secondary)
                }
            }
            if let gone = w.gone {
                Text(gone).foregroundStyle(.secondary).padding(.top, 16)
            } else {
                form(w).padding(.top, 18)
            }
            if let e = model.error { ModalError(text: e).padding(.top, 14) }
            ModalButtons(primary: w.confirmTitle, enabled: w.canSave, busy: model.busy, primaryHelp: w.confirmHelp,
                         cancelTitle: w.cancelTitle, cancelHelp: w.cancelHelp,
                         cancel: cancel, confirm: { w.changed ? model.save(done: done) : done() })
                .padding(.top, 22)
        }
        .padding(22)
        .frame(width: 500)
        .background(.background)
    }

    private func form(_ w: SyncEditWindowModel) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 10) {
                Text(w.nameLabel).frame(width: 82, alignment: .leading)
                TextField("", text: $model.name).textFieldStyle(.roundedBorder)
                    .help(w.nameHelp)
                    .accessibilityIdentifier("sync-name-field")
            }
            HStack(spacing: 10) {
                Text(w.locationLabel).frame(width: 82, alignment: .leading)
                Text(w.location)
                    .foregroundStyle(w.locationMoved ? .primary : .secondary)
                    .lineLimit(1).truncationMode(.middle)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .help(w.locationHelp)
                // An on-demand folder is where the Finder shows it; it moves once it is full again.
                Button(w.moveTitle, action: model.move).help(w.moveHelp)
                    .disabled(!w.moveEnabled)
            }
            if let label = w.onDemandLabel {
                Divider().padding(.vertical, 2)
                VStack(alignment: .leading, spacing: 5) {
                    Toggle(label, isOn: $model.onDemand)
                        .toggleStyle(.switch).controlSize(.small).help(w.onDemandHelp ?? "")
                        .accessibilityIdentifier("sync-edit-on-demand")
                    if let note = w.onDemandNote {
                        Text(note).font(Theme.secondary).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
            if let follow = w.follow {
                Divider().padding(.vertical, 2)
                FollowToggle(line: follow, isOn: $model.allDevices, readOnly: $model.readOnly)
            }
            Divider().padding(.vertical, 2)
            VStack(alignment: .leading, spacing: 8) {
                ModalCaption(text: w.shareCaption)
                HStack(spacing: 10) {
                    Text(w.shareNote).font(Theme.secondary).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    Spacer(minLength: 8)
                    if let share = w.shareTitle {
                        Button(share) { actions.showShare(model.id) }.help(w.shareHelp)
                            .accessibilityIdentifier("sync-edit-share")
                    }
                }
            }
            Divider().padding(.vertical, 2)
            DisclosureGroup(w.optionsTitle, isExpanded: $model.optionsOpen) {
                VStack(alignment: .leading, spacing: 10) {
                    Toggle(w.pauseLabel, isOn: $model.paused)
                        .toggleStyle(.switch).controlSize(.small)
                        .help(w.pauseHelp)
                    Toggle(w.lanLabel, isOn: $model.lanOnly)
                        .toggleStyle(.switch).controlSize(.small)
                        .help(w.lanHelp)
                    HStack(spacing: 10) {
                        Button(w.rescanTitle, action: model.rescan).help(w.rescanHelp)
                        Text(w.rescanNote)
                            .font(Theme.caption).foregroundStyle(.secondary)
                    }
                }
                .padding(.top, 10).padding(.leading, 4)
            }
        }
        .font(Theme.body)
    }
}

/// "On all my devices" (SPEC §7G.1 "Your devices"): the core's title, its line under it, and whether it may be switched;
/// under it a folder's "Read only" (`line.readOnly`) when the dialog binds one.
struct FollowToggle: View {
    let line: FollowLine
    @Binding var isOn: Bool
    var readOnly: Binding<Bool>? = nil

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            Toggle(line.title, isOn: $isOn)
                .toggleStyle(.switch).controlSize(.small)
                .disabled(!line.enabled)
                .help(line.help)
                .accessibilityIdentifier("sync-all-devices")
            if !line.line.isEmpty {
                Text(line.line).font(Theme.secondary).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            if let ro = line.readOnly, let readOnly {
                VStack(alignment: .leading, spacing: 5) {
                    Toggle(ro.title, isOn: readOnly)
                        .toggleStyle(.switch).controlSize(.small)
                        .disabled(!ro.enabled)
                        .help(ro.help)
                        .accessibilityIdentifier("sync-read-only")
                    if !ro.line.isEmpty {
                        Text(ro.line).font(Theme.secondary).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    }
                }
                .padding(.top, 4).padding(.leading, 16)
            }
        }
        .font(Theme.body)
    }
}

/// The add and edit windows: one add window, one edit window per folder. The menu closes before they open.
@MainActor final class SyncWindows: NSObject, NSWindowDelegate {
    private var add: (NSWindow, SyncAddModel)?
    private var edits: [String: NSWindow] = [:]
    private var finder: NSWindow?
    private var fileProvider: NSWindow?
    private var shares: [String: NSWindow] = [:]

    var qaAddModel: SyncAddModel? { add?.1 }
    var qaWindow: NSWindow? { add?.0 ?? edits.values.first }

    func showAdd(prefill: String?, actions: SyncActions) {
        if let (w, m) = add {
            if let prefill { m.form.mode = .shared; m.setKey(prefill) }
            NSApp.activate(); w.makeKeyAndOrderFront(nil)
            return
        }
        let model = SyncAddModel(core: actions.core, prefill: prefill ?? actions.pastedKey())
        let view = SyncAddView(model: model, cancel: { [weak self] in self?.closeAdd() }, done: { [weak self] f, share in
            self?.closeAdd()
            // On demand: its domain now, and the window that asks the user to allow it when macOS keeps it off.
            if f.onDemand, let fp = actions.fileProvider { Task { await fp.added(f) } }
            // A folder shared from this Mac: the next step is sharing it.
            if share { self?.showShare(id: f.id, actions: actions) }
        })
        let w = makeWindow(NSHostingController(rootView: view), id: "sync-add")
        add = (w, model)
        NSApp.activate()
        w.makeKeyAndOrderFront(nil)
    }

    func showEdit(id: String, actions: SyncActions) {
        if let w = edits[id] { NSApp.activate(); w.makeKeyAndOrderFront(nil); return }
        let model = SyncEditModel(core: actions.core, id: id)
        model.fileProvider = actions.fileProvider
        let view = SyncEditView(model: model, actions: actions, cancel: { [weak self] in self?.closeEdit(id) }, done: { [weak self] in self?.closeEdit(id) })
        let w = makeWindow(NSHostingController(rootView: view), id: "sync-edit-\(id)")
        edits[id] = w
        NSApp.activate()
        w.makeKeyAndOrderFront(nil)
    }

    /// The Share dialog of a folder (one per folder).
    func showShare(id: String, actions: SyncActions) {
        if let w = shares[id] { NSApp.activate(); w.makeKeyAndOrderFront(nil); return }
        let view = SyncShareView(model: SyncShareModel(core: actions.core, id: id), actions: actions, done: { [weak self] in self?.closeShare(id) })
        let w = makeWindow(NSHostingController(rootView: view), id: "sync-share-\(id)")
        shares[id] = w
        NSApp.activate()
        w.makeKeyAndOrderFront(nil)
    }

    /// The Share dialog of a folder not synced yet (Share ▸ Seedwell): add and share in one. Once Copy Link has
    /// added it, it is that folder's dialog.
    func showShare(newFolder path: String, actions: SyncActions) {
        let key = "new:" + path
        if let w = shares[key] { NSApp.activate(); w.makeKeyAndOrderFront(nil); return }
        let model = SyncShareModel(core: actions.core, newFolder: path)
        let view = SyncShareView(model: model, actions: actions, done: { [weak self, model] in self?.closeShare(model.id.isEmpty ? key : model.id) })
        let w = makeWindow(NSHostingController(rootView: view), id: "sync-share-new")
        model.onAdded = { [weak self] id in
            guard let self, let w = self.shares.removeValue(forKey: key) else { return }
            self.shares[id] = w
        }
        shares[key] = w
        NSApp.activate()
        w.makeKeyAndOrderFront(nil)
    }

    func closeShare(_ id: String) {
        guard let w = shares.removeValue(forKey: id) else { return }
        w.delegate = nil
        w.contentViewController = nil
        w.close()
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

    /// Allowing Seedwell's on-demand folders in the Finder (macOS starts the domain switched off).
    func showFileProviderSetup(host: FileProviderHost) {
        if let w = fileProvider { NSApp.activate(); w.makeKeyAndOrderFront(nil); return }
        let view = FileProviderSetupView(host: host, done: { [weak self] in self?.closeFileProviderSetup() })
        let w = makeWindow(NSHostingController(rootView: view), id: "file-provider-setup")
        fileProvider = w
        NSApp.activate()
        w.makeKeyAndOrderFront(nil)
    }

    func closeFileProviderSetup() {
        guard let w = fileProvider else { return }
        fileProvider = nil
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
        closeFileProviderSetup()
        for id in Array(edits.keys) { closeEdit(id) }
        for id in Array(shares.keys) { closeShare(id) }
    }

    func windowWillClose(_ notification: Notification) {
        guard let w = notification.object as? NSWindow else { return }
        if add?.0 === w { add = nil }
        if finder === w { finder = nil }
        if fileProvider === w { fileProvider = nil }
        if let id = shares.first(where: { $0.value === w })?.key { shares[id] = nil }
        if let id = edits.first(where: { $0.value === w })?.key { edits[id] = nil }
        w.delegate = nil
        DispatchQueue.main.async { w.contentViewController = nil }
    }
}
