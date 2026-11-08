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

@Suite("Hang watchdog", .timeLimit(.minutes(1)))
struct HangWatchdogTests {
    @Test func aLateBeatIsAStallAndAnOnTimeOneIsNot() {
        var d = HangDetector(report: 1.0, fail: nil)
        #expect(d.turn(now: 0) == .ping)
        #expect(d.turn(now: 0.25) == .wait)
        #expect(d.beat(now: 0.3) == nil, "on time")
        #expect(d.turn(now: 0.5) == .ping)
        #expect(d.turn(now: 5) == .wait, "no failing outside a test run")
        #expect(d.beat(now: 1.9) == .stall(ms: 1400), "1.4 s late")
        #expect(d.beat(now: 2) == nil, "a beat nobody asked for")
    }

    /// Beats `late` seconds late, one per ping of 0.25 s from `from` for `seconds`: what each beat was said to be.
    private func beats(_ d: inout HangDetector, from: Double, seconds: Double, late: Double) -> [HangDetector.Late] {
        var said: [HangDetector.Late] = []
        var t = from
        while t < from + seconds {
            #expect(d.turn(now: t) == .ping)
            if let l = d.beat(now: t + late) { said.append(l) }
            // The next ping at the watch thread's next turn after the beat.
            t += (late / 0.25).rounded(.down) * 0.25 + 0.25
        }
        return said
    }

    /// The failure this targets: **the main thread held up again and again in stalls each shorter than a second**, the
    /// app frozen for the user while the watchdog, which noted only single stalls of a second or more, said nothing
    /// (the Share window after adding a 914 GB folder, 2026-11-06). Beats 0.8 s late in a row are a stutter, said once
    /// for its span with how much of it was held up, never a stall.
    @Test func shortStallsInARowAreAStutter() {
        var d = HangDetector(report: 1.0, fail: 2.0, stutterWindow: 3, busyShare: 0.6)
        let said = beats(&d, from: 0, seconds: 3, late: 0.8)
        #expect(said.count == 1, "\(said)")
        guard case .stutter(let busy, let window) = said.first else {
            Issue.record("not a stutter: \(said)")
            return
        }
        #expect(busy >= 1800 && busy <= 3000 && window == 3000, "\(said)")
        // Said once: the next one needs a span of its own.
        let next = beats(&d, from: 3, seconds: 0.9, late: 0.8)
        #expect(next.isEmpty, "\(next)")
    }

    /// The main thread busy for less than the share (beats 0.1 s late every quarter second: 40 %) or briefly is no
    /// stutter, however long it goes on; nor are on-time beats.
    @Test func shortStallsWithRoomBetweenAreNotAStutter() {
        var d = HangDetector(report: 1.0, fail: 2.0, stutterWindow: 3, busyShare: 0.6)
        #expect(beats(&d, from: 0, seconds: 20, late: 0.1).isEmpty)
        #expect(beats(&d, from: 20, seconds: 20, late: 0).isEmpty)
        #expect(beats(&d, from: 40, seconds: 1.2, late: 0.8).isEmpty, "1.6 s held up: under 60 % of 3 s")
    }

    /// A long stall is said as a stall, and its time is not said again as part of a stutter (1.5 s of stall and 0.6 s of
    /// short stalls after it would be 70 % of 3 s).
    @Test func aStallIsNotAlsoAStutter() {
        var d = HangDetector(report: 1.0, fail: nil, stutterWindow: 3, busyShare: 0.6)
        let said = beats(&d, from: 1, seconds: 0.25, late: 1.5) + beats(&d, from: 2.5, seconds: 0.75, late: 0.2)
        #expect(said == [.stall(ms: 1500)], "\(said)")
    }

    /// The failure this targets: **a snapshot render or QA run failing as a stutter while it does exactly what it was
    /// asked**: the renderer draws its window at 2x every 50 ms until two drawings match, render after render, and under
    /// load each drawing took 350–400 ms, most of every 3 s, so the run ended as a stutter. A render's own captures are
    /// not a stutter, but a stall between them still is: the app holding the main thread between two captures is
    /// counted as before.
    @Test func aRendersOwnCapturesAreNotAStutterButAStallBetweenThemStillIs() {
        var d = HangDetector(report: 1.0, fail: 2.0, stutterWindow: 3, busyShare: 0.6)
        var own: [(from: Double, to: Double)] = []
        var said: [HangDetector.Late] = []
        // Captures of 0.4 s with 50 ms between, for 6 s: a ping lands in each, its beat waits for the capture's end.
        var t = 0.0
        while t < 6 {
            own.append((t, t + 0.4))
            #expect(d.turn(now: t + 0.05) == .ping)
            if let l = d.beat(now: t + 0.4, excused: own) { said.append(l) }
            t += 0.45
        }
        #expect(said.isEmpty, "the renderer's own captures were said: \(said)")
        // Between the captures (50 ms each now), the app holds the main thread for 0.35 s.
        while t < 12 {
            own.append((t, t + 0.05))
            #expect(d.turn(now: t + 0.01) == .ping)
            if let l = d.beat(now: t + 0.4, excused: own) { said.append(l) }
            t += 0.45
        }
        #expect(said.contains { if case .stutter = $0 { true } else { false } }, "a stutter between captures went unsaid: \(said)")
        // A capture as long as a stall is still a stall (the single-stall rule leaves nothing out).
        #expect(d.turn(now: 20) == .ping)
        #expect(d.beat(now: 21.5, excused: own + [(19.9, 21.5)]) == .stall(ms: 1500))
    }

    @Test func aTestRunFailsOnceAStallReachesItsLimit() {
        var d = HangDetector(report: 1.0, fail: 2.0)
        #expect(d.turn(now: 10) == .ping)
        #expect(d.turn(now: 11.5) == .wait)
        #expect(d.turn(now: 12.25) == .fail(ms: 2250))
        #expect(d.turn(now: 12.5) == .wait, "said once")
        #expect(d.beat(now: 13) == .stall(ms: 3000))
    }

    /// The real watchdog (its thread, its pings): a queue held for 0.4 s is reported once, with the stall's length and
    /// the last UI action; on-time beats are not.
    @Test func aHeldQueueIsReportedWithTheLastAction() async throws {
        let q = DispatchQueue(label: "watched")
        let seen = Seen()
        let wd = HangWatchdog(policy: UiWatchdog(pingMs: 20, reportMs: 150, failMs: 400, stutterWindowMs: 3_000, stutterBusyPct: 60), failing: false, queue: q,
                              lastAction: { seen.action }, onLate: { late, action in if case .stall(let ms) = late { seen.add(ms, action) } })
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
    /// queue is still held (the real `onFail` ends the process). The watch thread runs at utility priority, which a
    /// busy test run may wake late (it once missed a fixed half second entirely): the test waits for the report as long
    /// as the queue stays held, and the hold marks its end, so a report that came after it names that instead.
    @Test func aTestRunThatHangsFailsWhileItHangs() async throws {
        let q = DispatchQueue(label: "watched")
        let failed = Seen()
        let wd = HangWatchdog(policy: UiWatchdog(pingMs: 20, reportMs: 100, failMs: 250, stutterWindowMs: 3_000, stutterBusyPct: 60), failing: true, queue: q,
                              lastAction: { failed.action }, onLate: { _, _ in }, onFail: { late, action in if case .stall(let ms) = late { failed.add(ms, action) } })
        wd.start()
        defer { wd.stop() }
        try await Task.sleep(for: .milliseconds(80))
        let hold = 3.0
        q.async {
            failed.mark("render test")
            Thread.sleep(forTimeInterval: hold)
            failed.mark("released")
        }
        let start = ContinuousClock.now
        while failed.all.isEmpty, ContinuousClock.now - start < .seconds(hold - 0.2) {
            try await Task.sleep(for: .milliseconds(20))
        }
        let f = failed.all
        #expect(f.count == 1, "told while the queue is still held: \(f)")
        #expect((f.first?.ms ?? 0) >= 250 && f.first?.action == "render test", "\(f)")
    }

    /// The real watchdog in a snapshot render or QA run: a queue held again and again for 0.3 s (each hold shorter than a
    /// stall, a beat getting through between two) is a stutter, told as one and failing the run, and never a stall. The
    /// time counted is what the beats waited, which a watch thread woken late only makes smaller: the holds fill the
    /// queue, so even then most of it is counted.
    @Test func aTestRunThatStuttersFails() async throws {
        let q = DispatchQueue(label: "watched")
        let seen = Seen()
        let failed = Seen()
        let wd = HangWatchdog(policy: UiWatchdog(pingMs: 20, reportMs: 1_000, failMs: 2_000, stutterWindowMs: 1_000, stutterBusyPct: 60),
                              failing: true, queue: q, lastAction: { seen.action },
                              onLate: { late, action in if case .stall(let ms) = late { seen.add(ms, action) } },
                              onFail: { late, action in if case .stutter = late { failed.add(Self.ms(late), action) } })
        wd.start()
        defer { wd.stop() }
        // Each hold queues the next as it ends, behind the beat that waited for it.
        let end = Date().addingTimeInterval(4)
        func hold() {
            q.async {
                seen.mark("test: stutter")
                Thread.sleep(forTimeInterval: 0.3)
                if Date() < end { hold() }
            }
        }
        hold()
        let start = ContinuousClock.now
        while failed.all.isEmpty, ContinuousClock.now - start < .seconds(4) {
            try await Task.sleep(for: .milliseconds(20))
        }
        let f = failed.all
        #expect(f.count >= 1 && f.first?.action == "test: stutter" && (f.first?.ms ?? 0) >= 600, "\(f)")
        #expect(seen.all.isEmpty, "a hold was said as a stall: \(seen.all)")
    }

    /// The real watchdog in a snapshot render or QA run, with the renderer's captures marked as `SnapshotWindow.settled`
    /// marks them: a queue held again and again by captures (0.3 s each) does not fail the run, while the same queue held
    /// between short captures by other work does. The failure this targets: a render's captures ending the run as a
    /// stutter, or the exclusion swallowing the app's own stutter along with them.
    @Test func aTestRunIsNotFailedByTheRenderersCapturesButIsByAStutterBetweenThem() async throws {
        let q = DispatchQueue(label: "watched")
        let seen = Seen()
        let failed = Seen()
        let captures = RendererCaptures()
        let wd = HangWatchdog(policy: UiWatchdog(pingMs: 20, reportMs: 1_000, failMs: 2_000, stutterWindowMs: 1_000, stutterBusyPct: 60),
                              failing: true, queue: q, captures: captures, lastAction: { seen.action },
                              onLate: { _, _ in }, onFail: { late, action in failed.add(Self.ms(late), action) })
        wd.start()
        defer { wd.stop() }
        // Each hold queues the next as it ends, behind the beat that waited for it.
        func hold(until end: Date, capture: Double, other: Double, action: String) {
            q.async {
                seen.mark(action)
                captures.capture { Thread.sleep(forTimeInterval: capture) }
                Thread.sleep(forTimeInterval: other)
                if Date() < end { hold(until: end, capture: capture, other: other, action: action) }
            }
        }
        hold(until: Date().addingTimeInterval(2.5), capture: 0.3, other: 0, action: "test: captures")
        try await Task.sleep(for: .seconds(2.6))
        #expect(failed.all.isEmpty, "the renderer's captures failed the run: \(failed.all)")
        hold(until: Date().addingTimeInterval(4), capture: 0.03, other: 0.3, action: "test: stutter between captures")
        let start = ContinuousClock.now
        while failed.all.isEmpty, ContinuousClock.now - start < .seconds(4) {
            try await Task.sleep(for: .milliseconds(20))
        }
        let f = failed.all
        #expect(f.count >= 1 && f.first?.action == "test: stutter between captures", "\(f)")
    }

    /// A late beat's length as `Seen` keeps it: a stall's, or a stutter's time held up.
    private static func ms(_ late: HangDetector.Late) -> UInt64 {
        switch late {
        case .stall(let ms): ms
        case .stutter(let busy, _): busy
        }
    }

    @Test func theLinesAreTheCores() {
        #expect(uiStallLine(stalledMs: 1_420, lastAction: "Pause", failing: false) == "the UI thread did not answer for 1.4 s (last action: Pause)")
        #expect(uiStallLine(stalledMs: 2_000, lastAction: "render x", failing: true).hasSuffix("so this run fails"))
    }
}
