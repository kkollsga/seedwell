import AppKit
import os

/// Every folder sync command, in one place: the menu, the window and the modals all go through here
/// (SPEC 7G.5). Calls that can take a while (moving a folder, joining) run off the main thread.
@MainActor final class SyncActions {
    let core: Core
    /// Wired by the app delegate: the add modal (prefilled with a key or link), the edit modal, and the
    /// window's Folder Sync page (`askRemove`: with the remove question open under its header).
    var showAdd: (_ prefill: String?) -> Void = { _ in }
    var showEdit: (_ id: String) -> Void = { _ in }
    var showFolder: (_ id: String?, _ askRemove: Bool) -> Void = { _, _ in }
    /// The Share dialog of a folder (invitation links), from the folder list, the edit modal and Finder.
    var showShare: (_ id: String) -> Void = { _ in }
    /// A folder's chat: the channel's page.
    var showChat: (_ channel: String) -> Void = { _ in }
    /// The Share dialog of a folder not synced yet (add and share in one).
    var showShareNew: (_ path: String) -> Void = { _ in }
    /// The last folder that was removed (the window drops its page).
    var onRemoved: (String) -> Void = { _ in }
    /// On-demand folders in the Finder (the user's own app only; `nil` in tests, QA and other isolated runs).
    var fileProvider: FileProviderHost?

    init(core: Core) { self.core = core }

    func snapshot() -> SyncSnapshot { core.syncSnapshot() }

    /// Reveal: what the core's `syncReveal` says, the folder in Finder or its page. An on-demand folder is where the
    /// Finder shows it (its File Provider domain's root, when it has one).
    func reveal(_ f: SyncFolder) {
        guard f.onDemand else { return reveal(f, syncReveal(folder: f, onDemandPlace: nil)) }
        Task {
            let place = await fileProvider?.rootURL(f.id)?.path
            reveal(f, syncReveal(folder: f, onDemandPlace: place))
        }
    }

    private func reveal(_ f: SyncFolder, _ r: SyncReveal) {
        switch r {
        case .show(let path): NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
        case .page: showFolder(f.id, false)
        }
    }

    /// Runs one of the core's calls (an `async` one: off the main thread) for its result.
    nonisolated func run<T: Sendable>(_ work: @escaping @Sendable () async throws -> T) async -> Result<T, Error> {
        await catching(work)
    }

    /// Stops syncing on this Mac; with `trash`, the core then moves the folder to the Trash (it refuses one that cannot
    /// go there, which stays synced). Errors are told in an alert (the menu, which asked, is closed by then).
    /// (The core has an on-demand folder's domain removed first, so its place is an ordinary folder again: SPEC 7G.11.)
    func remove(_ f: SyncFolder, trash: Bool) {
        Task {
            let core = self.core
            let id = f.id
            let r = await run({ try await core.syncRemove(id: id, trashFiles: trash) })
            switch r {
            case .failure(let e):
                alert(syncRemoveFailed(name: f.name, error: SyncText.message(e)))
            case .success(let trashFailed):
                onRemoved(f.id)
                if let trashFailed { alert(trashFailed) }
            }
        }
    }

    /// Resolves the conflict copy `copy` (of `original`); the version not kept goes to the folder's archive on this Mac.
    /// Errors are told in an alert; `done` runs either way (the page reads its conflicts again).
    func resolve(_ folder: SyncFolder, copy: String, original: String, keep: SyncKeep, done: @escaping () -> Void) {
        Task {
            let core = self.core
            let id = folder.id
            if case .failure(let e) = await run({ try await core.syncResolveConflict(id: id, copy: copy, keep: keep) }) {
                alert(syncResolveFailed(original: original, error: SyncText.message(e)))
            }
            done()
        }
    }

    /// The system Share menu shared `path`: the Share dialog of the synced folder it is in (or why this Mac may not
    /// share it), else the Share dialog of that folder (or the file's folder) as a new one: Copy Link adds it and makes the link.
    func share(path: String) {
        // The path is looked at (its real name, whether it is a folder) off the main thread: it may be on a slow disk.
        Task {
            let (real, isDir) = await Task.detached(priority: .userInitiated) { () -> (String, Bool) in
                let real = FinderStatusWriter.realPath(path)
                var dir: ObjCBool = false
                return (real, FileManager.default.fileExists(atPath: real, isDirectory: &dir) && dir.boolValue)
            }.value
            share(real: real, isDir: isDir)
        }
    }

    private func share(real: String, isDir: Bool) {
        let f = syncFolderContaining(folders: snapshot().folders, path: real)
        let refusal = f.flatMap { syncShareRefusal(folder: $0) }
        // Like the Finder extension's "menu" lines: what the Share menu was used on, and what it opened.
        Logger(subsystem: "io.github.kkollsga.Seedwell", category: "share-menu")
            .info("share: \(f == nil ? "not synced" : refusal == nil ? "share dialog" : "may not share", privacy: .public) for \(real)")
        if let f {
            if let refusal { alert(refusal) } else { showShare(f.id) }
            return
        }
        showShareNew(isDir ? real : (real as NSString).deletingLastPathComponent)
    }

    /// Gives a device another role in a folder or channel `id` (owners only; the core says so otherwise). Errors are
    /// told in an alert; `done` runs either way (the page reads the members again).
    func setRole(_ id: String, _ m: SyncMember, _ role: SyncRole, done: @escaping () -> Void) {
        setRole(id, device: m.deviceId, deviceName: m.name, role, done: done)
    }

    func setRole(_ id: String, device: String, deviceName: String, _ role: SyncRole, done: @escaping () -> Void) {
        Task {
            let core = self.core
            if case .failure(let e) = await run({ try await core.syncSetRole(id: id, deviceId: device, role: role) }) {
                alert(syncSetRoleFailed(device: deviceName, error: SyncText.message(e)))
            }
            done()
        }
    }

    /// Removes a device from a folder or channel `id` (named `name`) for everyone (owners only). It is cut off at
    /// once and keeps what it has.
    func removeDevice(_ id: String, name: String, _ m: SyncMember, done: @escaping () -> Void) {
        removeDevice(id, name: name, device: m.deviceId, deviceName: m.name, done: done)
    }

    /// Removes a device gone from a folder or channel `id` (named `name`): as `removeDevice`.
    func removeGone(_ id: String, name: String, _ d: SyncGoneDevice, done: @escaping () -> Void) {
        removeDevice(id, name: name, device: d.deviceId, deviceName: d.name, done: done)
    }

    func removeDevice(_ id: String, name: String, device: String, deviceName: String, done: @escaping () -> Void) {
        Task {
            let core = self.core
            if case .failure(let e) = await run({ try await core.syncRemoveDevice(id: id, deviceId: device) }) {
                alert(syncRemoveDeviceFailed(device: deviceName, name: name, error: SyncText.message(e)))
            }
            done()
        }
    }

    /// Shares a folder again from this Mac when every owner and admin of it is gone (`canReshare`): the core makes it a
    /// new folder owned here, at the same place, its files untouched. Then the new folder's page and Share dialog open.
    /// Errors are told in an alert.
    func reshare(_ f: SyncFolder) {
        Task {
            let core = self.core
            let id = f.id
            switch await run({ try await core.syncReshare(id: id) }) {
            case .failure(let e):
                alert(syncReshareFailed(name: f.name, error: SyncText.message(e)))
            case .success(let new):
                showFolder(new.id, false)
                showShare(new.id)
            }
        }
    }

    func alert(_ a: SyncAlert) { alert(a.title, a.text) }

    func alert(_ title: String, _ text: String) {
        let a = NSAlert()
        a.messageText = title
        a.informativeText = text
        NSApp.activate()
        a.runModal()
    }

    /// Copies a key or link (it is a secret: shown and copied only, never logged).
    func copy(_ text: String) {
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(text, forType: .string)
    }

    /// The pasteboard's text when it holds a folder link (the add modal starts with it).
    func pastedKey() -> String? {
        // A QA run reads a pasteboard of its own, never the user's.
        let board = QuietRun.on ? NSPasteboard(name: NSPasteboard.Name("seedwell-qa-sync")) : .general
        guard let s = board.string(forType: .string), syncLooksLikeLink(text: s) else { return nil }
        return s.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}
