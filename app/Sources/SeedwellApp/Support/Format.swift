import Foundation

/// Human-readable text for sizes, rates and times. Wording follows the concepts.
enum Fmt {
    /// Decimal units like Finder ("6.2 GB", "412 MB"); locale-independent because the UI is English.
    static func bytes(_ n: UInt64) -> String {
        let units = ["bytes", "KB", "MB", "GB", "TB"]
        var value = Double(n)
        var k = 0
        while value >= 1000, k < units.count - 1 { value /= 1000; k += 1 }
        if k == 0 { return "\(n) bytes" }
        return String(format: value < 10 ? "%.1f %@" : "%.0f %@", value, units[k])
    }

    static func rate(_ n: UInt64) -> String {
        n == 0 ? "0 KB/s" : bytes(max(n, 1_000)) + "/s"
    }

    /// "4.6 of 6.2 GB": both numbers in the unit of the total.
    static func progress(done: UInt64, total: UInt64) -> String {
        let scale: Double = total >= 1_000_000_000_000 ? 1e12 : total >= 1_000_000_000 ? 1e9 : total >= 1_000_000 ? 1e6 : 1e3
        let d = Double(done) / scale
        return String(format: d < 10 ? "%.1f" : "%.0f", d) + " of " + bytes(total)
    }

    private static func span(_ secs: UInt64, style: DateComponentsFormatter.UnitsStyle) -> String {
        let f = DateComponentsFormatter()
        f.allowedUnits = [.day, .hour, .minute]
        f.maximumUnitCount = 1
        f.unitsStyle = style
        var cal = Calendar(identifier: .gregorian)
        cal.locale = Locale(identifier: "en_US")
        f.calendar = cal
        // Round up to whole minutes so "90 s left" reads "2 min", never "1 min".
        return f.string(from: TimeInterval((max(secs, 60) + 59) / 60 * 60)) ?? ""
    }

    /// `text` cut in the middle with an ellipsis when longer than `limit` characters.
    static func middleTruncated(_ text: String, to limit: Int) -> String {
        guard text.count > limit, limit > 2 else { return text }
        let head = (limit - 1 + 1) / 2, tail = limit - 1 - head
        return String(text.prefix(head)) + "…" + String(text.suffix(tail))
    }

    /// "2 minutes remaining" (list) or "2 min left" (menu).
    static func remaining(_ secs: UInt64?, short: Bool = false) -> String {
        guard let secs else { return short ? "calculating…" : "Calculating time remaining…" }
        if short { return span(secs, style: .short) + " left" }
        return secs < 60 ? "Less than a minute remaining" : span(secs, style: .full) + " remaining"
    }

    /// "12 minutes ago" (list) or "12 min ago" (menu).
    static func ago(_ unix: UInt64, short: Bool = false, now: Date = .now) -> String {
        let secs = UInt64(max(0, now.timeIntervalSince1970 - TimeInterval(unix)))
        if secs < 60 { return "just now" }
        if secs >= 86_400 { return secs < 2 * 86_400 ? "Yesterday" : "\(secs / 86_400) days ago" }
        return span(secs, style: short ? .short : .full) + " ago"
    }

    /// Seed time as one number and unit: "12 minutes", "9 days", "2 years".
    static func duration(_ secs: UInt64) -> String {
        let (n, unit): (UInt64, String) =
            if secs >= 365 * 86_400 { (secs / (365 * 86_400), "year") }
            else if secs >= 86_400 { (secs / 86_400, "day") }
            else if secs >= 3_600 { (secs / 3_600, "hour") }
            else { (max(1, secs / 60), "minute") }
        return "\(n) \(unit)\(n == 1 ? "" : "s")"
    }

    /// "2 h", "15 min", "3 days": how long until something happens.
    static func until(_ secs: UInt64) -> String {
        if secs >= 2 * 86_400 { return "\(secs / 86_400) days" }
        if secs >= 3_600 { let h = (secs + 1_799) / 3_600; return "\(h) h" }
        return "\(max(1, (secs + 59) / 60)) min"
    }

    /// "in 28 min", "due now": when a unix time (seconds) is ahead.
    static func inFuture(_ unix: UInt64, now: Date = .now) -> String {
        let secs = Int64(unix) - Int64(now.timeIntervalSince1970)
        if secs <= 30 { return "due now" }
        return "in " + until(UInt64(secs))
    }

    /// "(4 of 250)" with a swarm size, "(4 peers)" without.
    static func peers(connected: UInt32, swarm: UInt32?) -> String {
        guard let swarm else { return "(\(connected) peer\(connected == 1 ? "" : "s"))" }
        return "(\(connected) of \(max(swarm, connected)))"
    }

    /// "1 torrent", "3 torrents".
    static func count(_ n: some BinaryInteger, _ noun: String) -> String { "\(n) \(noun)\(n == 1 ? "" : "s")" }
    static func ratio(_ r: Double) -> String { String(format: "%.2f", r) }
    static func goal(_ r: Double) -> String { String(format: "%.1f", r) }
}
