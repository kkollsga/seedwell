import Foundation

// Folder sync (SPEC 7G, 8.2 item 5): the words and states the menu, the window and the modals share are the core's
// (trs-ffi `sync_text.rs` and `sync_menu.rs`, the same in every host app). This only forwards the ones used in many
// places, and writes dates in the user's own style (`Fmt.day`).

enum SyncText {
    /// The grey line under a folder's name: "Up to date · 3 devices", "Syncing · 42% · 12 MB/s", "Folder missing".
    static func subtitle(_ f: SyncFolder) -> String { syncFolderSubtitle(folder: f) }
    static func badge(_ f: SyncFolder) -> SyncBadge { syncFolderBadge(folder: f) }
    /// "Owner", "Admin", "Read & write", "Read only".
    static func role(_ r: SyncRole) -> String { syncRoleLabel(role: r) }
    /// The device may share the folder or channel (make invitation links).
    static func canShare(_ r: SyncRole) -> Bool { syncCanShare(role: r) }
    /// This Mac first, then by role, then by name.
    static func ordered(_ ms: [SyncMember]) -> [SyncMember] { syncMembersOrdered(members: ms) }
    /// A member's grey line: "Local network · Seedwell 0.1.6", "Offline · until 1 Nov".
    static func memberLine(_ m: SyncMember, device: SyncDevice?) -> String {
        syncMemberLine(member: m, device: device, expiresDay: m.expiresS.map { Fmt.day($0) })
    }
    /// "Read & write · Photos" for a pasted link, or why it cannot be used.
    static func linkLine(_ text: String, info: SyncLinkInfo?, problem: String? = nil) -> String {
        syncLinkLine(text: text, info: info, problem: problem)
    }
    /// The plain sentence of an error ("This folder is no longer synced on this Mac.").
    static func message(_ error: Error) -> String {
        (error as? CoreError).map { coreErrorMessage(error: $0) } ?? "\(error)"
    }
}

extension SyncBadge {
    /// What a screen reader says for the image: "up to date", "syncing", "needs attention".
    var accessibilityWord: String { syncBadgeLabel(badge: self) }
}

/// The add modal's choices and whether Start may be pressed (SPEC 7G.5): a value the modal edits, the rules the core's.
extension SyncAddForm {
    init() { self = syncAddFormNew() }

    var keyLine: String { syncAddKeyLine(form: self) }
    /// The pasted text is a usable link.
    var keyUsable: Bool { syncAddKeyUsable(form: self) }
    var canStart: Bool { syncAddCanStart(form: self) }

    /// A new link was pasted: what it is, and the location follows it unless the user chose one.
    mutating func setKey(_ text: String, info: SyncLinkInfo?, defaultLocation: String?, problem: String? = nil) {
        self = syncAddSetKey(form: self, text: text, info: info, defaultLocation: defaultLocation, problem: problem)
    }

    /// The user chose where it goes: the parent folder; the folder itself is named after the link.
    mutating func chooseParent(_ parent: String) { self = syncAddChooseParent(form: self, parent: parent) }
}
