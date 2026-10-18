import AppKit
import Foundation
import Network

/// Settles the OS path monitor's chatter (SPEC 7C.4c): a value is delivered once it has stood still
/// for `delay`. Delivered: a change of "usable path or not", and, while a path exists, a change of the
/// interfaces it runs over (Wi-Fi to Ethernet: the engine refreshes its port mapping). A launch that
/// starts online delivers nothing. Pure, with an injectable scheduler so it can be tested.
@MainActor final class PathDebounce {
    struct Path: Equatable { var satisfied: Bool; var interfaces: String }

    typealias Schedule = (TimeInterval, @escaping @MainActor () -> Void) -> () -> Void

    private let delay: TimeInterval
    private let schedule: Schedule
    private let deliver: (Bool) -> Void
    /// What the engine was told last (it starts out online).
    private var told = Path(satisfied: true, interfaces: "")
    private var knownInterfaces: String?
    private var cancel: (() -> Void)?

    init(delay: TimeInterval = 3, schedule: @escaping Schedule = PathDebounce.mainQueue, deliver: @escaping (Bool) -> Void) {
        self.delay = delay
        self.schedule = schedule
        self.deliver = deliver
    }

    static let mainQueue: Schedule = { delay, work in
        let item = DispatchWorkItem { MainActor.assumeIsolated { work() } }
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: item)
        return { item.cancel() }
    }

    func report(_ path: Path) {
        cancel?()
        cancel = nil
        // The first report only says what the interfaces are; it is not a change.
        if knownInterfaces == nil, path.satisfied { knownInterfaces = path.interfaces; told.interfaces = path.interfaces; return }
        let changed = path.satisfied != told.satisfied || (path.satisfied && path.interfaces != told.interfaces)
        guard changed else { return }
        cancel = schedule(delay) { [weak self] in
            guard let self else { return }
            cancel = nil
            told = path
            knownInterfaces = path.interfaces
            deliver(path.satisfied)
        }
    }
}

/// Tells the engine what the OS knows about the network, without polling: `NWPathMonitor` is event
/// driven, and the sleep and wake notifications arrive from the workspace. The engine decides what
/// they mean (hold, probe, release); nothing here sends a request.
@MainActor final class NetworkWatcher {
    private let core: Core
    private let monitor = NWPathMonitor()
    private var debounce: PathDebounce!
    private var observers: [NSObjectProtocol] = []

    init(core: Core) {
        self.core = core
        debounce = PathDebounce { [core] satisfied in core.networkPathChanged(satisfied: satisfied) }
        monitor.pathUpdateHandler = { [weak self] path in
            let ifaces = path.availableInterfaces.map(\.name).sorted().joined(separator: ",")
            let p = PathDebounce.Path(satisfied: path.status == .satisfied, interfaces: ifaces)
            DispatchQueue.main.async { MainActor.assumeIsolated { self?.debounce.report(p) } }
        }
        monitor.start(queue: DispatchQueue(label: "seedwell.net.path"))
        let ws = NSWorkspace.shared.notificationCenter
        observers.append(ws.addObserver(forName: NSWorkspace.willSleepNotification, object: nil, queue: .main) { [core] _ in
            core.systemWillSleep()
        })
        observers.append(ws.addObserver(forName: NSWorkspace.didWakeNotification, object: nil, queue: .main) { [core] _ in
            core.systemDidWake()
        })
    }

    /// QA: the path monitor said so (`net offline|online`), without waiting for the debounce.
    func inject(satisfied: Bool) { core.networkPathChanged(satisfied: satisfied) }

    isolated deinit {
        monitor.cancel()
        let ws = NSWorkspace.shared.notificationCenter
        for o in observers { ws.removeObserver(o) }
    }
}
