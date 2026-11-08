import Foundation
import os

/// The last thing the user, QA or the snapshot renderer asked the UI to do: a stall report names it.
enum UIAction {
    private static let lock = NSLock()
    nonisolated(unsafe) private static var last = "launch"

    static func mark(_ what: String) {
        lock.lock()
        last = what
        lock.unlock()
    }

    static var current: String {
        lock.lock()
        defer { lock.unlock() }
        return last
    }
}

/// When the main thread was busy with the renderer's own captures (`SnapshotWindow.settled`: a window drawn at 2x every
/// 50 ms until two drawings match, each a few hundred milliseconds under load): the work a snapshot render or a QA shot
/// was asked to do, not the app holding the user up, so the watchdog leaves it out of a stutter. A stall is a stall all
/// the same, and the app's own work between two captures still counts. Times in `CFAbsoluteTimeGetCurrent` seconds.
final class RendererCaptures: @unchecked Sendable {
    /// The renderer's and QA's (the app's watchdog reads it).
    static let shared = RendererCaptures()

    private let lock = NSLock()
    private var spans: [(from: Double, to: Double)] = []
    /// A capture running now (since then), and how deep (a capture inside one is the same span).
    private var open: Double?
    private var depth = 0

    /// Runs `work` (on the main thread) as one of the renderer's captures.
    func capture<T>(_ work: () throws -> T) rethrows -> T {
        let now = CFAbsoluteTimeGetCurrent()
        lock.lock()
        if depth == 0 { open = now }
        depth += 1
        lock.unlock()
        defer {
            let end = CFAbsoluteTimeGetCurrent()
            lock.lock()
            depth -= 1
            if depth == 0, let from = open {
                open = nil
                spans.append((from, end))
                // A minute is more than any stutter window looks back.
                spans.removeAll { $0.to < end - 60 }
            }
            lock.unlock()
        }
        return try work()
    }

    /// The captures that ran after `since`, up to `now` (one still running: until `now`).
    func spans(since: Double, now: Double) -> [(from: Double, to: Double)] {
        lock.lock()
        defer { lock.unlock() }
        var all = spans.filter { $0.to > since }
        if let from = open { all.append((from, now)) }
        return all
    }
}

/// When a beat of the main thread came late: the watchdog's bookkeeping, apart from threads and clocks (tests drive it
/// with their own times, in seconds).
struct HangDetector {
    enum Turn: Equatable {
        /// Ask the main thread for a beat.
        case ping
        /// A beat is on its way.
        case wait
        /// The main thread has not answered for this many milliseconds: the run fails.
        case fail(ms: UInt64)
    }

    /// What late beats came to: one stall, or many short ones in a row.
    enum Late: Equatable {
        /// The main thread did not answer for this many milliseconds.
        case stall(ms: UInt64)
        /// The main thread was held up for `busyMs` of the last `windowMs`, in stalls each too short to be one: the app
        /// stutters (it feels frozen) while no single stall shows.
        case stutter(busyMs: UInt64, windowMs: UInt64)

        /// The core's line for it ("the UI thread did not answer for 1.4 s (last action: Pause)").
        func line(lastAction: String, failing: Bool) -> String {
            switch self {
            case .stall(let ms): uiStallLine(stalledMs: ms, lastAction: lastAction, failing: failing)
            case .stutter(let busy, let window): uiStutterLine(busyMs: busy, windowMs: window, lastAction: lastAction, failing: failing)
            }
        }
    }

    /// A beat later than this is a stall (seconds).
    let report: Double
    /// A stall this long fails the run (`nil`: never).
    let fail: Double?
    /// A stutter: held up for `busyShare` (0…1) of any `stutterWindow` seconds in shorter stalls.
    let stutterWindow: Double
    let busyShare: Double
    /// When the beat on its way was asked for.
    private(set) var askedAt: Double?
    private var failed = false
    /// The short stalls in the last `stutterWindow`, each from its ping to its beat: only the time the main thread
    /// surely was busy (a beat waiting for it) is counted, never more.
    private var held: [(from: Double, to: Double)] = []

    init(report: Double, fail: Double?, stutterWindow: Double = 3, busyShare: Double = 0.6) {
        self.report = report
        self.fail = fail
        self.stutterWindow = stutterWindow
        self.busyShare = busyShare
    }

    /// The watch thread's turn at `now`.
    mutating func turn(now: Double) -> Turn {
        guard let asked = askedAt else {
            askedAt = now
            return .ping
        }
        if let fail, !failed, now - asked >= fail {
            failed = true
            return .fail(ms: UInt64((now - asked) * 1000))
        }
        return .wait
    }

    /// The main thread gave the beat at `now`: a stall (this beat was that late), a stutter (the short stalls of the
    /// last `stutterWindow` add up to `busyShare` of it: said once, the next one needs a span of its own), or nothing.
    /// `excused`: when the main thread was busy with the renderer's own captures (`RendererCaptures`), left out of a
    /// stutter's time; a stall counts whole.
    mutating func beat(now: Double, excused: [(from: Double, to: Double)] = []) -> Late? {
        guard let asked = askedAt else { return nil }
        askedAt = nil
        failed = false
        let late = now - asked
        if late >= report {
            // Said as a stall: the time around it is not said again as a stutter.
            held.removeAll()
            return .stall(ms: UInt64(late * 1000))
        }
        guard late > 0 else { return nil }
        let start = now - stutterWindow
        held.append((asked, now))
        held.removeAll { $0.to <= start }
        let busy = held.reduce(0) { sum, h in
            let from = max(h.from, start)
            // The captures in it (they never overlap: each runs on the main thread).
            let own = excused.reduce(0) { $0 + max(0, min(h.to, $1.to) - max(from, $1.from)) }
            return sum + max(0, h.to - from - own)
        }
        guard busy >= busyShare * stutterWindow else { return nil }
        held.removeAll()
        return .stutter(busyMs: UInt64((busy * 1000).rounded()), windowMs: UInt64((stutterWindow * 1000).rounded()))
    }
}

/// Notices a main thread that stops answering, or keeps stopping: the app must never hang or stutter. A thread of its
/// own asks the main queue for a beat every `ping_ms` of the core's `uiWatchdog()` (menus and modal alerts drain it, so
/// they count as answering); a beat later than `report_ms` is a stall, told to `onLate` with its length when it ends;
/// short stalls that hold the main thread up for `stutter_busy_pct` of `stutter_window_ms` are a stutter, told the same
/// way; the main thread's time in the renderer's own captures (`captures`) is left out of a stutter. With `failing`
/// (snapshot renders and QA runs), a stall that reaches `fail_ms` goes to `onFail` at once, from the watch thread (a
/// hang may never end), and so does a stutter, from the main thread. Cheap: one block every quarter second.
final class HangWatchdog: @unchecked Sendable {
    private let lock = NSLock()
    private var detector: HangDetector
    private var running = false
    private let ping: Double
    private let failing: Bool
    /// The renderer's captures (not a stutter), and how far back a stutter looks.
    private let captures: RendererCaptures
    private let stutterWindow: Double
    /// The queue watched: the main queue (tests: a queue of their own, so they never hold the real main thread).
    private let queue: DispatchQueue
    /// What the user (or QA, or the renderer) did last.
    private let lastAction: @Sendable () -> String
    private let onLate: @Sendable (_ late: HangDetector.Late, _ action: String) -> Void
    private let onFail: @Sendable (_ late: HangDetector.Late, _ action: String) -> Void

    init(policy: UiWatchdog = uiWatchdog(), failing: Bool, queue: DispatchQueue = .main, captures: RendererCaptures = .shared,
         lastAction: @escaping @Sendable () -> String = { UIAction.current },
         onLate: @escaping @Sendable (_ late: HangDetector.Late, _ action: String) -> Void,
         onFail: @escaping @Sendable (_ late: HangDetector.Late, _ action: String) -> Void = HangWatchdog.endRun) {
        ping = Double(policy.pingMs) / 1000
        self.failing = failing
        self.queue = queue
        self.captures = captures
        stutterWindow = Double(policy.stutterWindowMs) / 1000
        self.lastAction = lastAction
        detector = HangDetector(report: Double(policy.reportMs) / 1000, fail: failing ? Double(policy.failMs) / 1000 : nil,
                                stutterWindow: Double(policy.stutterWindowMs) / 1000, busyShare: Double(policy.stutterBusyPct) / 100)
        self.onLate = onLate
        self.onFail = onFail
    }

    func start() {
        lock.lock()
        defer { lock.unlock() }
        guard !running else { return }
        running = true
        let t = Thread { [self] in watch() }
        t.name = "seedwell-hang-watchdog"
        t.qualityOfService = .utility
        t.start()
    }

    /// Stops watching (the app quits: its last wait is the engine's, on purpose).
    func stop() {
        lock.lock()
        running = false
        lock.unlock()
    }

    private func watch() {
        while true {
            lock.lock()
            guard running else { lock.unlock(); return }
            let turn = detector.turn(now: CFAbsoluteTimeGetCurrent())
            lock.unlock()
            switch turn {
            case .ping:
                // The main queue: drained by the main run loop in its common modes (an open menu, a modal alert) and
                // by any other driver of the main thread.
                queue.async { [self] in beat() }
            case .wait:
                break
            case .fail(let ms):
                onFail(.stall(ms: ms), lastAction())
            }
            Thread.sleep(forTimeInterval: ping)
        }
    }

    /// On the watched queue (the main thread): the beat.
    private func beat() {
        let now = CFAbsoluteTimeGetCurrent()
        let own = captures.spans(since: now - stutterWindow, now: now)
        lock.lock()
        let late = detector.beat(now: now, excused: own)
        lock.unlock()
        guard let late else { return }
        let action = lastAction()
        onLate(late, action)
        // A long stall already failed the run from the watch thread; a stutter is known only now.
        if failing, case .stutter = late { onFail(late, action) }
    }

    /// A test run that hangs or stutters ends here, with the reason on stderr and a failing status.
    @Sendable static func endRun(_ late: HangDetector.Late, _ action: String) {
        let line = "seedwell: " + late.line(lastAction: action, failing: true) + "\n"
        FileHandle.standardError.write(Data(line.utf8))
        Logger(subsystem: "io.github.kkollsga.Seedwell", category: "hang").fault("\(line, privacy: .public)")
        // The main thread may be held: end the process from here, without the exit handlers that might wait for it.
        _exit(70)
    }
}
