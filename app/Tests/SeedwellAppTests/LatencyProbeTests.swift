import Foundation
import Testing
@testable import SeedwellApp

/// The QA latency probe (`ui latency`, QALatency) behind the big-folder scene's verdict.
@Suite("Latency probe", .timeLimit(.minutes(1)))
struct LatencyProbeTests {
    /// The failures: a QA shot (the renderer's capture) counted against the app, which would fail the scene on QA's
    /// own drawing; or the app's own work on the main thread right after it left out, which would pass a real stall.
    /// A stand-in main queue is held 300 ms in a capture, then 150 ms by the app's own work.
    @Test func aCaptureIsExcusedAndTheAppsOwnWorkCounts() async throws {
        let captures = RendererCaptures()
        let q = DispatchQueue(label: "test.main")
        let probe = LatencyProbe(captures: captures, queue: q)
        let reads = Reads()
        probe.start(reads: [("slow", { reads.slow() })], beatEvery: 0.010, readEvery: 0.020)
        try await Task.sleep(for: .milliseconds(60))
        q.async { captures.capture { Thread.sleep(forTimeInterval: 0.300) } }
        q.async { Thread.sleep(forTimeInterval: 0.150) }
        try await Task.sleep(for: .milliseconds(800))
        probe.stop()
        let r = probe.report()
        let raw = r.beats.map(\.ms).max() ?? 0
        let own = r.worst?.ownMs ?? 0
        #expect(raw >= 400, "a beat waited through both: \(raw) ms")
        #expect(own >= 140, "the app's own 150 ms counts: \(own) ms")
        #expect(own <= raw - 250, "the capture's 300 ms is excused: own \(own) of \(raw) ms")
        #expect(r.over(100) > 0 && r.excusedBeats > 0)
        #expect((r.calls["slow"]?.max() ?? 0) >= 30, "a slow core read shows: \(r.calls)")
        #expect(r.lines.contains { $0.hasPrefix("latency-json: {") })
    }

    @Test func quantilesAreNearestRank() {
        #expect(LatencyProbe.Report.quantile([], 0.99) == 0)
        #expect(LatencyProbe.Report.quantile([5], 0.99) == 5)
        #expect(LatencyProbe.Report.quantile(Array(1...100).map(Double.init), 0.99) == 99)
        #expect(LatencyProbe.Report.quantile(Array(1...200).map(Double.init), 0.99) == 198)
    }
}

/// A core read that takes 40 ms.
private final class Reads: Sendable {
    func slow() { Thread.sleep(forTimeInterval: 0.040) }
}
