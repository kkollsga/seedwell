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
    static func until(_ secs: UInt64) -> String { fmtUntil(secs: secs) }

    /// "1 Nov" (with the year when it is not this year), in the user's own date style: the day the core's sync and chat
    /// lines name ("Link until 1 Nov").
    static func day(_ unix: UInt64, now: Date = .now) -> String {
        let d = Date(timeIntervalSince1970: TimeInterval(unix))
        let sameYear = Calendar.current.isDate(d, equalTo: now, toGranularity: .year)
        return d.formatted(sameYear ? .dateTime.day().month(.abbreviated) : .dateTime.day().month(.abbreviated).year())
    }

    static func count(_ n: some BinaryInteger, _ noun: String) -> String { fmtCount(n: UInt64(max(0, Int64(n))), noun: noun) }
    static func ratio(_ r: Double) -> String { fmtRatio(r: r) }
    static func goal(_ r: Double) -> String { fmtGoal(r: r) }
}

extension Date {
    /// Unix milliseconds: the clock the core's models take.
    var unixMs: UInt64 { UInt64(max(0, timeIntervalSince1970 * 1000)) }
}

/// The core's `LocalClock`: the time of day and the day in the user's own style (the core decides which form a text
/// uses: the time of day on the same day, else the weekday and the time). The day is `Fmt.day`'s format.
final class MacClock: LocalClock, @unchecked Sendable {
    func timeOfDay(unixS: UInt64) -> String {
        Date(timeIntervalSince1970: TimeInterval(unixS)).formatted(date: .omitted, time: .shortened)
    }

    func weekdayTime(unixS: UInt64) -> String {
        Date(timeIntervalSince1970: TimeInterval(unixS)).formatted(.dateTime.weekday(.abbreviated).hour().minute())
    }

    func day(unixS: UInt64, withYear: Bool) -> String {
        let d = Date(timeIntervalSince1970: TimeInterval(unixS))
        return d.formatted(withYear ? .dateTime.day().month(.abbreviated).year() : .dateTime.day().month(.abbreviated))
    }

    /// "Nov 1, 2026 at 10:29 PM": Get Info's Added and Finished.
    func dateTime(unixS: UInt64) -> String {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US")
        f.dateStyle = .medium
        f.timeStyle = .short
        return f.string(from: Date(timeIntervalSince1970: TimeInterval(unixS)))
    }

    func utcOffsetS(unixS: UInt64) -> Int32 {
        Int32(TimeZone.current.secondsFromGMT(for: Date(timeIntervalSince1970: TimeInterval(unixS))))
    }
}
