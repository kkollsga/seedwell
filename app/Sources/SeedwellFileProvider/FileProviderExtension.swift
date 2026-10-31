import AppKit
import FileProvider
import Foundation
import SeedwellFileProviderShared
import UniformTypeIdentifiers

// Seedwell's on-demand folders in the Finder (SPEC 7G.11). Stateless and thin: every call is one request to the running
// app over the socket in the app group's container, answered by the core. Nothing is decided here.

/// A closure handed to another queue (the system's completion handlers are not marked Sendable).
struct Unchecked<T>: @unchecked Sendable {
    let value: T
}

/// Socket calls block: run them off the system's queues.
func background(_ work: @escaping () -> Void) {
    let w = Unchecked(value: work)
    DispatchQueue.global(qos: .userInitiated).async { w.value() }
}

/// Starts Seedwell when it is not running (in the menu bar only), at most every 30 s.
enum AppLauncher {
    static let gate = FPLaunchGate { launch() }

    /// The app this extension is in: `Seedwell.app/Contents/PlugIns/SeedwellFileProvider.appex`.
    static var app: URL {
        Bundle.main.bundleURL.deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
    }

    static func launch() {
        let cfg = NSWorkspace.OpenConfiguration()
        cfg.activates = false
        cfg.addsToRecentItems = false
        cfg.arguments = ["--background"]
        NSWorkspace.shared.openApplication(at: app, configuration: cfg) { _, _ in }
    }
}

/// What every part of the extension needs: the client and which folder this domain is.
struct Context: Sendable {
    let client: FPClient
    let folder: String
    let name: String

    func request(_ op: String, item: String? = nil) -> FPRequest { FPRequest(op, folder: folder, item: item) }

    func item(_ w: FPItem) -> FPProviderItem { FPProviderItem(w, rootName: name) }

    /// A copy of `url` in the container's `u/`, for the app to take (removed after the request).
    func upload(_ url: URL?) throws -> URL {
        guard let dir = FPGroup.container?.appendingPathComponent(FPGroup.uploads, isDirectory: true) else {
            throw FPErrors.error(FPFailure(.notRunning, FPFailure.notRunningMessage))
        }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let to = dir.appendingPathComponent(UUID().uuidString)
        if let url {
            try FileManager.default.copyItem(at: url, to: to)  // a clone on APFS
        } else {
            try Data().write(to: to)  // a new empty file
        }
        return to
    }
}

@objc(SeedwellFileProviderExtension)
final class FileProviderExtension: NSObject, NSFileProviderReplicatedExtension, NSFileProviderCustomAction {
    let domain: NSFileProviderDomain
    let ctx: Context

    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        let dir = FPGroup.container ?? URL(fileURLWithPath: "/var/empty")
        ctx = Context(client: FPClient(dir: dir, gate: AppLauncher.gate), folder: domain.identifier.rawValue, name: domain.displayName)
        super.init()
    }

    func invalidate() {}

    func item(for identifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest,
              completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void) -> Progress {
        let ctx = self.ctx
        if identifier == .trashContainer || identifier == .workingSet {
            completionHandler(nil, NSError.fileProviderErrorForNonExistentItem(withIdentifier: identifier))
            return Progress()
        }
        background {
            switch ctx.client.ask(ctx.request("item", item: FPIdentifiers.core(identifier)), as: FPItem.self) {
            case .success(let w): completionHandler(ctx.item(w), nil)
            case .failure(let f): completionHandler(nil, FPErrors.error(f, item: identifier))
            }
        }
        return Progress()
    }

    func fetchContents(for itemIdentifier: NSFileProviderItemIdentifier, version requestedVersion: NSFileProviderItemVersion?,
                       request: NSFileProviderRequest,
                       completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void) -> Progress {
        let (ctx, domain) = (self.ctx, self.domain)
        let progress = Progress(totalUnitCount: 1)
        let p = Unchecked(value: progress)
        background {
            let got = ctx.client.ask(ctx.request("fetch", item: FPIdentifiers.core(itemIdentifier)), as: FPFetched.self,
                                     progress: { done, total in
                                         p.value.totalUnitCount = Int64(max(total, 1))
                                         p.value.completedUnitCount = Int64(min(done, total))
                                     },
                                     opened: { conn in p.value.cancellationHandler = { conn.cancel() } })
            switch got {
            case .success(let f):
                let from = URL(fileURLWithPath: f.path)
                // The system takes the file from its temporary folder (on the domain's volume) and unlinks it.
                var url = from
                if let tmp = try? NSFileProviderManager(for: domain)?.temporaryDirectoryURL() {
                    let to = tmp.appendingPathComponent(UUID().uuidString)
                    if (try? FileManager.default.moveItem(at: from, to: to)) != nil {
                        url = to
                    } else if (try? FileManager.default.copyItem(at: from, to: to)) != nil {
                        try? FileManager.default.removeItem(at: from)
                        url = to
                    }
                }
                completionHandler(url, ctx.item(f.item), nil)
            case .failure(let f):
                completionHandler(nil, nil, FPErrors.error(p.value.isCancelled ? FPFailure(.cancelled, "Cancelled.") : f, item: itemIdentifier))
            }
        }
        return progress
    }

    func createItem(basedOn itemTemplate: NSFileProviderItem, fields: NSFileProviderItemFields, contents url: URL?,
                    options: NSFileProviderCreateItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        let ctx = self.ctx
        let type = itemTemplate.contentType ?? .data
        // Folders and packages are folders; a package's files come as items of their own.
        let kind = type.conforms(to: .directory) ? "dir" : type.conforms(to: .symbolicLink) ? "symlink" : "file"
        var req = ctx.request("create")
        req.parent = FPIdentifiers.core(itemTemplate.parentItemIdentifier)
        req.name = itemTemplate.filename
        req.kind = kind
        req.mayExist = options.contains(.mayAlreadyExist)
        background {
            var upload: URL?
            if kind == "file" {
                do { upload = try ctx.upload(url) } catch { return completionHandler(nil, [], false, error) }
                req.contents = upload?.lastPathComponent
            }
            defer { if let upload { try? FileManager.default.removeItem(at: upload) } }
            switch ctx.client.ask(req, as: FPItemAnswer.self) {
            case .success(let a): completionHandler(ctx.item(a.item), [], false, nil)
            case .failure(let f): completionHandler(nil, [], false, FPErrors.error(f))
            }
        }
        return Progress()
    }

    func modifyItem(_ item: NSFileProviderItem, baseVersion version: NSFileProviderItemVersion, changedFields: NSFileProviderItemFields,
                    contents newContents: URL?, options: NSFileProviderModifyItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        let ctx = self.ctx
        let id = item.itemIdentifier
        var req = ctx.request("modify", item: FPIdentifiers.core(id))
        if changedFields.contains(.filename) { req.name = item.filename }
        if changedFields.contains(.parentItemIdentifier) { req.parent = FPIdentifiers.core(item.parentItemIdentifier) }
        let content = changedFields.contains(.contents) && newContents != nil
        if content { req.base = FPPages.version(version) }
        background {
            var upload: URL?
            if content {
                do { upload = try ctx.upload(newContents) } catch { return completionHandler(nil, [], false, error) }
                req.contents = upload?.lastPathComponent
            }
            defer { if let upload { try? FileManager.default.removeItem(at: upload) } }
            switch ctx.client.ask(req, as: FPModified.self) {
            // A conflict copy (the content had moved on) comes as a new item through the working set; the item itself
            // fetches the other version again.
            case .success(let m): completionHandler(ctx.item(m.item), [], m.fetchContent, nil)
            case .failure(let f): completionHandler(nil, [], false, FPErrors.error(f, item: id))
            }
        }
        return Progress()
    }

    func deleteItem(identifier: NSFileProviderItemIdentifier, baseVersion version: NSFileProviderItemVersion,
                    options: NSFileProviderDeleteItemOptions = [], request: NSFileProviderRequest,
                    completionHandler: @escaping (Error?) -> Void) -> Progress {
        let ctx = self.ctx
        var req = ctx.request("delete", item: FPIdentifiers.core(identifier))
        req.base = FPPages.version(version)
        req.recursive = options.contains(.recursive)
        background {
            switch ctx.client.ask(req, as: FPDeleted.self) {
            case .success: completionHandler(nil)
            // A read-only member's delete frees the content instead: rejected, so the item comes back online only.
            case .failure(let f): completionHandler(f.kind == .noSuchItem ? nil : FPErrors.error(f, item: identifier))
            }
        }
        return Progress()
    }

    func enumerator(for containerItemIdentifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest) throws -> NSFileProviderEnumerator {
        switch containerItemIdentifier {
        case .workingSet: return WorkingSetEnumerator(ctx)
        case .trashContainer: return EmptyEnumerator(ctx)
        default: return FolderEnumerator(ctx, container: FPIdentifiers.core(containerItemIdentifier))
        }
    }

    /// The system's set of items on disk changed: the app compares it with the last one, and the core lets go of the
    /// files the system let go of (Remove Download, disk pressure).
    func materializedItemsDidChange(completionHandler: @escaping () -> Void) {
        let ctx = self.ctx
        guard let manager = NSFileProviderManager(for: domain) else { return completionHandler() }
        // Numbered by when it was read, so the app ignores a report that arrives after a later one.
        let report = UInt64(Date().timeIntervalSince1970 * 1_000_000_000)
        let collector = MaterializedSet(manager.enumeratorForMaterializedItems())
        let done = Unchecked(value: completionHandler)
        collector.collect { ids in
            background {
                guard let ids else { return done.value() }
                for req in FPRequest.materialized(folder: ctx.folder, ids: ids, report: report) {
                    if case .failure = ctx.client.ask(req, as: FPReleased.self) { break }
                }
                done.value()
            }
        }
    }

    // MARK: Finder actions (Info.plist: NSExtensionFileProviderActions)

    func performAction(identifier actionIdentifier: NSFileProviderExtensionActionIdentifier,
                       onItemsWithIdentifiers itemIdentifiers: [NSFileProviderItemIdentifier],
                       completionHandler: @escaping (Error?) -> Void) -> Progress {
        let ctx = self.ctx
        let on: Bool
        switch actionIdentifier.rawValue {
        case "io.github.kkollsga.Seedwell.keep": on = true
        case "io.github.kkollsga.Seedwell.unkeep": on = false
        default:
            completionHandler(NSError(domain: NSCocoaErrorDomain, code: NSFeatureUnsupportedError))
            return Progress()
        }
        background {
            for id in itemIdentifiers {
                var req = ctx.request("pin", item: FPIdentifiers.core(id))
                req.on = on
                if case .failure(let f) = ctx.client.ask(req, as: FPItemAnswer.self) {
                    return completionHandler(FPErrors.error(f, item: id))
                }
            }
            completionHandler(nil)
        }
        return Progress()
    }
}
