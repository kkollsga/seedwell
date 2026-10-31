import Foundation

// The File Provider extension's protocol with the app (SPEC 7G.11): one JSON line per request over a Unix socket in
// the app group's container, answered by the core (crates/ffi/src/sync_host.rs says what each op asks and answers).
// The extension decides nothing: what an item may do, whether it is kept and what an error means come from the app.

/// The app group the app and its extension share: Team-prefixed, so it needs no provisioning profile.
public enum FPGroup {
    public static let id = "6GND8Z2C7Z.io.github.kkollsga.Seedwell"
    /// The socket's name in the container (the whole path must fit in 103 bytes).
    public static let socketName = "fp"
    /// Where the extension puts content it hands to the app (the app's copies for the extension go to `t/`).
    public static let uploads = "u"

    /// The group's container (the system makes it on first use).
    public static var container: URL? {
        FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: id)
    }
}

/// The protocol's version (`sync_host::PROTOCOL`).
public let fpProtocol = 1

/// A request. Only the fields an op uses are set; the rest are left out of the line.
public struct FPRequest: Encodable, Equatable, Sendable {
    public var v = fpProtocol
    public var op: String
    public var folder: String
    public var item: String?
    public var cursor: String?
    public var limit: Int?
    public var anchor: String?
    public var parent: String?
    public var name: String?
    public var kind: String?
    /// The name of the content's file in the container's `u/` (the only place the app takes content from).
    public var contents: String?
    public var base: String?
    public var on: Bool?
    public var ids: [String]?
    public var recursive: Bool?
    public var mayExist: Bool?
    /// A report of the system's set of items on disk: its number (later ones higher), and which part of how many
    /// this is (`part` counts from 0); the app takes a report only whole.
    public var report: UInt64?
    public var part: Int?
    public var parts: Int?

    public init(_ op: String, folder: String, item: String? = nil) {
        self.op = op
        self.folder = folder
        self.item = item
    }

    /// Identifiers per part of a report (the app takes requests of 64 KiB, and parts of reports up to 2 MiB).
    public static let materializedPart = 5000

    /// A report of the system's set of items on disk, in parts.
    public static func materialized(folder: String, ids: [String], report: UInt64) -> [FPRequest] {
        let parts = stride(from: 0, to: max(ids.count, 1), by: materializedPart).map { Array(ids[$0..<min($0 + materializedPart, ids.count)]) }
        return parts.enumerated().map { n, part in
            var r = FPRequest("materialized", folder: folder)
            r.ids = part
            r.report = report
            r.part = n
            r.parts = parts.count
            return r
        }
    }

    /// The line sent: JSON, then a newline. Written field by field, in this order: `v` and `op` first, so the app
    /// knows a long report as one before it reads past the limit of an ordinary request (an encoder keeps no order);
    /// the fields an op does not use are left out.
    public func line() throws -> Data {
        let e = JSONEncoder()
        var fields: [String] = []
        func add<T: Encodable>(_ key: String, _ value: T?) throws {
            guard let value else { return }
            fields.append("\"\(key)\":" + String(decoding: try e.encode(value), as: UTF8.self))
        }
        try add("v", v)
        try add("op", op)
        try add("folder", folder)
        try add("item", item)
        try add("cursor", cursor)
        try add("limit", limit)
        try add("anchor", anchor)
        try add("parent", parent)
        try add("name", name)
        try add("kind", kind)
        try add("contents", contents)
        try add("base", base)
        try add("on", on)
        try add("recursive", recursive)
        try add("may_exist", mayExist)
        try add("report", report)
        try add("part", part)
        try add("parts", parts)
        try add("ids", ids)
        return Data(("{" + fields.joined(separator: ",") + "}\n").utf8)
    }
}

/// An item as the app describes it (`sync_host::WireItem`).
public struct FPItem: Decodable, Equatable, Sendable {
    public var id: String
    public var parent: String
    public var name: String
    /// `file`, `dir` or `symlink`.
    public var kind: String
    public var size: UInt64
    public var mtimeNs: Int64
    public var contentVersion: String
    public var metadataVersion: String
    /// `online_only`, `downloading`, `downloaded`, `modified_here`, `unavailable`.
    public var state: String
    /// Kept on this computer (pinned, or below a pinned folder).
    public var keep: Bool
    /// Pinned itself (the user's own Always Keep on This Mac).
    public var pinned: Bool
    public var exec: Bool
    public var symlinkTarget: String?
    /// `read`, `write`, `rename`, `reparent`, `delete`.
    public var caps: [String]

    public init(id: String, parent: String, name: String, kind: String, size: UInt64 = 0, mtimeNs: Int64 = 0,
                contentVersion: String = "", metadataVersion: String = "", state: String = "downloaded", keep: Bool = false,
                pinned: Bool = false, exec: Bool = false, symlinkTarget: String? = nil, caps: [String] = ["read"]) {
        (self.id, self.parent, self.name, self.kind, self.size, self.mtimeNs) = (id, parent, name, kind, size, mtimeNs)
        (self.contentVersion, self.metadataVersion, self.state, self.keep, self.pinned) = (contentVersion, metadataVersion, state, keep, pinned)
        (self.exec, self.symlinkTarget, self.caps) = (exec, symlinkTarget, caps)
    }
}

/// A page of a folder's children.
public struct FPPage: Decodable, Equatable, Sendable {
    public var items: [FPItem]
    public var next: String?
}

/// What changed since an anchor.
public struct FPChanges: Decodable, Equatable, Sendable {
    public var items: [FPItem]
    public var deleted: [String]
    public var anchor: String
    public var more: Bool
}

public struct FPAnchor: Decodable, Equatable, Sendable {
    public var anchor: String
}

/// A fetched file: `path` is the app's copy in the container, the extension's to move.
public struct FPFetched: Decodable, Equatable, Sendable {
    public var item: FPItem
    public var path: String
}

public struct FPItemAnswer: Decodable, Equatable, Sendable {
    public var item: FPItem
}

/// A change: the item now, whether the system should fetch its content again (the user's edit became a conflict
/// copy, which arrives as a new item), and that copy.
public struct FPModified: Decodable, Equatable, Sendable {
    public var item: FPItem
    public var fetchContent: Bool
    public var conflictCopy: FPItem?
}

public struct FPDeleted: Decodable, Equatable, Sendable {
    public var deleted: UInt64
}

public struct FPReleased: Decodable, Equatable, Sendable {
    public var released: UInt64
}

public struct FPHello: Decodable, Equatable, Sendable {
    public var app: String
}

/// An answer with nothing in it.
public struct FPEmpty: Decodable, Equatable, Sendable {}

/// A request that failed, as the app says (`sync_host::ErrKind`), or because the app could not be reached.
public struct FPFailure: Error, Equatable, Sendable {
    public enum Kind: String, Decodable, Sendable {
        case noSuchItem = "no_such_item"
        case unreachable
        case anchorExpired = "anchor_expired"
        case deletionRejected = "deletion_rejected"
        case notEmpty = "not_empty"
        /// Not synced at all (a link): the system keeps it here and stops asking.
        case excluded
        case badRequest = "bad_request"
        case failed
        /// The app is not running (and did not start in time).
        case notRunning = "not_running"
        /// The request was cancelled here (the connection closed).
        case cancelled
    }

    public var kind: Kind
    public var message: String

    public init(_ kind: Kind, _ message: String) {
        self.kind = kind
        self.message = message
    }

    public static let notRunningMessage = "Seedwell isn't running."
}

/// One line of an answer: a fetch's progress, or the final answer.
struct FPLine<T: Decodable>: Decodable {
    struct Progress: Decodable {
        var done: UInt64
        var total: UInt64
    }

    struct Err: Decodable {
        var kind: String
        var message: String
    }

    var v: Int
    var progress: Progress?
    /// Still working (a line now and then while a request takes long).
    var alive: Bool?
    var ok: T?
    var err: Err?
}

/// Decodes an answer's lines (snake case).
func fpDecoder() -> JSONDecoder {
    let d = JSONDecoder()
    d.keyDecodingStrategy = .convertFromSnakeCase
    return d
}
