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
        if f.contains(where: { $0.state == .waitingForDevices }) { return "\(all) · Waiting for devices" }
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

    /// "Read & write · Photos" for a pasted key or link, or why it cannot be used.
    static func linkLine(_ text: String, info: SyncLinkInfo?) -> String {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if t.isEmpty { return "" }
        guard let info else { return "Not a folder key or link" }
        if info.alreadySynced { return "This folder is already synced on this Mac" }
        if info.access == .encrypted { return "Encrypted keys are not supported yet" }
        return [access(info.access), info.name].compactMap { $0 }.joined(separator: " · ")
    }

    /// The folder's name a key gives (its name hint, else the first group of its id).
    static func suggestedName(_ info: SyncLinkInfo) -> String {
        if let n = info.name?.trimmingCharacters(in: .whitespaces), !n.isEmpty { return n }
        return String(info.folderId.split(separator: "-").first ?? "Folder")
    }

    /// A pasteboard text that is worth prefilling the add modal with: a key or a `seedwell://sync/` link.
    static func looksLikeKey(_ text: String) -> Bool {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard t.count < 2_000 else { return false }
        return t.lowercased().hasPrefix("seedwell://sync/") || ["SWW-", "SWR-", "SWE-"].contains { t.uppercased().hasPrefix($0) }
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
    var location = ""
    /// The user picked the location: a new key does not move it any more.
    var locationChosen = false

    var keyLine: String { SyncText.linkLine(key, info: info) }

    /// The pasted text is a usable key.
    var keyUsable: Bool {
        guard let info else { return false }
        return info.access != .encrypted && !info.alreadySynced
    }

    var canStart: Bool {
        switch mode {
        case .local: !folder.trimmingCharacters(in: .whitespaces).isEmpty
        case .shared: keyUsable && !location.trimmingCharacters(in: .whitespaces).isEmpty
        }
    }

    /// A new key was pasted: decode it, and follow it with the location unless the user chose one.
    mutating func setKey(_ text: String, info: SyncLinkInfo?, defaultLocation: String?) {
        key = text
        self.info = info
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
