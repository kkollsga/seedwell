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

    /// A beat later than this is a stall (seconds).
    let report: Double
    /// A stall this long fails the run (`nil`: never).
    let fail: Double?
    /// When the beat on its way was asked for.
    private(set) var askedAt: Double?
    private var failed = false

    init(report: Double, fail: Double?) {
        self.report = report
        self.fail = fail
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

    /// The main thread gave the beat at `now`: how long it was held up, when that is a stall (milliseconds).
    mutating func beat(now: Double) -> UInt64? {
        guard let asked = askedAt else { return nil }
        askedAt = nil
        failed = false
        let late = now - asked
        return late >= report ? UInt64(late * 1000) : nil
    }
}

/// Notices a main thread that stops answering: the app must never hang. A thread of its own asks the main queue for a
/// beat every `ping_ms` of the core's `uiWatchdog()` (menus and modal alerts drain it, so they count as answering); a beat
/// later than `report_ms` is a stall, told to `onStall` with its length when it ends. With `failing`
/// (snapshot renders and QA runs), a stall that reaches `fail_ms` goes to `onFail` at once, from the watch thread: a hang
/// may never end. Cheap: one block every quarter second.
final class HangWatchdog: @unchecked Sendable {
    private let lock = NSLock()
    private var detector: HangDetector
    private var running = false
    private let ping: Double
    /// The queue watched: the main queue (tests: a queue of their own, so they never hold the real main thread).
    private let queue: DispatchQueue
    /// What the user (or QA, or the renderer) did last.
    private let lastAction: @Sendable () -> String
    private let onStall: @Sendable (_ ms: UInt64, _ action: String) -> Void
    private let onFail: @Sendable (_ ms: UInt64, _ action: String) -> Void

    init(policy: UiWatchdog = uiWatchdog(), failing: Bool, queue: DispatchQueue = .main,
         lastAction: @escaping @Sendable () -> String = { UIAction.current },
         onStall: @escaping @Sendable (_ ms: UInt64, _ action: String) -> Void,
         onFail: @escaping @Sendable (_ ms: UInt64, _ action: String) -> Void = HangWatchdog.endRun) {
        ping = Double(policy.pingMs) / 1000
        self.queue = queue
        self.lastAction = lastAction
        detector = HangDetector(report: Double(policy.reportMs) / 1000, fail: failing ? Double(policy.failMs) / 1000 : nil)
        self.onStall = onStall
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
                onFail(ms, lastAction())
            }
            Thread.sleep(forTimeInterval: ping)
        }
    }

    /// On the watched queue (the main thread): the beat.
    private func beat() {
        lock.lock()
        let late = detector.beat(now: CFAbsoluteTimeGetCurrent())
        lock.unlock()
        if let late { onStall(late, lastAction()) }
    }

    /// A test run that hangs ends here, with the reason on stderr and a failing status.
    @Sendable static func endRun(_ ms: UInt64, _ action: String) {
        let line = "seedwell: " + uiStallLine(stalledMs: ms, lastAction: action, failing: true) + "\n"
        FileHandle.standardError.write(Data(line.utf8))
        Logger(subsystem: "io.github.kkollsga.Seedwell", category: "hang").fault("\(line, privacy: .public)")
        // The main thread is held: end the process from here, without the exit handlers that might wait for it.
        _exit(70)
    }
}
