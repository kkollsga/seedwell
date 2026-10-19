import Foundation

/// The quiet "what is the tracker side doing right now" line of Get Info (SPEC 8.3). Worked out from
/// absolute times, so a ticking view needs no new data: the words match the plain-language problems.
enum TrackerLive {
    /// "tracker.example" for a (masked) announce URL; empty when it has no host.
    static func host(_ url: String) -> String { URL(string: url)?.host ?? "" }

    static func text(_ t: TrackerLine, now: Date = .now) -> String {
        let at = UInt64(max(0, now.timeIntervalSince1970))
        let host = host(t.url)
        switch t.state {
        case "backoff":
            guard let retry = t.retryAt else { return "Tracker not responding" }
            if retry <= at { return "Retrying tracker now…" }
            return "Retrying tracker in \(Fmt.until(retry - at)) (attempt \(t.failures + 1))"
        case "halted":
            return "Announcing stopped: the tracker refused this torrent. It is asked again when you ask."
        case "working", "warning":
            let next = t.nextAnnounce.map { " · next announce \(Fmt.clockAt($0, now: now))" } ?? ""
            return "Tracker OK" + next
        default:
            return host.isEmpty ? "Announcing to the tracker…" : "Announcing to \(host)…"
        }
    }

    /// While a user's ask waits for the tracker's own minimum interval: when it allows the next one.
    static func allowedAt(_ t: TrackerLine, now: Date = .now) -> UInt64? {
        guard let ok = t.manualOkAt, ok > UInt64(now.timeIntervalSince1970) else { return nil }
        return ok
    }
}
