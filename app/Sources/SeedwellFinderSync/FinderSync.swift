import AppKit
import FinderSync
import os
import SeedwellFinderShared

/// Seedwell's Finder extension (SPEC 7G.5): badges on synced folders and their items, and "Show in Seedwell"
/// in their shortcut menu. It runs sandboxed in its own process and knows only what the app writes to the
/// status file (`FinderStatus`); it never touches the folders. Finder calls in on any thread, so the state is
/// behind a lock and Finder is told about changes on the main thread.
@objc(SeedwellFinderSync)
final class FinderSync: FIFinderSync, @unchecked Sendable {
    private let dir: URL
    private let appID: String
    private let lock = NSLock()
    private var badges = FinderBadges(nil)
    private var status: FinderStatus?
    /// The items Finder has asked about and what they show, so a change can update them.
    private var shown: [String: FinderBadge?] = [:]
    private var watch: DispatchSourceFileSystemObject?
    private var timer: DispatchSourceTimer?
    private let log = Logger(subsystem: "io.github.kkollsga.Seedwell", category: "finder-extension")

    override init() {
        let info = Bundle.main.infoDictionary ?? [:]
        dir = URL(fileURLWithPath: Self.expand(info["SeedwellStatusDir"] as? String ?? "~/Library/Application Support/Seedwell/Finder"))
        // The app's id is the extension's without its last part.
        appID = (Bundle.main.bundleIdentifier ?? "io.github.kkollsga.Seedwell.FinderSync").components(separatedBy: ".").dropLast().joined(separator: ".")
        super.init()
        let c = FIFinderSyncController.default()
        for b in FinderBadge.allCases {
            c.setBadgeImage(b.image, label: b.label, forBadgeIdentifier: b.rawValue)
        }
        reload()
        startWatching()
    }

    /// `~` is the real home: inside the sandbox `NSHomeDirectory()` is the extension's container.
    private static func expand(_ path: String) -> String {
        guard path.hasPrefix("~/") else { return path }
        let home = getpwuid(getuid()).flatMap { String(cString: $0.pointee.pw_dir) } ?? NSHomeDirectory()
        return home + path.dropFirst(1)
    }

    // MARK: The status file

    /// The app replaces the file atomically, so the folder is watched (a write there is a new file). A slow
    /// timer covers the app having crashed without saying so.
    private func startWatching() {
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let fd = open(dir.path, O_EVTONLY)
        if fd >= 0 {
            let src = DispatchSource.makeFileSystemObjectSource(fileDescriptor: fd, eventMask: [.write, .rename, .delete], queue: .global(qos: .utility))
            src.setEventHandler { [weak self] in self?.reload() }
            src.setCancelHandler { close(fd) }
            src.resume()
            watch = src
        }
        let t = DispatchSource.makeTimerSource(queue: .global(qos: .utility))
        t.schedule(deadline: .now() + 30, repeating: 30)
        t.setEventHandler { [weak self] in self?.reload() }
        t.resume()
        timer = t
    }

    private func reload() {
        var s = FinderStatus.read(from: dir)
        if s?.running == true, NSRunningApplication.runningApplications(withBundleIdentifier: appID).isEmpty {
            s?.running = false  // it quit without saying so
        }
        lock.lock()
        guard s != status else { lock.unlock(); return }
        status = s
        log.info("status: \(s?.folders.count ?? 0) folders, app running \(s?.running ?? false)")
        let b = FinderBadges(s)
        badges = b
        var changed: [(String, FinderBadge?)] = []
        for (path, old) in shown {
            let new = b.badge(for: path)
            if new != old { shown.updateValue(new, forKey: path); changed.append((path, new)) }
        }
        lock.unlock()
        let roots = Set(b.roots.map { URL(fileURLWithPath: $0, isDirectory: true) })
        DispatchQueue.main.async {
            let c = FIFinderSyncController.default()
            if c.directoryURLs != roots { c.directoryURLs = roots }
            for (path, badge) in changed {
                c.setBadgeIdentifier(badge?.rawValue ?? "", for: URL(fileURLWithPath: path))
            }
        }
    }

    // MARK: Finder

    override func requestBadgeIdentifier(for url: URL) {
        let path = url.path
        lock.lock()
        let badge = badges.badge(for: path)
        shown.updateValue(badge, forKey: path)
        lock.unlock()
        log.debug("badge \(badge?.rawValue ?? "none", privacy: .public) for \(url.lastPathComponent)")
        FIFinderSyncController.default().setBadgeIdentifier(badge?.rawValue ?? "", for: url)
    }

    override func endObservingDirectory(at url: URL) {
        // Its items are out of sight: forget them (a subfolder still open keeps its own).
        let dir = url.path.hasSuffix("/") ? url.path : url.path + "/"
        lock.lock()
        shown = shown.filter { path, _ in !(path.hasPrefix(dir) && !path.dropFirst(dir.count).contains("/")) }
        lock.unlock()
    }

    override func menu(for menuKind: FIMenuKind) -> NSMenu? {
        let f = menuKind == .toolbarItemMenu ? nil : folder(of: clicked)
        log.info("menu \(menuKind.rawValue, privacy: .public): \(f == nil ? "no synced folder" : f!.canShare ? "share" : "show only", privacy: .public)")
        guard let f else { return nil }
        let menu = NSMenu(title: "")
        let show = NSMenuItem(title: "Show in Seedwell", action: #selector(showInSeedwell(_:)), keyEquivalent: "")
        show.target = self
        menu.addItem(show)
        if f.canShare {
            let share = NSMenuItem(title: "Share “\(f.name)”…", action: #selector(share(_:)), keyEquivalent: "")
            share.target = self
            menu.addItem(share)
        }
        return menu
    }

    @objc private func showInSeedwell(_ sender: Any?) {
        openInApp("")
    }

    @objc private func share(_ sender: Any?) {
        openInApp("/share")
    }

    /// Opens `seedwell://folder/<id><suffix>` for the folder of the item the menu was opened on.
    private func openInApp(_ suffix: String) {
        guard let f = folder(of: clicked),
              let url = URL(string: "seedwell://folder/\(f.id)\(suffix)") else { return }
        NSWorkspace.shared.open(url)
    }

    /// The item the menu is for: the selected one for a right-click on items, else the folder clicked in.
    private var clicked: URL? {
        let c = FIFinderSyncController.default()
        return c.selectedItemURLs()?.first ?? c.targetedURL()
    }

    /// The synced folder an item is in (paths compared as for the badges: Unicode form, the data volume's path).
    private func folder(of url: URL?) -> FinderStatus.Folder? {
        guard let path = url?.path else { return nil }
        lock.lock()
        defer { lock.unlock() }
        return badges.folder(containing: path)
    }
}
