import Charts
import SwiftUI

/// The summary above a tracker's torrents (SPEC §7E.2): four labelled totals and, when there is traffic,
/// one small sparkline of the last 30 days of upload. No axes, no dates: the caption says what it is.
struct TrackerHeader: View {
    let tracker: TrackerSummary
    let stats: TrackerStats
    let daily: [DailyStat]

    private var ratio: Double { stats.lifetimeDown == 0 ? 0 : Double(stats.lifetimeUp) / Double(stats.lifetimeDown) }
    private var week: ArraySlice<DailyStat> { daily.suffix(7) }
    private var hasTraffic: Bool { daily.contains { $0.upBytes > 0 } }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top, spacing: 16) {
                // An automatic tracker keeps no daily history: its totals are its torrents' own.
                stat(Fmt.bytes(stats.lifetimeUp), "Uploaded", sub: daily.isEmpty ? nil : "\(Fmt.bytes(week.reduce(0) { $0 + $1.upBytes })) this week")
                stat(Fmt.bytes(stats.lifetimeDown), "Downloaded", sub: daily.isEmpty ? nil : "\(Fmt.bytes(week.reduce(0) { $0 + $1.downBytes })) this week")
                stat(Fmt.ratio(ratio), "Ratio", sub: "all time")
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
            Text(value).font(.system(size: 20, weight: .semibold)).monospacedDigit().lineLimit(1)
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
