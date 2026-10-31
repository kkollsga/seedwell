import FileProvider
import Foundation
import UniformTypeIdentifiers

/// Item identifiers: the core's ids are File Provider's, except the folder itself (`root` ↔ the root container).
public enum FPIdentifiers {
    /// The core's id of the folder itself.
    public static let root = "root"

    public static func core(_ id: NSFileProviderItemIdentifier) -> String {
        id == .rootContainer ? root : id.rawValue
    }

    public static func provider(_ id: String) -> NSFileProviderItemIdentifier {
        id == root ? .rootContainer : NSFileProviderItemIdentifier(id)
    }
}

/// The app's errors as File Provider's: what each kind means to the system is the app's decision
/// (`sync_host::ErrKind`); the message is the core's and is shown as it is.
public enum FPErrors {
    public static func error(_ f: FPFailure, item: NSFileProviderItemIdentifier? = nil) -> NSError {
        let info: [String: Any] = [NSLocalizedDescriptionKey: f.message]
        func fp(_ c: NSFileProviderError.Code) -> NSError {
            NSError(domain: NSFileProviderErrorDomain, code: c.rawValue, userInfo: info)
        }
        switch f.kind {
        case .noSuchItem:
            if let item { return NSError.fileProviderErrorForNonExistentItem(withIdentifier: item) }
            return fp(.noSuchItem)
        case .unreachable, .notRunning: return fp(.serverUnreachable)
        case .anchorExpired: return fp(.syncAnchorExpired)
        case .deletionRejected: return fp(.deletionRejected)
        case .notEmpty: return fp(.directoryNotEmpty)
        case .excluded: return fp(.excludedFromSync)
        case .cancelled: return NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError, userInfo: info)
        case .badRequest, .failed: return fp(.cannotSynchronize)
        }
    }

    /// The capabilities the app allows.
    public static func capabilities(_ caps: [String], dir: Bool) -> NSFileProviderItemCapabilities {
        var c: NSFileProviderItemCapabilities = []
        for cap in caps {
            switch cap {
            case "read": c.insert(dir ? .allowsContentEnumerating : .allowsReading)
            case "write": c.insert(dir ? .allowsAddingSubItems : .allowsWriting)
            case "rename": c.insert(.allowsRenaming)
            case "reparent": c.insert(.allowsReparenting)
            case "delete": c.insert(.allowsDeleting)
            default: break
            }
        }
        return c
    }
}

/// An item as File Provider sees it, from what the app said.
public final class FPProviderItem: NSObject, NSFileProviderItem, @unchecked Sendable {
    public let wire: FPItem
    /// The folder's name, for the root (the domain's display name).
    let rootName: String

    public init(_ wire: FPItem, rootName: String = "Seedwell") {
        self.wire = wire
        self.rootName = rootName
    }

    var isRoot: Bool { wire.id == FPIdentifiers.root }
    var isDir: Bool { wire.kind == "dir" }

    public var itemIdentifier: NSFileProviderItemIdentifier { FPIdentifiers.provider(wire.id) }
    public var parentItemIdentifier: NSFileProviderItemIdentifier { isRoot ? .rootContainer : FPIdentifiers.provider(wire.parent) }
    public var filename: String { isRoot ? rootName : wire.name }

    public var contentType: UTType {
        let ext = (wire.name as NSString).pathExtension
        switch wire.kind {
        // A package (an app, a Pages document) is a folder the Finder shows as one file.
        case "dir":
            if !isRoot, !ext.isEmpty, let t = UTType(tag: ext, tagClass: .filenameExtension, conformingTo: nil), t.conforms(to: .package) { return t }
            return .folder
        case "symlink": return .symbolicLink
        default: return UTType(filenameExtension: ext) ?? .data
        }
    }

    public var documentSize: NSNumber? { wire.kind == "file" ? NSNumber(value: wire.size) : nil }
    public var contentModificationDate: Date? { Date(timeIntervalSince1970: Double(wire.mtimeNs) / 1e9) }
    public var creationDate: Date? { contentModificationDate }
    public var capabilities: NSFileProviderItemCapabilities { FPErrors.capabilities(wire.caps, dir: isDir) }

    public var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(contentVersion: Data(wire.contentVersion.utf8), metadataVersion: Data(wire.metadataVersion.utf8))
    }

    /// Kept on this Mac: downloaded ahead and never evicted. The folder itself downloads lazily (the default).
    public var contentPolicy: NSFileProviderContentPolicy {
        if wire.keep { return .downloadEagerlyAndKeepDownloaded }
        return isRoot ? .downloadLazily : .inherited
    }

    public var symlinkTargetPath: String? { wire.symlinkTarget }

    public var fileSystemFlags: NSFileProviderFileSystemFlags {
        var f: NSFileProviderFileSystemFlags = [.userReadable]
        if wire.caps.contains("write") { f.insert(.userWritable) }
        if wire.exec { f.insert(.userExecutable) }
        return f
    }

    /// For the Finder actions' rules (Always Keep on This Mac, Stop Keeping).
    public var userInfo: [AnyHashable: Any]? { ["keep": wire.keep, "pinned": wire.pinned, "state": wire.state] }
}

/// A page token: a cursor or an anchor the app gave, marked so the system's own initial pages read as "from the start".
public enum FPPages {
    static let prefix = "c:"

    public static func page(_ token: String) -> NSFileProviderPage {
        NSFileProviderPage(Data((prefix + token).utf8))
    }

    /// The token in a page, or `nil` for a first page.
    public static func token(_ page: NSFileProviderPage) -> String? {
        guard let s = String(data: page.rawValue, encoding: .utf8), s.hasPrefix(prefix) else { return nil }
        return String(s.dropFirst(prefix.count))
    }

    public static func anchor(_ a: String) -> NSFileProviderSyncAnchor {
        NSFileProviderSyncAnchor(Data(a.utf8))
    }

    public static func anchor(_ a: NSFileProviderSyncAnchor) -> String {
        String(data: a.rawValue, encoding: .utf8) ?? "0"
    }

    /// The content version a change started from, as the app named it.
    public static func version(_ v: NSFileProviderItemVersion) -> String? {
        String(data: v.contentVersion, encoding: .utf8)
    }
}

/// Every item the system has on disk, paged through `enumeratorForMaterializedItems` (the extension reports it to the
/// app; the app reads it before asking the system to download what the core holds).
public final class MaterializedSet: NSObject, NSFileProviderEnumerationObserver, @unchecked Sendable {
    public let enumerator: NSFileProviderEnumerator
    private var ids: [String] = []
    private var finish: (([String]?) -> Void)?
    /// Itself, while it collects (the enumerator need not keep its observer).
    private var alive: MaterializedSet?

    public init(_ e: NSFileProviderEnumerator) { enumerator = e }

    public func collect(_ done: @escaping ([String]?) -> Void) {
        finish = done
        alive = self
        enumerator.enumerateItems(for: self, startingAt: NSFileProviderPage(Data()))
    }

    public func didEnumerate(_ updatedItems: [NSFileProviderItemProtocol]) {
        ids += updatedItems.map { FPIdentifiers.core($0.itemIdentifier) }
    }

    public func finishEnumerating(upTo nextPage: NSFileProviderPage?) {
        if let nextPage {
            enumerator.enumerateItems(for: self, startingAt: nextPage)
        } else {
            enumerator.invalidate()
            finish?(ids)
            finish = nil
            alive = nil
        }
    }

    public func finishEnumeratingWithError(_ error: Error) {
        enumerator.invalidate()
        finish?(nil)
        finish = nil
        alive = nil
    }
}
