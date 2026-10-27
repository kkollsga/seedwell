import Foundation

/// Human-readable text for sizes, rates and times. The wording is the core's (`trs_control::display`, the same in
/// every host app); this only forwards, and keeps the time of day, which follows the user's own clock style.
enum Fmt {
    static func bytes(_ n: UInt64) -> String { fmtBytes(n: n) }
    static func liveRate(_ n: UInt64, _ arrow: String) -> String { fmtLiveRate(n: n, arrow: arrow) }
    static func rate(_ n: UInt64) -> String { fmtRate(n: n) }
    static func progress(done: UInt64, total: UInt64) -> String { fmtProgress(done: done, total: total) }
    static func middleTruncated(_ text: String, to limit: Int) -> String { fmtMiddleTruncated(text: text, limit: UInt32(max(0, limit))) }
    static func remaining(_ secs: UInt64?, short: Bool = false) -> String { fmtRemaining(secs: secs, compact: short) }
    static func ago(_ unix: UInt64, short: Bool = false, now: Date = .now) -> String {
        fmtAgo(unix: unix, now: UInt64(max(0, now.timeIntervalSince1970)), compact: short)
    }
    static func duration(_ secs: UInt64) -> String { fmtDuration(secs: secs) }
    static func until(_ secs: UInt64) -> String { fmtUntil(secs: secs) }

    /// "22:29": the time of day that is `secs` from now (in the user's own clock style).
    static func clock(in secs: UInt64, now: Date = .now) -> String {
        now.addingTimeInterval(TimeInterval(secs)).formatted(date: .omitted, time: .shortened)
    }

    /// "22:29" or "Mon 22:29" for a unix time (seconds): when something began or will happen.
    static func clockAt(_ unix: UInt64, now: Date = .now) -> String {
        let d = Date(timeIntervalSince1970: TimeInterval(unix))
        if Calendar.current.isDate(d, inSameDayAs: now) { return d.formatted(date: .omitted, time: .shortened) }
        return d.formatted(.dateTime.weekday(.abbreviated).hour().minute())
    }

    static func inFuture(_ unix: UInt64, now: Date = .now) -> String { fmtInFuture(unix: unix, now: UInt64(max(0, now.timeIntervalSince1970))) }
    static func peers(connected: UInt32, known: UInt32) -> String { fmtPeers(connected: connected, known: known) }
    static let peersHelp = fmtPeersHelp()
    static func count(_ n: some BinaryInteger, _ noun: String) -> String { fmtCount(n: UInt64(max(0, Int64(n))), noun: noun) }
    static func ratio(_ r: Double) -> String { fmtRatio(r: r) }
    static func goal(_ r: Double) -> String { fmtGoal(r: r) }
}
