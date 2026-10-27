import Charts
import SwiftUI

/// The summary above a tracker's torrents (SPEC §7E.2): the last 7 days' upload, download and ratio, the completed count and, when there is traffic,
/// one small sparkline of the last 30 days of upload. No axes, no dates: the caption says what it is.
struct TrackerHeader: View {
    let tracker: TrackerSummary
    let stats: TrackerStats
    let daily: [DailyStat]
    /// The tracker's torrents moving now, bytes/s.
    var down: UInt64 = 0
    var up: UInt64 = 0

    /// Uploaded, downloaded and their ratio over today and the six days before it.
    static func lastWeek(_ daily: [DailyStat]) -> (up: UInt64, down: UInt64, ratio: Double) {
        let week = daily.filter { $0.daysAgo < 7 }
        let up = week.reduce(0) { $0 + $1.upBytes }, down = week.reduce(0) { $0 + $1.downBytes }
        return (up, down, down == 0 ? 0 : Double(up) / Double(down))
    }
    private var week: (up: UInt64, down: UInt64, ratio: Double) { Self.lastWeek(daily) }
    private var hasTraffic: Bool { daily.contains { $0.upBytes > 0 } }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top, spacing: 16) {
                stat(Fmt.liveRate(down, "↓"), "Download speed", sub: nil)
                stat(Fmt.liveRate(up, "↑"), "Upload speed", sub: nil)
                stat(Fmt.bytes(week.up), "Uploaded", sub: "Last 7 days")
                stat(Fmt.bytes(week.down), "Downloaded", sub: "Last 7 days")
                stat(Fmt.ratio(week.ratio), "Ratio", sub: "Last 7 days")
                stat("\(stats.torrentsCompleted)", "Torrents completed", sub: nil)
            }
            if hasTraffic {
                VStack(alignment: .leading, spacing: 4) {
                    sparkline.frame(height: 30)
                    Text("Upload, last 30 days").font(.system(size: 11)).foregroundStyle(.tertiary)
                }
            }
        }
        .padding(.horizontal, 10)
        .padding(.top, 8)
        .padding(.bottom, 4)
    }

    private func stat(_ value: String, _ caption: String, sub: String?) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(value).font(.system(size: 20, weight: .semibold)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.7)
            Text(caption).font(.system(size: 12)).foregroundStyle(.secondary)
            Text(sub ?? " ").font(.system(size: 11)).foregroundStyle(.tertiary).monospacedDigit()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
    }

    private var sparkline: some View {
        Chart(daily, id: \.daysAgo) { d in
            AreaMark(x: .value("Day", -Int(d.daysAgo)), y: .value("Uploaded", d.upBytes))
                .foregroundStyle(Color.accentColor.opacity(0.18))
                .interpolationMethod(.monotone)
            LineMark(x: .value("Day", -Int(d.daysAgo)), y: .value("Uploaded", d.upBytes))
                .foregroundStyle(Color.accentColor.opacity(0.85))
                .lineStyle(StrokeStyle(lineWidth: 1.5, lineCap: .round))
                .interpolationMethod(.monotone)
        }
        .chartXAxis(.hidden)
        .chartYAxis(.hidden)
        .chartLegend(.hidden)
        .chartYScale(domain: 0...(Double(max(1, daily.map(\.upBytes).max() ?? 1)) * 1.1))
        .accessibilityHidden(true)
    }
}
