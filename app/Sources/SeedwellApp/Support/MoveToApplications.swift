import AppKit

/// Opened straight from its disk image or from Downloads, Seedwell offers to move itself to Applications. The rule
/// and the words are the core's (`move_to_applications_offer`); this gives it the facts only the host sees and does
/// the move: a copy in Applications first, then (when asked) the app already there to the Trash (its data in
/// Application Support is never touched), the copy in its place without the download's quarantine mark, and this
/// one quits while a small helper waits for it, ejects the disk image and opens the copy.
@MainActor
enum MoveToApplications {
    /// Asks once, at launch; `quit` ends this run the normal way (trackers hear `stopped`).
    static func offer(quit: @escaping () -> Void) {
        let fm = FileManager.default
        let bundle = Bundle.main.bundleURL.standardizedFileURL
        let target = applicationsFolder().appendingPathComponent(bundle.lastPathComponent).standardizedFileURL
        guard target.path != bundle.path else { return }
        let readOnly = (try? bundle.resourceValues(forKeys: [.volumeIsReadOnlyKey]).volumeIsReadOnly) ?? false
        let installed: String? = fm.fileExists(atPath: target.path)
            ? (Bundle(url: target)?.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "")
            : nil
        let place = AppPlace(path: bundle.path, home: NSHomeDirectory(), readOnlyVolume: readOnly, installedVersion: installed)
        guard let offer = moveToApplicationsOffer(placeOf: place) else { return }

        NSApp.activate()
        let alert = NSAlert()
        alert.messageText = offer.title
        alert.informativeText = offer.text
        alert.addButton(withTitle: offer.moveTitle)
        alert.addButton(withTitle: offer.notNowTitle)
        guard alert.runModal() == .alertFirstButtonReturn else { return }

        do {
            try move(bundle, to: target, replacing: offer.replaces)
            try relaunch(target, eject: diskImageVolume(of: bundle, readOnly: readOnly))
        } catch {
            let failed = NSAlert()
            failed.messageText = offer.title
            failed.informativeText = moveToApplicationsFailed(reason: error.localizedDescription)
            failed.runModal()
            return
        }
        quit()
    }

    /// /Applications, or the user's own Applications folder when this user may not write there (as install.sh does).
    static func applicationsFolder() -> URL {
        if FileManager.default.isWritableFile(atPath: "/Applications") { return URL(fileURLWithPath: "/Applications") }
        let own = URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Applications")
        try? FileManager.default.createDirectory(at: own, withIntermediateDirectories: true)
        return own
    }

    /// The mounted disk image the app runs from (`/Volumes/<name>`), to eject once it has quit; `nil` elsewhere (and
    /// for a translocated copy, whose disk image's place macOS hides).
    static func diskImageVolume(of bundle: URL, readOnly: Bool) -> String? {
        let parts = bundle.path.split(separator: "/", omittingEmptySubsequences: true)
        guard readOnly, parts.count >= 3, parts[0] == "Volumes" else { return nil }
        return "/Volumes/\(parts[1])"
    }

    /// The copy is made beside the target first, so a failed copy leaves both apps as they were; the old app goes to
    /// the Trash only once the copy is complete.
    static func move(_ bundle: URL, to target: URL, replacing: Bool) throws {
        let fm = FileManager.default
        let staging = target.deletingLastPathComponent().appendingPathComponent(".\(target.lastPathComponent).moving-\(UUID().uuidString)")
        do {
            try fm.copyItem(at: bundle, to: staging)
            try clearQuarantine(staging)
            if fm.fileExists(atPath: target.path) {
                guard replacing else { throw CocoaError(.fileWriteFileExists) }
                try fm.trashItem(at: target, resultingItemURL: nil)
            }
            try fm.moveItem(at: staging, to: target)
        } catch {
            try? fm.removeItem(at: staging)
            throw error
        }
        // A copy from Downloads (not translocated, so its place is known) is a move: the original goes to the Trash.
        let source = bundle.path
        if !source.contains("/AppTranslocation/"), source.hasPrefix(NSHomeDirectory() + "/Downloads/"), fm.isDeletableFile(atPath: source) {
            try? fm.trashItem(at: bundle, resultingItemURL: nil)
        }
    }

    /// The copy was checked by Gatekeeper when this one opened (it is signed and notarized); without the download's
    /// mark it opens from Applications directly, not from a translocated copy.
    private static func clearQuarantine(_ app: URL) throws {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/xattr")
        p.arguments = ["-dr", "com.apple.quarantine", app.path]
        try p.run()
        p.waitUntilExit()
    }

    /// A helper outlives this app: it waits for it to end, ejects the disk image (when there is one) and opens the
    /// moved copy. The values are arguments, never script text.
    private static func relaunch(_ app: URL, eject volume: String?) throws {
        let script = #"while kill -0 "$1" 2>/dev/null; do sleep 0.2; done; [ -z "$3" ] || hdiutil detach "$3" -quiet || true; open "$2""#
        let log = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("seedwell-move.log")
        try Spawn.detached(argv: ["/bin/sh", "-c", script, "seedwell-move", String(getpid()), app.path, volume ?? ""], log: log)
    }
}
