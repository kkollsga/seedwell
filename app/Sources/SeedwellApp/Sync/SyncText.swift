import Foundation

// Folder sync (SPEC 7G, 8.2 item 5): the words and states the menu, the window and the modals share, as plain
// data with no AppKit, so they can be tested.

/// The look of a folder's status image.
enum SyncBadge: Equatable {
    case upToDate
    /// A progress ring (0...1).
    case progress(Double)
    case paused
    /// A conflict copy or an error: something for the user to look at.
    case attention
    case missing
}

enum SyncText {
    /// Longest folder name the menu shows before the middle is cut.
    static let titleLimit = 40

    /// How far the folder is (0...1): the bytes it has of the bytes it should have. Nil when not known.
    static func fraction(_ f: SyncFolder) -> Double? {
        guard f.bytes > 0, f.needBytes <= f.bytes else { return nil }
        return Double(f.bytes - f.needBytes) / Double(f.bytes)
    }

    static func devices(_ n: UInt32) -> String { Fmt.count(n, "device") }

    /// The grey line under a folder's name: "Up to date · 3 devices", "Syncing · 42% · 12 MB/s",
    /// "Waiting for devices", "Folder missing".
    static func subtitle(_ f: SyncFolder) -> String {
        let conflicts = f.conflicts > 0 ? Fmt.count(f.conflicts, "conflict") : nil
        switch f.state {
        case .missing: return "Folder missing"
        case .paused: return "Paused"
        case .error: return f.error.map { "Can't sync · \($0)" } ?? "Can't sync"
        case .starting: return "Starting…"
        case .scanning: return [conflicts, "Scanning…"].compactMap { $0 }.joined(separator: " · ")
        case .waitingForDevices:
            return [conflicts, "Waiting for devices"].compactMap { $0 }.joined(separator: " · ")
        case .waitingForAdmin: return "Waiting for the owner or an admin"
        case .syncing:
            var parts = ["Syncing"]
            if let p = fraction(f) { parts.append("\(min(99, Int(p * 100)))%") }
            else if f.needFiles > 0 { parts.append("\(Fmt.count(f.needFiles, "file")) to go") }
            if f.rateIn > 0 { parts.append(Fmt.rate(f.rateIn)) }
            return ([conflicts].compactMap { $0 } + parts).joined(separator: " · ")
        case .upToDate:
            if let conflicts { return f.connected > 0 ? "\(conflicts) · \(devices(f.connected))" : conflicts }
            return f.connected > 0 ? "Up to date · \(devices(f.connected))" : "Up to date"
        }
    }

    static func badge(_ f: SyncFolder) -> SyncBadge {
        switch f.state {
        case .missing: return .missing
        case .paused: return .paused
        case .error: return .attention
        case _ where f.conflicts > 0: return .attention
        case .syncing, .waitingForDevices: return .progress(fraction(f) ?? 0)
        case .waitingForAdmin: return .progress(0)
        case .starting, .scanning: return .progress(0)
        case .upToDate: return .upToDate
        }
    }

    /// The menu's Folder Sync line: "3 folders · Up to date", "Syncing 2 · 12 MB/s", "1 conflict",
    /// "1 folder missing". The thing that needs the user wins.
    static func summary(_ s: SyncSnapshot) -> String {
        if !s.running, s.folders.isEmpty { return s.problem == nil ? "Starting…" : "Not running" }
        let f = s.folders
        if f.isEmpty { return "No folders" }
        let missing = f.filter { $0.state == .missing }.count
        if missing > 0 { return "\(Fmt.count(missing, "folder")) missing" }
        let errors = f.filter { $0.state == .error }.count
        if errors > 0 { return "\(Fmt.count(errors, "folder")) can't sync" }
        let conflicts = f.reduce(0) { $0 + Int($1.conflicts) }
        if conflicts > 0 { return Fmt.count(conflicts, "conflict") }
        let syncing = f.filter { $0.state == .syncing }
        if !syncing.isEmpty {
            let rate = syncing.reduce(UInt64(0)) { $0 + $1.rateIn }
            return "Syncing \(syncing.count)" + (rate > 0 ? " · \(Fmt.rate(rate))" : "")
        }
        let all = Fmt.count(f.count, "folder")
        if f.allSatisfy({ $0.state == .paused }) { return "\(all) · Paused" }
        if f.contains(where: { $0.state == .waitingForDevices || $0.state == .waitingForAdmin }) { return "\(all) · Waiting for devices" }
        if f.contains(where: { $0.state == .starting || $0.state == .scanning }) { return "\(all) · Scanning…" }
        return "\(all) · Up to date"
    }

    /// The menu line needs the user (shown in the secondary colour like the Sharing line's errors).
    static func needsAttention(_ s: SyncSnapshot) -> Bool {
        s.folders.contains { $0.state == .missing || $0.state == .error || $0.conflicts > 0 }
    }

    static func access(_ a: SyncAccess) -> String {
        switch a {
        case .readWrite: "Read & write"
        case .readOnly: "Read only"
        case .encrypted: "Encrypted"
        }
    }

    /// The core's lowercase reason as a line: a capital first letter, no full stop.
    static func sentence(_ s: String) -> String {
        var t = s.trimmingCharacters(in: .whitespaces)
        while t.hasSuffix(".") { t.removeLast() }
        return t.prefix(1).uppercased() + t.dropFirst()
    }

    /// A role as people read it.
    static func role(_ r: SyncRole) -> String {
        switch r {
        case .owner: "Owner"
        case .admin: "Admin"
        case .readWrite: "Read & write"
        case .readOnly: "Read only"
        }
    }

    /// What a role may do, in one line (the Share dialog and the members' menu).
    static func roleNote(_ r: SyncRole) -> String {
        switch r {
        case .owner: "Can do everything, including changing roles"
        case .admin: "Can change files and share the folder"
        case .readWrite: "Can change files"
        case .readOnly: "Receives changes, never sends its own"
        }
    }

    /// Members in the order the folder page lists them: this Mac first, then by role (owners first), then by name.
    static func ordered(_ ms: [SyncMember]) -> [SyncMember] {
        let rank: (SyncRole) -> Int = { [.owner, .admin, .readWrite, .readOnly].firstIndex(of: $0) ?? 4 }
        return ms.sorted { a, b in
            if a.isMe != b.isMe { return a.isMe }
            if rank(a.role) != rank(b.role) { return rank(a.role) < rank(b.role) }
            return a.name.localizedStandardCompare(b.name) == .orderedAscending
        }
    }

    /// A member's grey line: "Local network · Seedwell 0.1.6", "Offline", and "until 1 Nov" when its access ends.
    static func memberLine(_ m: SyncMember, device: SyncDevice?) -> String {
        let until = m.expiresS.map { "until \(day($0))" }
        if m.isMe { return until.map { "Access \($0)" } ?? "" }
        let reach = device.flatMap { d in m.online ? [via(d.via), d.client].filter { !$0.isEmpty }.joined(separator: " · ") : nil }
        return [reach ?? "Offline", until].compactMap { $0 }.joined(separator: " · ")
    }

    /// The device may share the folder (create invitation links).
    static func canShare(_ r: SyncRole) -> Bool { r == .owner || r == .admin }

    /// "Read & write · Photos" for a pasted link, or why it cannot be used: the core's `problem` when it gave one
    /// ("this link was made by a newer Seedwell…").
    static func linkLine(_ text: String, info: SyncLinkInfo?, problem: String? = nil) -> String {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if t.isEmpty { return "" }
        guard let info else {
            if let problem { return SyncText.sentence(problem) }
            return ["SWW-", "SWR-", "SWE-"].contains { t.uppercased().hasPrefix($0) }
                ? "Folders are shared with links now: ask for a seedwell:// link"
                : "Not a Seedwell folder link"
        }
        if info.alreadySynced { return "This folder is already synced on this Mac" }
        if info.expired { return "This link has expired: ask for a new one" }
        let chat = info.channelName.map { "with the channel “\($0)”\(info.channelJoined ? " (joined already)" : "")" }
        return [role(info.role), info.name, chat].compactMap { $0 }.joined(separator: " · ")
    }

    /// An invitation link's line in the Share dialog: "Read & write · 0 of 1 used", or "· Used", "· Expired",
    /// "· Revoked". `now` in Unix seconds.
    static func inviteLine(_ i: SyncInvite, now: UInt64) -> String {
        let state = switch inviteState(i, now: now) {
        case .revoked: "Revoked"
        case .used: "Used"
        case .expired: "Expired"
        case .open: i.maxUses == 0 ? "\(Fmt.count(i.uses, "use")), unlimited" : "\(i.uses) of \(i.maxUses) used"
        }
        return "\(role(i.role)) · \(state)"
    }

    /// What the Share dialog's next link does, in words: "One Mac can join with this link within 7 days, as Read &
    /// write." `0` is no limit.
    /// `chat`: the channel the link joins as well.
    static func shareSummary(role r: SyncRole, uses: UInt32, linkDays: UInt32, chat: String? = nil) -> String {
        let who = switch uses {
        case 1: "One Mac"
        case 0: "Any number of Macs"
        default: "Up to \(uses) Macs"
        }
        let when = linkDays > 0 ? " within \(Fmt.count(linkDays, "day"))" : ""
        let also = chat.map { " It joins the channel “\($0)” too." } ?? ""
        return "\(who) can join with this link\(when), as \(role(r)).\(also)"
    }

    /// The time left of a sharing period: "Sharing ends in 6 days, 23 hours", "… in 5 hours, 12 minutes",
    /// "… in 12 minutes", "Sharing has ended".
    static func countdown(until: UInt64, now: UInt64) -> String {
        guard until > now else { return "Sharing has ended" }
        let left = until - now
        let (d, h, m) = (left / 86_400, left % 86_400 / 3_600, left % 3_600 / 60)
        let parts: [String] = d > 0 ? [Fmt.count(d, "day"), Fmt.count(h, "hour")]
            : h > 0 ? [Fmt.count(h, "hour"), Fmt.count(m, "minute")]
            : [Fmt.count(max(m, 1), "minute")]
        return "Sharing ends in " + parts.joined(separator: ", ")
    }

    /// The folder's sharing period under its setting: "When it ends (1 Nov), every Mac but the owners' leaves the
    /// folder, and links stop working." `owner`: this Mac sets it.
    static func sharingLine(until: UInt64?, owner: Bool) -> String {
        guard let until else {
            return owner ? "Shared until you stop it. Macs you remove keep the files they have." : "Shared until the owner stops it."
        }
        return "When it ends (\(day(until))), every Mac but the owners’ leaves the folder, and links stop working."
    }

    /// The grey line under an open link: "Link until 1 Nov" (empty without an expiry).
    static func inviteDetail(_ i: SyncInvite) -> String {
        [i.expiresS.map { "Link until \(day($0))" }, i.channelName.map { "with the channel “\($0)”" }].compactMap { $0 }.joined(separator: " · ")
    }

    /// "1 Nov" (with the year when it is not this year).
    static func day(_ unix: UInt64) -> String {
        let d = Date(timeIntervalSince1970: TimeInterval(unix))
        let sameYear = Calendar.current.isDate(d, equalTo: .now, toGranularity: .year)
        return d.formatted(sameYear ? .dateTime.day().month(.abbreviated) : .dateTime.day().month(.abbreviated).year())
    }

    enum InviteState { case open, used, expired, revoked }

    static func inviteState(_ i: SyncInvite, now: UInt64) -> InviteState {
        if i.revoked { return .revoked }
        if i.maxUses > 0, i.uses >= i.maxUses { return .used }
        if let e = i.expiresS, e <= now { return .expired }
        return .open
    }

    /// The folder's name a key gives (its name hint, else the first group of its id).
    static func suggestedName(_ info: SyncLinkInfo) -> String {
        if let n = info.name?.trimmingCharacters(in: .whitespaces), !n.isEmpty { return n }
        return String(info.folderId.split(separator: "-").first ?? "Folder")
    }

    /// A pasteboard text that is worth prefilling the add modal with: a `seedwell://sync/` link.
    static func looksLikeLink(_ text: String) -> Bool {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard t.count < 2_000 else { return false }
        return t.lowercased().hasPrefix("seedwell://sync/")
    }

    /// "Studio iMac · LAN"
    static func via(_ v: String) -> String {
        switch v {
        case "LAN": "Local network"
        case "Relay": "Through a relay"
        default: "Internet"
        }
    }
}

/// The add modal's choices and whether Start may be pressed (SPEC 7G.5).
struct SyncAddForm: Equatable {
    enum Mode: String, CaseIterable, Identifiable {
        case local = "From this Mac", shared = "Shared with me"
        var id: String { rawValue }
    }

    var mode: Mode = .local
    /// From this Mac: the folder chosen.
    var folder = ""
    /// Shared with me: what was pasted, what it is, and where it goes.
    var key = ""
    var info: SyncLinkInfo?
    /// Why the pasted text is not a usable link, in the core's words (none when it is one, or nothing is pasted).
    var problem: String?
    var location = ""
    /// The user picked the location: a new key does not move it any more.
    var locationChosen = false

    var keyLine: String { SyncText.linkLine(key, info: info, problem: problem) }

    /// The pasted text is a usable link.
    var keyUsable: Bool {
        guard let info else { return false }
        return !info.expired && !info.alreadySynced
    }

    var canStart: Bool {
        switch mode {
        case .local: !folder.trimmingCharacters(in: .whitespaces).isEmpty
        case .shared: keyUsable && !location.trimmingCharacters(in: .whitespaces).isEmpty
        }
    }

    /// A new key was pasted: decode it, and follow it with the location unless the user chose one.
    mutating func setKey(_ text: String, info: SyncLinkInfo?, defaultLocation: String?, problem: String? = nil) {
        key = text
        self.info = info
        self.problem = info == nil ? problem : nil
        if !locationChosen { location = info == nil ? "" : (defaultLocation ?? "") }
    }

    /// The user chose where it goes: the parent folder, the folder itself is named after the key.
    mutating func chooseParent(_ parent: String) {
        let name = info.map(SyncText.suggestedName) ?? "Folder"
        location = (parent as NSString).appendingPathComponent(name)
        locationChosen = true
    }
}

/// The delete confirmation under a row of the menu's folder list: one open at a time, closed whenever the
/// menu opens (SPEC 8.2 item 5).
struct SyncRemoval: Equatable {
    /// The folder whose confirmation is open.
    private(set) var open: String?
    /// "Also move the files to the Trash".
    var trash = false

    /// The trash button of a row: opens its confirmation (closing another), or closes its own.
    mutating func toggle(_ id: String) {
        if open == id { close() } else { open = id; trash = false }
    }

    mutating func close() { open = nil; trash = false }

    func isOpen(_ id: String) -> Bool { open == id }

    /// The folder is gone (removed elsewhere): its confirmation goes too.
    mutating func keep(only ids: Set<String>) { if let o = open, !ids.contains(o) { close() } }

    static func question(_ name: String) -> String { "Stop syncing “\(name)” on this Mac? Other devices keep their copy." }
    static let trashLabel = "Also move the files to the Trash"
}

extension SyncText {
    /// The plain sentence of a core error ("This folder is no longer synced on this Mac.").
    static func message(_ error: Error) -> String {
        switch error {
        case let CoreError.Failed(m), let CoreError.InvalidSource(m): return m
        case let CoreError.Invalid(p): return p.joined(separator: "\n")
        case CoreError.NotFound: return "This folder is no longer synced on this Mac."
        default: return "\(error)"
        }
    }
}
