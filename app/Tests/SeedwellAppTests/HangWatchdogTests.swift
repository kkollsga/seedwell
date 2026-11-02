import Foundation
import Testing
@testable import SeedwellApp

/// What a watchdog saw, from whichever thread told it.
private final class Seen: @unchecked Sendable {
    private let lock = NSLock()
    private var items: [(ms: UInt64, action: String)] = []

    func add(_ ms: UInt64, _ action: String) {
        lock.lock()
        items.append((ms, action))
        lock.unlock()
    }

    var all: [(ms: UInt64, action: String)] {
        lock.lock()
        defer { lock.unlock() }
        return items
    }

    /// The test's own "last UI action" (UIAction is the app's, which other suites mark meanwhile).
    private var marked = ""

    func mark(_ action: String) {
        lock.lock()
        marked = action
        lock.unlock()
    }

    var action: String {
        lock.lock()
        defer { lock.unlock() }
        return marked
    }
}

/// A stand-in for the main thread: the watchdog watches this queue, and a test holds it as long work would hold the
/// main thread (the real one stays free for the other suites, whose timing a held main thread would upset).
private func held(_ q: DispatchQueue, seconds: Double, marking action: String, in last: Seen) {
    q.async {
        last.mark(action)
        Thread.sleep(forTimeInterval: seconds)
    }
}

@Suite("Hang watchdog")
struct HangWatchdogTests {
    @Test func aLateBeatIsAStallAndAnOnTimeOneIsNot() {
        var d = HangDetector(report: 1.0, fail: nil)
        #expect(d.turn(now: 0) == .ping)
        #expect(d.turn(now: 0.25) == .wait)
        #expect(d.beat(now: 0.3) == nil, "on time")
        #expect(d.turn(now: 0.5) == .ping)
        #expect(d.turn(now: 5) == .wait, "no failing outside a test run")
        #expect(d.beat(now: 1.9) == 1400, "1.4 s late")
        #expect(d.beat(now: 2) == nil, "a beat nobody asked for")
    }

    @Test func aTestRunFailsOnceAStallReachesItsLimit() {
        var d = HangDetector(report: 1.0, fail: 2.0)
        #expect(d.turn(now: 10) == .ping)
        #expect(d.turn(now: 11.5) == .wait)
        #expect(d.turn(now: 12.25) == .fail(ms: 2250))
        #expect(d.turn(now: 12.5) == .wait, "said once")
        #expect(d.beat(now: 13) == 3000)
    }

    /// The real watchdog (its thread, its pings): a queue held for 0.4 s is reported once, with the stall's length and
    /// the last UI action; on-time beats are not.
    @Test func aHeldQueueIsReportedWithTheLastAction() async throws {
        let q = DispatchQueue(label: "watched")
        let seen = Seen()
        let wd = HangWatchdog(policy: UiWatchdog(pingMs: 20, reportMs: 150, failMs: 400), failing: false, queue: q,
                              lastAction: { seen.action }, onStall: { ms, action in seen.add(ms, action) })
        wd.start()
        defer { wd.stop() }
        try await Task.sleep(for: .milliseconds(150))
        #expect(seen.all.isEmpty, "a free queue answers in time")
        held(q, seconds: 0.4, marking: "test: hold the queue", in: seen)
        try await Task.sleep(for: .milliseconds(700))
        let s = seen.all
        #expect(s.count == 1, "\(s)")
        // A stall is measured from the ping that waited for it. The watch thread sleeps between pings, and the system
        // may wake a background thread's timer late (it coalesces them: under load the ping went out 150 ms into the
        // hold, and 252 ms of the 400 were measured), so the length is between the report limit and the hold.
        let ms = s.first?.ms ?? 0
        #expect(ms >= 150 && ms <= 600 && s.first?.action == "test: hold the queue", "\(s)")
    }

    /// A snapshot render or QA run: a stall that reaches the limit is told at once, from the watch thread, while the
    /// queue is still held (the real `onFail` ends the process).
    @Test func aTestRunThatHangsFailsWhileItHangs() async throws {
        let q = DispatchQueue(label: "watched")
        let failed = Seen()
        let wd = HangWatchdog(policy: UiWatchdog(pingMs: 20, reportMs: 100, failMs: 250), failing: true, queue: q,
                              lastAction: { failed.action }, onStall: { _, _ in }, onFail: { ms, action in failed.add(ms, action) })
        wd.start()
        defer { wd.stop() }
        try await Task.sleep(for: .milliseconds(80))
        held(q, seconds: 0.8, marking: "render test", in: failed)
        try await Task.sleep(for: .milliseconds(500))
        let f = failed.all
        #expect(f.count == 1, "told while the queue is still held: \(f)")
        #expect((f.first?.ms ?? 0) >= 250 && f.first?.action == "render test", "\(f)")
    }

    @Test func theLinesAreTheCores() {
        #expect(uiStallLine(stalledMs: 1_420, lastAction: "Pause", failing: false) == "the UI thread did not answer for 1.4 s (last action: Pause)")
        #expect(uiStallLine(stalledMs: 2_000, lastAction: "render x", failing: true).hasSuffix("so this run fails"))
    }
}
