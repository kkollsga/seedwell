import Foundation

/// First launch only: the port Transmission listens on, offered as Seedwell's own (the router may
/// already forward it). Transmission's preferences are only ever READ: no code here, or anywhere
/// in the app, writes to them.
///
/// The decision is taken once, on a fresh state folder, and remembered in two small files of that
/// folder so that the offer survives until the user takes it (Settings ▸ Network) and is never
/// made again on a later launch.
enum TransmissionPort {
    /// `~/Library/Preferences/org.m0k.transmission.plist` of the home in use. Isolated runs
    /// (`--home-root`) look below their own home, never the real one.
    static func defaultPrefs() -> URL {
        let home = ProcessInfo.processInfo.environment["HOME"] ?? NSHomeDirectory()
        return URL(fileURLWithPath: home).appendingPathComponent("Library/Preferences/org.m0k.transmission.plist")
    }

    /// `BindPort` of a Transmission preferences file (XML or binary plist), `nil` when the file is
    /// missing, unreadable, or holds no usable port. Read-only.
    static func readBindPort(_ prefs: URL) -> UInt16? {
        guard let data = try? Data(contentsOf: prefs, options: .mappedIfSafe),
              let plist = try? PropertyListSerialization.propertyList(from: data, options: [], format: nil) as? [String: Any],
              let n = (plist["BindPort"] as? NSNumber)?.intValue,
              (1024...65535).contains(n)
        else { return nil }
        return UInt16(n)
    }

    private static func offerFile(_ stateDir: String) -> URL { URL(fileURLWithPath: stateDir).appendingPathComponent("transmission-port-offer") }
    private static func checkedFile(_ stateDir: String) -> URL { URL(fileURLWithPath: stateDir).appendingPathComponent("transmission-port-checked") }

    /// Call before the core starts. On the first launch of a fresh state folder it looks at
    /// `prefs` (the default location unless a path is given, which QA and tests do) and remembers
    /// what it found. Returns the offer that is still open, if any.
    @discardableResult
    static func prepare(stateDir: String, prefs: URL? = nil) -> UInt16? {
        let fm = FileManager.default
        let fresh = !fm.fileExists(atPath: URL(fileURLWithPath: stateDir).appendingPathComponent("state.redb").path)
        if fresh, !fm.fileExists(atPath: checkedFile(stateDir).path) {
            try? Data().write(to: checkedFile(stateDir))
            if let port = readBindPort(prefs ?? defaultPrefs()) {
                try? "\(port)\n".write(to: offerFile(stateDir), atomically: true, encoding: .utf8)
            }
        }
        return pending(stateDir: stateDir)
    }

    /// The open offer: remembered on the first launch and not yet taken.
    static func pending(stateDir: String) -> UInt16? {
        guard let s = try? String(contentsOf: offerFile(stateDir), encoding: .utf8),
              let n = UInt16(s.trimmingCharacters(in: .whitespacesAndNewlines)), n > 0 else { return nil }
        return n
    }

    /// The offer was taken (or the port was set to it by hand): it is not shown again.
    static func clear(stateDir: String) {
        try? FileManager.default.removeItem(at: offerFile(stateDir))
    }
}
