import Foundation

/// QA only (`ui latency start | stop | report`): a probe fine enough to show that the app stays interactive, where the
/// hang watchdog (a beat every 250 ms, a stall from 1 s) only catches a frozen app.
///
/// - **The main thread.** A thread of its own posts a beat to the main queue every 10 ms (the queue the watchdog uses,
///   which the main run loop drains in its common modes: an open menu and a modal alert count as answering) and notes
///   how late each beat ran, and the UI action at that moment (`UIAction.current`). The part of a beat's delay that the
///   main thread spent in the renderer's own captures (`RendererCaptures`: a QA shot) is told apart and not counted:
///   that is QA's drawing, not the app holding the user up.
/// - **The core.** Another thread calls, every 50 ms, the reads the UI's refresh makes on the main thread (`snapshot`:
///   the window and the menu; `syncSnapshot`: the Folder Sync rows) and notes each round trip. A core lock held for
///   seconds shows here although the UI thread never waits for it: the UI's data would go stale meanwhile.
///
/// While it runs it holds an activity that keeps App Nap away (a quiet QA run is never the active app, and a napping
/// app's timers come late): the user's app is active while they click through it.
final class LatencyProbe: @unchecked Sendable {
    /// The app's (QAControl's).
    static let shared = LatencyProbe()

    /// One beat: when it was posted (seconds since start), how late it ran, the part of that the app's own work took
    /// (`ownMs`: without the renderer's captures), and the UI action as it ran.
    struct Beat: Equatable {
        let at: Double
        let ms: Double
        let ownMs: Double
        let action: String
        /// The main thread's CPU time since the beat before it ran: of a late beat, how much the main thread really
        /// worked; the rest it waited (for a CPU on a loaded Mac, or blocked).
        var ranMs: Double = 0
    }

    private let lock = NSLock()
    /// Each start is a new run: the threads of an older one stop, and their late beats are dropped.
    private var run = 0
    private var running = false
    private var started = 0.0
    private var ended: Double?
    private var beats: [Beat] = []
    private var calls: [String: [Double]] = [:]
    private var activity: NSObjectProtocol?
    /// The main thread's CPU clock at the last beat it ran (ns).
    private var lastCpu: UInt64 = 0
    private let captures: RendererCaptures
    /// The main thread's queue (tests: one of their own).
    private let queue: DispatchQueue

    init(captures: RendererCaptures = .shared, queue: DispatchQueue = .main) {
        self.captures = captures
        self.queue = queue
    }

    /// Starts a new run (one running is ended): beats every `beatEvery`, the core's reads (`reads`, by name) every
    /// `readEvery`.
    func start(reads: [(String, @Sendable () -> Void)], beatEvery: Double = 0.010, readEvery: Double = 0.050) {
        lock.lock()
        run += 1
        let me = run
        running = true
        started = CFAbsoluteTimeGetCurrent()
        ended = nil
        beats = []
        beats.reserveCapacity(8_000)
        lastCpu = 0
        calls = Dictionary(uniqueKeysWithValues: reads.map { ($0.0, []) })
        if activity == nil {
            activity = ProcessInfo.processInfo.beginActivity(options: [.userInitiated, .latencyCritical], reason: "QA latency probe")
        }
        lock.unlock()

        let beat = Thread { [self] in
            while alive(me) {
                let sent = CFAbsoluteTimeGetCurrent()
                queue.async { [self] in served(me, sent: sent) }
                Thread.sleep(forTimeInterval: beatEvery)
            }
        }
        beat.name = "seedwell-qa-latency-beat"
        beat.qualityOfService = .userInteractive
        beat.start()

        guard !reads.isEmpty else { return }
        let core = Thread { [self] in
            while alive(me) {
                for (name, read) in reads {
                    let t0 = CFAbsoluteTimeGetCurrent()
                    read()
                    let ms = (CFAbsoluteTimeGetCurrent() - t0) * 1000
                    lock.lock()
                    if run == me, running { calls[name, default: []].append(ms) }
                    lock.unlock()
                }
                Thread.sleep(forTimeInterval: readEvery)
            }
        }
        core.name = "seedwell-qa-latency-core"
        core.qualityOfService = .userInitiated
        core.start()
    }

    /// Stops the run; its numbers stay for `report`.
    func stop() {
        lock.lock()
        if running { ended = CFAbsoluteTimeGetCurrent() }
        running = false
        if let a = activity { ProcessInfo.processInfo.endActivity(a) }
        activity = nil
        lock.unlock()
    }

    private func alive(_ me: Int) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return running && run == me
    }

    /// On the main thread: a beat posted at `sent` ran now.
    private func served(_ me: Int, sent: Double) {
        let now = CFAbsoluteTimeGetCurrent()
        let excused = Self.overlap(captures.spans(since: sent, now: now), from: sent, to: now)
        let action = UIAction.current
        let cpu = clock_gettime_nsec_np(CLOCK_THREAD_CPUTIME_ID)
        lock.lock()
        let ran = lastCpu == 0 ? 0 : Double(cpu &- lastCpu) / 1e6
        lastCpu = cpu
        // A beat posted before `stop` still counts when it runs after it (a stall that ends after the stop).
        if run == me, running || ended.map({ sent <= $0 }) == true {
            beats.append(Beat(at: sent - started, ms: (now - sent) * 1000, ownMs: max(0, now - sent - excused) * 1000, action: action,
                              ranMs: ran))
        }
        lock.unlock()
    }

    /// How much of `from…to` the spans cover (they never overlap: each capture runs on the main thread).
    static func overlap(_ spans: [(from: Double, to: Double)], from: Double, to: Double) -> Double {
        spans.reduce(0) { $0 + max(0, min(to, $1.to) - max(from, $1.from)) }
    }

    /// The run so far (or as stopped): see `Report`.
    func report() -> Report {
        lock.lock()
        defer { lock.unlock() }
        let span = (ended ?? CFAbsoluteTimeGetCurrent()) - started
        return Report(beats: beats, calls: calls, span: running || ended != nil ? span : 0, running: running)
    }

    /// What a run came to.
    struct Report {
        let beats: [Beat]
        let calls: [String: [Double]]
        let span: Double
        let running: Bool

        /// The `p` quantile (0…1) of `values`, nearest rank; 0 for none.
        static func quantile(_ values: [Double], _ p: Double) -> Double {
            guard !values.isEmpty else { return 0 }
            let sorted = values.sorted()
            let rank = Int((p * Double(sorted.count)).rounded(.up)) - 1
            return sorted[min(max(rank, 0), sorted.count - 1)]
        }

        /// The latest beat, counted without the renderer's captures.
        var worst: Beat? { beats.max { $0.ownMs < $1.ownMs } }
        /// The five latest, latest first.
        var worstFive: [Beat] { Array(beats.sorted { $0.ownMs > $1.ownMs }.prefix(5)) }
        var p99: Double { Self.quantile(beats.map(\.ownMs), 0.99) }
        func over(_ ms: Double) -> Int { beats.filter { $0.ownMs > ms }.count }
        /// The beats the renderer's captures held up, and the time they took out.
        var excusedBeats: Int { beats.filter { $0.ms - $0.ownMs > 0.5 }.count }
        var excusedMax: Double { beats.map { $0.ms - $0.ownMs }.max() ?? 0 }

        /// The lines `ui latency report` writes: a summary, the five worst beats, one line per core read, and the same as
        /// one JSON object for scripts.
        var lines: [String] {
            var out: [String] = []
            let f = { (v: Double) in String(format: "%.1f", v) }
            let w = worst
            out.append("latency: beats \(beats.count) in \(f(span)) s\(running ? " (running)" : ""), max \(f(w?.ownMs ?? 0)) ms at \"\(w?.action ?? "")\", p99 \(f(p99)) ms, over 50 ms \(over(50)), over 100 ms \(over(100)); renderer captures held \(excusedBeats) beats up to \(f(excusedMax)) ms (not counted)")
            for b in worstFive {
                out.append("latency-worst: +\(String(format: "%.2f", b.at)) s \(f(b.ownMs)) ms (raw \(f(b.ms)) ms, the main thread ran \(f(b.ranMs)) ms of it) \"\(b.action)\"")
            }
            for (action, worst, late) in byAction.prefix(8) {
                out.append("latency-action: \(f(worst)) ms max, \(late) beats over 50 ms \"\(action)\"")
            }
            for name in calls.keys.sorted() {
                let v = calls[name] ?? []
                out.append("latency-core: \(name) calls \(v.count), max \(f(v.max() ?? 0)) ms, p99 \(f(Self.quantile(v, 0.99))) ms")
            }
            out.append("latency-json: " + json)
            return out
        }

        /// Per UI action: its latest beat and its beats over 50 ms, latest first (where the time goes).
        var byAction: [(action: String, max: Double, over50: Int)] {
            Dictionary(grouping: beats, by: \.action)
                .map { ($0.key, $0.value.map(\.ownMs).max() ?? 0, $0.value.filter { $0.ownMs > 50 }.count) }
                .sorted { $0.1 > $1.1 }
        }

        var json: String {
            let w = worst
            let obj: [String: Any] = [
                "beats": beats.count,
                "span_s": span,
                "max_ms": w?.ownMs ?? 0,
                "max_action": w?.action ?? "",
                "max_at_s": w?.at ?? 0,
                "p99_ms": p99,
                "over50": over(50),
                "over100": over(100),
                "excused_beats": excusedBeats,
                "excused_max_ms": excusedMax,
                "worst": worstFive.map { ["at_s": $0.at, "ms": $0.ownMs, "raw_ms": $0.ms, "ran_ms": $0.ranMs, "action": $0.action] as [String: Any] },
                "max_ran_ms": w?.ranMs ?? 0,
                "actions": byAction.prefix(12).map { ["action": $0.action, "max_ms": $0.max, "over50": $0.over50] as [String: Any] },
                "core": calls.mapValues { v in ["calls": v.count, "max_ms": v.max() ?? 0, "p99_ms": Self.quantile(v, 0.99)] as [String: Any] },
            ]
            guard let data = try? JSONSerialization.data(withJSONObject: obj, options: [.sortedKeys]) else { return "{}" }
            return String(decoding: data, as: UTF8.self)
        }
    }
}
