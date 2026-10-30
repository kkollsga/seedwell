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

    init(core: Core) { self.core = core }

    func snapshot() -> SyncSnapshot { core.syncSnapshot() }

    /// Reveals the folder in Finder; a missing one shows its page instead (there is nothing to reveal).
    func reveal(_ f: SyncFolder) {
        if FileManager.default.fileExists(atPath: f.path) {
            NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: f.path)])
        } else {
            showFolder(f.id, false)
        }
    }

    /// Runs a blocking core call off the main thread.
    nonisolated func run<T: Sendable>(_ work: @escaping @Sendable () throws -> T) async -> Result<T, Error> {
        await Task.detached(priority: .userInitiated) { Result { try work() } }.value
    }

    /// Stops syncing on this Mac; with `trash`, the core then moves the folder to the Trash (it refuses one that cannot
    /// go there, which stays synced). Errors are told in an alert (the menu, which asked, is closed by then).
    func remove(_ f: SyncFolder, trash: Bool) {
        Task {
            let core = self.core
            let id = f.id
            switch await run({ try core.syncRemove(id: id, trashFiles: trash) }) {
            case .failure(let e):
                alert(syncRemoveFailed(name: f.name, error: SyncText.message(e)))
            case .success(let trashFailed):
                onRemoved(f.id)
                if let trashFailed { alert(trashFailed) }
            }
        }
    }

    /// Resolves a conflict; the version not kept goes to the folder's archive on this Mac. Errors are told
    /// in an alert; `done` runs either way (the page reads its conflicts again).
    func resolve(_ folder: SyncFolder, _ c: SyncConflict, keep: SyncKeep, done: @escaping () -> Void) {
        Task {
            let core = self.core
            let (id, copy) = (folder.id, c.copy)
            if case .failure(let e) = await run({ try core.syncResolveConflict(id: id, copy: copy, keep: keep) }) {
                alert(syncResolveFailed(original: c.original, error: SyncText.message(e)))
            }
            done()
        }
    }

    /// The system Share menu shared `path`: the Share dialog of the synced folder it is in (or why this Mac may not
    /// share it), else the Share dialog of that folder (or the file's folder) as a new one: Copy Link adds it and makes the link.
    func share(path: String) {
        let real = FinderStatusWriter.realPath(path)
        let f = syncFolderContaining(folders: snapshot().folders, path: real)
        let refusal = f.flatMap { syncShareRefusal(folder: $0) }
        // Like the Finder extension's "menu" lines: what the Share menu was used on, and what it opened.
        Logger(subsystem: "io.github.kkollsga.Seedwell", category: "share-menu")
            .info("share: \(f == nil ? "not synced" : refusal == nil ? "share dialog" : "may not share", privacy: .public) for \(real)")
        if let f {
            if let refusal { alert(refusal) } else { showShare(f.id) }
            return
        }
        var dir: ObjCBool = false
        let isDir = FileManager.default.fileExists(atPath: real, isDirectory: &dir) && dir.boolValue
        showShareNew(isDir ? real : (real as NSString).deletingLastPathComponent)
    }

    /// Gives a device another role in a folder or channel `id` (owners only; the core says so otherwise). Errors are
    /// told in an alert; `done` runs either way (the page reads the members again).
    func setRole(_ id: String, _ m: SyncMember, _ role: SyncRole, done: @escaping () -> Void) {
        Task {
            let core = self.core
            let device = m.deviceId
            if case .failure(let e) = await run({ try core.syncSetRole(id: id, deviceId: device, role: role) }) {
                alert(syncSetRoleFailed(device: m.name, error: SyncText.message(e)))
            }
            done()
        }
    }

    /// Removes a device from a folder or channel `id` (named `name`) for everyone (owners only). It is cut off at
    /// once and keeps what it has.
    func removeDevice(_ id: String, name: String, _ m: SyncMember, done: @escaping () -> Void) {
        Task {
            let core = self.core
            let device = m.deviceId
            if case .failure(let e) = await run({ try core.syncRemoveDevice(id: id, deviceId: device) }) {
                alert(syncRemoveDeviceFailed(device: m.name, name: name, error: SyncText.message(e)))
            }
            done()
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
        guard let s = NSPasteboard.general.string(forType: .string), syncLooksLikeLink(text: s) else { return nil }
        return s.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}
