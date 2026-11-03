import Charts
import SwiftUI

/// The summary above a tracker's torrents (SPEC §7E.2), as the core words it (`trackerHeaderModel`): the speeds now, the
/// last 7 days' upload, download and ratio, the completed count and, when there is traffic, one small sparkline of the
/// last 30 days of upload. No axes, no dates: the caption says what it is.
struct TrackerHeader: View {
    let model: TrackerHeaderModel

    init(model: TrackerHeaderModel) { self.model = model }

    /// The header of a tracker's list: its totals and days, and its torrents' rates now (bytes/s).
    init(tracker: TrackerSummary, stats: TrackerStats, daily: [DailyStat], down: UInt64 = 0, up: UInt64 = 0) {
        model = trackerHeaderModel(stats: stats, daily: daily, down: down, up: up)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top, spacing: 16) {
                ForEach(Array(model.stats.enumerated()), id: \.offset) { _, s in stat(s) }
            }
            if !model.chart.isEmpty {
                VStack(alignment: .leading, spacing: 4) {
                    sparkline.frame(height: 30)
                    Text(model.chartCaption).font(Theme.caption).foregroundStyle(.tertiary)
                }
            }
        }
        .padding(.horizontal, 10)
        .padding(.top, 8)
        .padding(.bottom, 4)
    }

    private func stat(_ s: TrackerHeaderStat) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(s.value).font(Theme.title).monospacedDigit().lineLimit(1).minimumScaleFactor(0.7)
            Text(s.caption).font(Theme.secondary).foregroundStyle(.secondary)
            Text(s.sub ?? " ").font(Theme.caption).foregroundStyle(.tertiary).monospacedDigit()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
    }

    private var sparkline: some View {
        Chart(model.chart, id: \.daysAgo) { d in
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
        .chartYScale(domain: 0...(Double(max(1, model.chart.map(\.upBytes).max() ?? 1)) * 1.1))
        .accessibilityHidden(true)
    }
}
