import FileProvider
import Foundation
import SeedwellFileProviderShared

/// Items per page asked of the app.
let pageSize = 500

/// A folder's children, in the app's pages. Changes come through the working set (a replicated extension's system
/// asks only it), so a folder's own change enumeration only moves its anchor along.
final class FolderEnumerator: NSObject, NSFileProviderEnumerator {
    let ctx: Context
    let container: String

    init(_ ctx: Context, container: String) {
        self.ctx = ctx
        self.container = container
    }

    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        let (ctx, container, o) = (ctx, container, Unchecked(value: observer))
        background {
            var req = ctx.request("children", item: container)
            req.cursor = FPPages.token(page)
            req.limit = pageSize
            switch ctx.client.ask(req, as: FPPage.self) {
            case .success(let p):
                o.value.didEnumerate(p.items.map(ctx.item))
                o.value.finishEnumerating(upTo: p.next.map(FPPages.page))
            case .failure(let f):
                o.value.finishEnumeratingWithError(FPErrors.error(f, item: FPIdentifiers.provider(container)))
            }
        }
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        currentAnchor(ctx) { now, error in
            if let now { observer.finishEnumeratingChanges(upTo: now, moreComing: false) } else { observer.finishEnumeratingWithError(error!) }
        }
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        currentAnchor(ctx) { now, _ in completionHandler(now) }
    }
}

/// The working set: every item of the folder (the extension keeps no list of what the system has on disk, so every
/// change is reported), and what changed since an anchor from the core's change feed.
final class WorkingSetEnumerator: NSObject, NSFileProviderEnumerator {
    let ctx: Context

    init(_ ctx: Context) { self.ctx = ctx }

    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        let (ctx, o) = (ctx, Unchecked(value: observer))
        background {
            var req = ctx.request("changes")
            req.anchor = FPPages.token(page) ?? "0"
            req.limit = pageSize
            switch ctx.client.ask(req, as: FPChanges.self) {
            case .success(let c):
                o.value.didEnumerate(c.items.map(ctx.item))
                o.value.finishEnumerating(upTo: c.more ? FPPages.page(c.anchor) : nil)
            case .failure(let f):
                o.value.finishEnumeratingWithError(FPErrors.error(f))
            }
        }
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        let (ctx, o) = (ctx, Unchecked(value: observer))
        background {
            var req = ctx.request("changes")
            req.anchor = FPPages.anchor(anchor)
            req.limit = pageSize
            switch ctx.client.ask(req, as: FPChanges.self) {
            case .success(let c):
                if !c.items.isEmpty { o.value.didUpdate(c.items.map(ctx.item)) }
                if !c.deleted.isEmpty { o.value.didDeleteItems(withIdentifiers: c.deleted.map(FPIdentifiers.provider)) }
                o.value.finishEnumeratingChanges(upTo: FPPages.anchor(c.anchor), moreComing: c.more)
            case .failure(let f):
                // An anchor from before the folder's index was made anew: the system lists everything again.
                o.value.finishEnumeratingWithError(FPErrors.error(f))
            }
        }
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        currentAnchor(ctx) { now, _ in completionHandler(now) }
    }
}

/// The trash: Seedwell keeps none in the domain (a delete is a delete, on every device; the Finder's Trash is outside).
final class EmptyEnumerator: NSObject, NSFileProviderEnumerator {
    let ctx: Context

    init(_ ctx: Context) { self.ctx = ctx }

    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        observer.finishEnumerating(upTo: nil)
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        observer.finishEnumeratingChanges(upTo: anchor, moreComing: false)
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        currentAnchor(ctx) { now, _ in completionHandler(now) }
    }
}

/// The folder's anchor for "now" (off the caller's queue).
func currentAnchor(_ ctx: Context, _ done: @escaping (NSFileProviderSyncAnchor?, Error?) -> Void) {
    background {
        switch ctx.client.ask(ctx.request("anchor"), as: FPAnchor.self) {
        case .success(let a): done(FPPages.anchor(a.anchor), nil)
        case .failure(let f): done(nil, FPErrors.error(f))
        }
    }
}
