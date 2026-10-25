import AppKit
import FinderSync
import os
import SeedwellFinderShared

/// Keeps the Finder extension's status file current (SPEC 7G.5): every two seconds it builds the status from the
/// sync snapshot and rewrites the file when it changed. On quit it says the app is not running, so Finder's badges
/// do not claim a sync that no longer happens.
@MainActor final class FinderStatusWriter {
    let core: Core
    /// `<state>/Finder`: for the app's own state, `~/Library/Application Support/Seedwell/Finder`, the one folder
    /// the extension's sandbox may read.
    let dir: URL
    /// Called once, on the first tick that sees a synced folder (the app may offer to turn the extension on).
    var onFirstFolder: (() -> Void)?
    private var timer: Timer?
    private var last: FinderStatus?
    private let log = Logger(subsystem: "io.github.kkollsga.Seedwell", category: "finder")

    init(core: Core) {
        self.core = core
        dir = URL(fileURLWithPath: core.stateDir()).appendingPathComponent("Finder", isDirectory: true)
    }

    /// The user has switched the extension on in System Settings.
    static var extensionEnabled: Bool { FIFinderSyncController.isExtensionEnabled }

    func start() {
        tick()
        timer = Timer.scheduledTimer(withTimeInterval: 2, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.tick() }
        }
    }

    func stop() {
        timer?.invalidate()
        timer = nil
        write(FinderStatus(running: false, folders: last?.folders ?? []))
    }

    private func tick() {
        let snap = core.syncSnapshot()
        let core = self.core
        write(Self.status(snap, activity: { core.syncActivity(id: $0) }))
        if !snap.folders.isEmpty, let first = onFirstFolder {
            onFirstFolder = nil
            first()
        }
    }

    private func write(_ s: FinderStatus) {
        guard s != last else { return }
        do {
            try s.write(to: dir)
            last = s
        } catch {
            log.error("finder status: \(error.localizedDescription, privacy: .public)")
        }
    }

    /// The status for a snapshot. Pure apart from resolving symlinks in the roots (Finder hands the extension
    /// real paths, see `realPath`).
    nonisolated static func status(_ s: SyncSnapshot, activity: (String) -> SyncActivity) -> FinderStatus {
        let folders = s.folders.map { f -> FinderStatus.Folder in
            let a = activity(f.id)
            return FinderStatus.Folder(
                id: f.id, name: f.name, path: realPath(f.path),
                badge: badge(f), pending: a.pending, pendingMore: a.pendingMore, conflicts: a.conflicts,
                issues: f.issues.map(\.path).filter { !$0.isEmpty }, complete: !a.ownIgnores, canShare: SyncText.canShare(f.role))
        }
        return FinderStatus(running: s.running, folders: folders)
    }

    /// The path with symlinks resolved, as Finder names it. Not `resolvingSymlinksInPath`, which turns
    /// `/private/tmp` into `/tmp`. A missing folder keeps its path.
    nonisolated static func realPath(_ path: String) -> String {
        guard let p = realpath(path, nil) else { return path }
        defer { free(p) }
        return String(cString: p)
    }

    /// A folder's own badge.
    nonisolated static func badge(_ f: SyncFolder) -> FinderBadge {
        let b: FinderBadge
        switch f.state {
        case .paused: return .paused
        case .missing, .error: return .error
        case .starting, .scanning: b = .syncing
        case .waitingForDevices, .waitingForAdmin: b = .offline
        case .syncing:
            let (down, up) = (f.rateIn > 0 || f.needFiles > 0, f.rateOut > 0)
            b = down && up ? .syncing : down ? .downloading : up ? .uploading : .syncing
        case .upToDate:
            b = f.rateOut > 0 ? .uploading : f.connected == 0 ? .offline : .synced
        }
        // A conflict waits for the user: it shows unless something is moving right now.
        return f.conflicts > 0 && [.synced, .uploading, .offline].contains(b) ? .conflict : b
    }
}
